-- Index cleanup + the two feed/enrichment indexes that were missing, then a
-- targeted statistics refresh.
--
-- Three separate problems, one migration, because all three are pure index/stat
-- changes that touch no row of user data and are individually idempotent.
--
-- MEASURED RUNTIME (re-measured 2026-07-26 on a byte-for-byte copy of the 1.37 GB
-- production DB, production pragmas, page cache explicitly evicted first, all
-- statements inside one transaction as sqlx runs them): 18.3 s cold, 0.12 s on a
-- second run. Idempotency confirmed end-to-end — re-running drops nothing further
-- (all 7 target indexes already absent), creates nothing further (both new indexes
-- present), and simply recomputes the same stat rows. Migrations run before the
-- listener binds, so this is 18.3 s of one-time startup delay on the first boot
-- after deploy; 0059 adds ~5 s on top, for ~23 s total.
--
--
-- PART 1 — drop seven indexes that are a strict PREFIX of another index.
--
-- SQLite can serve any lookup from a wider index that shares the same leading
-- columns, so a narrow prefix index can never win a plan its wider sibling cannot
-- also serve. It still costs a B-tree insert on every write to the table and ~100 MB
-- of the 1.37 GB file (7.3%), and — because the planner picks the SMALLEST index when
-- it needs a full scan — it quietly becomes the scan target for whole-table
-- aggregates. Each drop below names the index that supersedes it:
--
--   idx_chapter_source_series(source_series_id)      805k rows, 39.58 MB
--     -> idx_chapter_ss_lang_pubdate(source_series_id, lang, published_at, created_at)
--     -> sqlite_autoindex_chapter_2, i.e. UNIQUE (source_series_id, external_id)
--   idx_work_alias_token_token(token)              1,254k rows, 22.87 MB
--     -> idx_work_alias_token_token_work(token, work_id)
--   idx_work_alias_work(work_id)                     425k rows, 19.95 MB
--     -> sqlite_autoindex_work_alias_2, i.e. UNIQUE (work_id, normalized_title, lang)
--   idx_work_credit_work(work_id)                    276k rows, 12.80 MB
--     -> sqlite_autoindex_work_credit_1, i.e. PRIMARY KEY (work_id, role, name)
--   idx_work_cover_work(work_id)                     109k rows,  4.98 MB
--     -> sqlite_autoindex_work_cover_1, i.e. PRIMARY KEY (work_id, cover_file_name)
--   idx_suwayomi_series_library(in_library)           13.8k rows, 0.13 MB
--     -> idx_suwayomi_series_latest_chapter(in_library, latest_chapter_at DESC, id DESC),
--        added by 0050; the old single-column index was never dropped and is now dead —
--        no query in the tree still plans against it.
--   idx_comment_media_comment(comment_id)              0 rows, 0.004 MB
--     -> idx_comment_media_gc(comment_id, created_at)
--
-- Verified against a copy of the production DB: every query in apps/server/src that
-- touches these tables keeps an identical or better EXPLAIN QUERY PLAN, and no plan
-- degrades from a SEARCH to a SCAN. The one measured cost is `refresh_work_fts`'s
-- grouped chapter count (catalog::refresh_work_fts), whose covering scan moves from
-- the 39.6 MB idx_chapter_source_series to the 73.8 MB UNIQUE autoindex: ~10.3s ->
-- ~13.0s cold, +6% warm. That is a boot/post-sync background rebuild, not a request
-- path, and it is paid back by removing an index write from all 805k chapter rows.
--
-- The freed pages go on the freelist and are reused by subsequent inserts; the file
-- does not shrink without a VACUUM, which is deliberately NOT run here (it would
-- rewrite the whole 1.37 GB and rewrite every Litestream page).
DROP INDEX IF EXISTS idx_chapter_source_series;
DROP INDEX IF EXISTS idx_work_alias_token_token;
DROP INDEX IF EXISTS idx_work_alias_work;
DROP INDEX IF EXISTS idx_work_credit_work;
DROP INDEX IF EXISTS idx_work_cover_work;
DROP INDEX IF EXISTS idx_suwayomi_series_library;
DROP INDEX IF EXISTS idx_comment_media_comment;


-- PART 2 — two indexes that were missing.
--
-- `canonicalUpdates` is BRANCHED on the NSFW preference (graphql::canonical_updates):
-- the anonymous branch pins `is_nsfw = 0` and rides idx_feed_updates_order, but the
-- opted-in branch has no WHERE clause at all, so its leading `is_nsfw` column is
-- useless and the planner falls back to SCAN feed_updates + USE TEMP B-TREE FOR ORDER
-- BY — sorting all 47.5k rows to return a 25-row page, on every /updates view by every
-- NSFW-opted-in user. This index is that same ordering without the is_nsfw prefix, so
-- the opted-in branch reads 25 index entries in order and stops. Measured on a copy of
-- production, page 1: 1105 ms cold / 7.40 ms warm -> 13.0 ms cold / 0.06 ms warm, which
-- finally puts the opted-in branch level with the anonymous one (0.06 ms). Deeper pages
-- benefit identically (OFFSET 100: 8.86 ms -> 0.07 ms warm). Costs 3.2 MiB.
CREATE INDEX IF NOT EXISTS idx_feed_updates_latest
    ON feed_updates (latest_at DESC, work_id DESC);

