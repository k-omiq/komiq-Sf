-- Indices for the rewritten `updates` feed (graphql::updates).
--
-- WHY THIS MIGRATION EXISTS
--
-- The Updates feed used to order by `series_scan_state.last_new_chapter_at` — OUR
-- detection time — while the reader labels every card with the REAL upstream release
-- time (`suwayomi_series.latest_chapter_at`, migration 0050). So a chapter released
-- 184 days ago that our scanner happened to notice a minute ago sorted to the top of
-- the feed and rendered "184d", directly above a card reading "3h". The label column
-- was, measured against production, unordered noise.
--
-- The fix is to sort by the column the reader actually displays, which means the query
-- has to be driven from `suwayomi_series` (where that column lives) so it can ride
-- idx_suwayomi_series_latest_chapter (in_library, latest_chapter_at DESC, id DESC) and
-- read the page in order with NO temp B-tree. That inverts the shape of the query: it
-- now walks all 13.8k in-library index entries and applies the two membership tests
-- per walked row, where before it walked only the 1,316 rows that had a detection
-- timestamp. Both tests were random fetches into big tables, so without the three
-- indices below the deep pages of the feed got progressively slower with OFFSET —
-- which matters much more now than it did, because the reader is gaining real
-- page-number pagination with URL state, making page 64 directly linkable.
--
-- Measured on a copy of production (1.37 GB, 13,847 in-library series, 1,316 feed
-- members, 64 pages for an anonymous viewer). "cold" = `sync` then
-- posix_fadvise(POSIX_FADV_DONTNEED) over the whole db file + -wal, plus a fresh
-- connection so SQLite's own page cache is empty too; median of 3.
--
--                    before 0063          after 0063
--   page 1        119 ms / 1.32 ms     43.6 ms / 0.96 ms
--   page 32      1798 ms / 10.39 ms     393 ms / 4.83 ms
--   page 64      1906 ms / 38.75 ms     519 ms / 13.54 ms
--
-- For reference, the OLD (mis-sorted) query on the same file measures
-- 51 ms / 0.95 ms, 628 ms / 5.10 ms, 768 ms / 8.25 ms — so the corrected ordering
-- lands at parity on page 1, better cold at every depth, and within ~5 ms warm at
-- the deepest page. The opted-in (show_nsfw = 1) branch improves far more, from
-- 565 ms to 55 ms cold at the last page, because its NSFW probe short-circuits.
--
-- Total cost ~14.9 MiB of index (measured via dbstat).


-- 1. The "has our scanner ever detected a new chapter for this series" test.
--
-- This is the dominant cost: it runs once per WALKED row, i.e. 13,847 times per
-- request regardless of which page is asked for. The only usable index was the
-- `series_id` PRIMARY KEY autoindex, which answers the equality but NOT the
-- `last_new_chapter_at IS NOT NULL` predicate — so every probe followed through to a
-- random fetch of the `series_scan_state` table row. Isolated measurement at
-- OFFSET 1260: 846 ms cold / 12.64 ms warm for that test alone.
--
-- A PARTIAL index carrying only the 1,316 rows that have a detection timestamp makes
-- the test index-only AND tiny — 0.02 MiB, which stays resident in the page cache
-- essentially forever. Same probe, same offset: 20 ms cold / 3.76 ms warm.
--
-- The resolver names this index with `INDEXED BY`, and it has to. Verified on the
-- production copy with ANALYZE run and with the two-column and non-partial variants
-- built alongside: SQLite's planner picks sqlite_autoindex_series_scan_state_1 every
-- time, because both candidates answer the equality and its cost model does not
-- credit the partial index's much smaller height. `INDEXED BY` is therefore load
-- bearing — do NOT drop this index without also editing graphql::updates, because the
-- query will fail outright ("no query solution") rather than quietly degrade.
CREATE INDEX IF NOT EXISTS idx_scan_state_detected_series
    ON series_scan_state(series_id)
    WHERE last_new_chapter_at IS NOT NULL;

-- 2. + 3. The NSFW gate (`series_cache::NSFW_GATE_SQL` and the copy in
-- graphql::updates): NOT EXISTS over source_series JOIN work. It only runs for rows
-- that passed test 1 (~1,316 per request, SQLite short-circuits the AND), but each
-- one was two random fetches into a 123k-row and a 121k-row table.
--
-- idx_source_series_type_key is (source_type, source_key) and stops there, so the
-- probe had to fetch the source_series row for `work_id`; and `work` was reached
-- through its TEXT PRIMARY KEY autoindex, which then had to fetch the work row for
-- the two nsfw columns. Appending exactly the columns each probe reads turns both
-- into COVERING index reads — the EXPLAIN QUERY PLAN goes from
--   SEARCH ss USING INDEX idx_source_series_type_key
--   SEARCH w  USING INDEX sqlite_autoindex_work_1
-- to
--   SEARCH ss USING COVERING INDEX idx_source_series_suwayomi_nsfw
--   SEARCH w  USING COVERING INDEX idx_work_nsfw_flags
-- and the gate alone at OFFSET 1260 drops to 578 ms cold / 5.66 ms warm.
--
-- These help EVERY caller of the NSFW gate, not just this feed: `library`,
-- `recently_added` and `search_catalogue` in series_cache.rs run the same predicate.
--
-- idx_source_series_suwayomi_nsfw is a strict superset of idx_source_series_type_key,
-- which is deliberately NOT dropped here: 0058 documents the hazard of removing an
-- index the planner still wants as a skip-scan fallback (dropping
-- idx_chapter_source_series measurably regressed two unrelated queries). Reclaiming
-- its 8 MiB is a separate, separately-measured change.
CREATE INDEX IF NOT EXISTS idx_source_series_suwayomi_nsfw
    ON source_series(source_type, source_key, work_id);

CREATE INDEX IF NOT EXISTS idx_work_nsfw_flags
    ON work(id, is_nsfw_override, is_nsfw);


-- Refresh planner statistics for the tables this migration's plans depend on.
-- Without this, sqlite_stat1 has no row for the three new indices and the planner keeps
-- choosing the old non-covering ones (verified: the covering plan above only appears
-- after ANALYZE). `suwayomi_series` gains no index here but is the DRIVING table of the
-- rewritten query, and its stats decide whether the driving scan rides
-- idx_suwayomi_series_latest_chapter at all — so it is refreshed alongside the three
-- whose index sets changed. Per-table ANALYZE, mirroring 0058's PART 3, so this does not
-- rewrite statistics for the 800k-row chapter table. Measured at ~4 s total on the
-- production copy, index builds included.
ANALYZE series_scan_state;
ANALYZE source_series;
ANALYZE work;
ANALYZE suwayomi_series;