-- `works_needing_enrichment` (the X1 metadata-backfill drain) filters
-- source_type = 'mangadex' and orders by ss.created_at. idx_source_series_type_key
-- leads with source_type but continues on source_key, so created_at was never
-- index-ordered: the planner materialized all ~109k matching rows through a temp
-- B-TREE to return LIMIT 25, on a task that runs on a timer. With created_at as the
-- second column the scan is already in ORDER BY order and LIMIT short-circuits — the
-- temp B-tree disappears from the plan entirely. Measured on a copy of production:
-- 17.4 s cold / 1056 ms warm -> 20 ms cold / 0.04 ms warm. Costs 6.2 MiB.
--
-- CORRECTION (verified 2026-07-26): an earlier draft of this header claimed the index
-- also sped up `chapter_owner_is_nsfw` (the canonicalPages NSFW gate) from 351 ms to
-- 0.72 ms. That does NOT reproduce and the claim was wrong. It was measured with
-- chapter keys taken in index order, which flatters the pre-existing plan by ~4000x.
-- Re-measured against 25 RANDOMLY SAMPLED real chapter uuids, this migration makes
-- that query slightly WORSE (169.65 ms -> 178.83 ms warm), because dropping
-- idx_chapter_source_series in PART 1 costs the planner its skip-scan option.
-- `known_chapter_id` degrades further still (59.81 ms -> 122.93 ms on a MISS).
--
-- The real fix for those is migration 0062, which indexes `chapter.external_id`
-- directly and takes them to 0.052 ms / 0.004 ms. 0058 and 0062 should therefore be
-- deployed together; 0058 alone is a regression for the chapter-open request path.
-- Always sample keys randomly when benchmarking these — index-ordered keys hide it.
CREATE INDEX IF NOT EXISTS idx_source_series_type_created
    ON source_series (source_type, created_at);


-- PART 3 — refresh query-planner statistics for the tables that have none.
--
-- sqlite_stat1 in production was last written when 48 migrations existed (its
-- _sqlx_migrations row says so), i.e. BEFORE 0049-0055, and nothing had refreshed
-- it since. (`db::spawn_analyze` now keeps six of these tables current on a 6 h
-- timer, but it ships in the same unlanded batch as this migration and only
-- covers feed_updates, source_series, suwayomi_series, series_scan_state,
-- merge_candidate and notifications — the work_fts shadow tables, comment_votes,
-- chapter_override, canonical_library, sync_state and maintenance_flag below are
-- analyzed HERE and nowhere else, so this part is not redundant with it.)
-- Every table those migrations added — feed_updates (0051),
-- work_fts (0052), sync_state (0047), maintenance_flag (0055) — plus notifications,
-- comment_votes, chapter_override and canonical_library have NO stat row at all, and
-- SQLite falls back to assuming roughly a million rows for them. That default is what
-- flips plans on exactly the new tables, and it is why the two indexes above need
-- stats of their own to be chosen at all.
--
-- This is a per-TABLE ANALYZE, not a bare `ANALYZE`, because migrations run inside
-- db::init before the listener binds — every second here is startup downtime. A full
-- ANALYZE of this database measures 95 s cold; the tables below measure ~4.5 s cold
-- together, and they are precisely the ones with missing or known-stale stats. The
-- large, already-analyzed tables (chapter 26 s, work_alias_token 20 s, work_alias 12 s)
-- keep their existing rows and are deliberately not re-scanned here.
--
-- `PRAGMA analysis_limit` was rejected: it caps the sampled rows per index, which pins
-- the "average rows per key" estimate at the limit — it reported source_type as
-- selecting 401 of 123k source_series rows instead of the real 61.5k, and is_nsfw as
-- 401 of 47.5k feed_updates rows. Those are the exact selectivities the two new
-- indexes are chosen on, so sampling would defeat the point.
--
-- ANALYZE only rewrites the sqlite_stat1 rows for the named table, so re-running this
-- migration is safe and simply recomputes them. Every stat row this produces was
-- checked against a full exact ANALYZE of the same DB and matches it byte for byte.
--
-- One knock-on worth knowing about: with source_series' real cardinality in hand, the
-- planner re-plans `catalog::refresh_feed_updates` from 109k index probes into
-- idx_chapter_ss_lang_pubdate to a straight scan of `chapter` plus a temp B-tree GROUP
-- BY. That is the right call for the boot rebuild (41.9 s -> 18.7 s cold) and a small
-- loss for a post-sync rebuild with the pages already hot (2.9 s -> 4.3 s). Both are
-- background, neither is a request path.
ANALYZE feed_updates;
ANALYZE source_series;
ANALYZE suwayomi_series;
-- `ANALYZE work_fts` is a silent no-op: ANALYZE skips virtual tables, so the FTS5
-- index has to be reached through its shadow tables by name. Listed individually
-- rather than left to a bare `ANALYZE` for the downtime reason above.
ANALYZE work_fts_idx;
ANALYZE work_fts_data;
ANALYZE work_fts_docsize;
ANALYZE work_fts_content;
ANALYZE work_fts_config;
ANALYZE series_scan_state;
ANALYZE merge_candidate;
ANALYZE notifications;
ANALYZE comment_votes;
ANALYZE chapter_override;
ANALYZE canonical_library;
ANALYZE sync_state;
ANALYZE maintenance_flag;
