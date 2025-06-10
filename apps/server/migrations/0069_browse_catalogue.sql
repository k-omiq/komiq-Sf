-- Browse over the WHOLE canonical catalogue: one materialized row per browsable work,
-- including the 67,000 works that have no chapter we can date.
--
-- WHY THIS TABLE EXISTS, AND WHY IT IS NOT MORE COLUMNS ON `feed_series_updates`
--
-- Browse (migrations 0064 + 0068) pages `feed_series_updates`, which is 48,567 of the
-- 115,567 browsable works. That table's contract is explicit and load-bearing: "a work
-- with no dated chapter is not an update, so it is EXCLUDED from the feed rather than
-- sorted last" (0064, on `released_at NOT NULL`). The Updates page, `graphql::updates_feed`
-- and several tests all depend on it, so widening it with NULL-`released_at` rows would
-- corrupt the updates feed to fix Browse. Hence a second, wider materialization.
--
-- The 67,000 excluded works are not the obscure tail. MangaDex REMOVES chapters when a
-- series is licensed or claimed, so "no chapters upstream" correlates with POPULAR:
-- `Boku no Hero Academia` (58 aliases), `Nausicaä of the Valley of the Wind`,
-- `Koukaku Kidoutai: THE GHOST IN THE SHELL`, `Houseki no Kuni`, `Chi's Sweet Home` are
-- all in that set. 3,239 of them already carry a Suwayomi source mapping, i.e. chapters
-- for them will arrive from a source that is not MangaDex. Hiding them hid exactly what
-- people search for.
--
-- SCOPE, measured on production 2026-07-27: `work` = 115,569; works with no
-- `source_series` at all = 2 (EXCLUDED — there is no source to open, and `canonicalSeries`
-- + the Suwayomi path both need one); works with an empty title = 0. So 115,567 rows here,
-- of which 86,603 are visible to an anonymous viewer and 28,964 are effective-NSFW.
-- Merge-retired works are NOT a concern: a merge DELETEs the losing `work` row (0 survive
-- in production), and the `work_id` foreign key below cascades, so a merge cannot leave a
-- duplicate card behind.
--
-- PURE CACHE, same contract as 0064/0068: every column is derived from `work` /
-- `source_series` / `chapter` / `suwayomi_series` / `feed_series_updates`, and the table is
-- rewritten by `catalog::refresh_browse_catalogue` at boot and once per catalogue-sync
-- cycle. The backfill at the bottom of this file exists so Browse is correct on the very
-- FIRST request after startup: that refresh is spawned ~20 s into boot (see `main`), and
-- without a backfill Browse would serve an empty catalogue until it landed.
--
-- WHERE THE VALUES COME FROM. For a work that HAS a `feed_series_updates` row, every
-- overlapping column is COPIED VERBATIM from it — including its NULLs — rather than
-- re-derived. Two reasons: the derivations are already written and reviewed there (the
-- effective-NSFW COALESCE, the epoch-millis clock, the mirror-wins merge of the two
-- halves), and a Browse card must not disagree with the same work's Updates card. Verified
-- on a copy of production: 0 of the 48,567 shared rows differ on any copied column. For
-- the other 67,000 the same expressions are applied to `work` directly.
CREATE TABLE IF NOT EXISTS browse_catalogue (
    work_id            TEXT PRIMARY KEY REFERENCES work(id) ON DELETE CASCADE,

    -- The id the card NAVIGATES to, which is not always `work_id` — identical rule to
    -- `feed_series_updates.reader_id`, because getting it wrong produces a card that 404s:
    -- `graphql::canonical_series` rejects a work with no MangaDex anchor outright
    -- (`if work.mangadex_id.is_none() { return Err("No such work") }`).
    --
    -- A row copied from the feed keeps the feed's id verbatim (that also preserves the 293
    -- rows whose work IS MangaDex-anchored but whose feed row came from the scanner half
    -- and therefore navigates to the Suwayomi page). A row we derive here uses the anchor
    -- test itself: a `w_…` when the work has a `source_series` of type `mangadex` — which
    -- is exactly what `catalog::load_canonical_work` reads `mangadex_id` from — and
    -- otherwise the numeric Suwayomi series id. All 1,143 chapterless works with no
    -- MangaDex anchor have a Suwayomi source, so none of them is left without a
    -- destination.
    reader_id          TEXT NOT NULL,

    title              TEXT NOT NULL,

    -- Ready-to-use, origin-relative cover path (`/covers/{work_id}.webp[?v=N]`), exactly as
    -- `cover::work_cover_url` builds it, and the RAW Suwayomi thumbnail as the fallback for
    -- a work with no cached blob. The thumbnail stays raw because absolutizing it needs
    -- `SuwayomiClient::image_base_url`, which is RUNTIME CONFIG rather than data — baking a
    -- host into a cache table would outlive a config change and serve dead image URLs. Two
    -- columns rather than one overloaded one; the resolver picks. Same as 0064.
    cover_url          TEXT,
    suwayomi_thumbnail TEXT,

    -- The effective format, COLLAPSED to the reader's three words ('MANGA' | 'MANHWA' |
    -- 'MANHUA') the way `toViewType` collapses them, so the format filter is one indexed
    -- equality. Cannot be derived in SQL: `graphql::types::resolve_comic_type` is a
    -- five-step derivation (override -> original_language -> genre tags -> Unicode script
    -- of the title -> Manga). `catalog::fill_comic_types` runs the real function over this
    -- table inside the rebuild's transaction, which is what keeps Browse's format facet
    -- identical to every other surface's format badge.
    --
    -- The backfill below can therefore only seed the 48,567 rows the feed has already had
    -- typed by that same function. The other 67,000 stay NULL — and a NULL is invisible to
    -- the format filter — until the first refresh (~20 s into boot) types them. That is the
    -- same known gap 0068 and `scanner::upsert_feed_series_update` document, and it is
    -- narrow: it costs the format TABS 67k works for 20 s, while the alternative (a SQL
    -- approximation of `resolve_comic_type`) would disagree with the badges permanently.
    comic_type         TEXT,

    -- Publication status, NORMALIZED to the five words `SeriesStatus` actually has, by the
    -- same `catalog::FSU_STATUS_SQL` fold 0068 uses (`ON_HIATUS` -> HIATUS,
    -- `PUBLISHING_FINISHED`/`LICENSED` -> COMPLETED). Un-normalized, the 51 `ON_HIATUS` +
    -- 150 `PUBLISHING_FINISHED` works would be unreachable by EVERY value a client can send
    -- for `status:`, because `graphql::types::komika_status` returns `None` for the long
    -- forms. NULLABLE only to avoid an ALTER-with-default rewrite equivalent; the expression
    -- always produces a word (`COALESCE(w.status, 'UNKNOWN')` covers the 0 NULL-status
    -- works).
    status             TEXT,

    -- MangaDex's four-level rating with its NULL collapsed to the SAFEST tier. `NOT NULL
    -- DEFAULT` rather than a nullable copy for the reason 0068 measured: `WHERE
    -- COALESCE(content_rating, 'safe') IN (…)` is an expression, not a column, so the
    -- planner can use it neither as a seek key nor as index payload — which is what keeps
    -- the filtered COUNT index-only (0068 measured 4.38 ms vs 24.31 ms on the feed).
    -- 1,377 works have no rating upstream; defaulting them to 'safe' includes them in the
    -- narrowest filter rather than excluding them from every one.
    content_rating     TEXT NOT NULL DEFAULT 'safe',

    -- The EFFECTIVE flag `COALESCE(is_nsfw_override, is_nsfw)`, never the raw column: both
    -- admin "mark NSFW"/"mark SFW" mutations write ONLY the override, so a raw copy would
    -- make Browse the one surface that ignores every manual reclassification.
    is_nsfw            INTEGER NOT NULL DEFAULT 0,

    -- The English chapter count: `COUNT(DISTINCT chapter.number)` over the work's English
    -- MangaDex mirror, falling back to the Suwayomi source count when that counts nothing.
    -- Browse's CHAPTERS sort key, the number its cards print, AND the `hasChapters` filter.
    --
    -- `NOT NULL DEFAULT 0` for the same indexability reason as `content_rating`, and because
    -- 0 sorts last under DESC, which is where a work with no countable chapter belongs.
    --
    -- 0 IS NOT "not in the updates feed", in either direction, and both directions are
    -- measured: 11,054 works that ARE in the feed count 0 here (their mirrored chapters
    -- carry a NULL `number` — oneshots), and 1,505 works that are NOT in the feed count more
    -- than 0 (chapters exist but none of them is datable, or they are Suwayomi-only and
    -- unscanned). So `hasChapters: true` is "we know of a chapter", which is the honest
    -- reading of what a reader wants from that filter — it is NOT a way to recover the
    -- pre-0069 Browse row set exactly.
    --
    -- KNOWN DIVERGENCE, inherited from 0068: the series DETAIL page shows
    -- `catalog::main_chapter_count`, which groups by `floor(number)` so a split-numbered
    -- series (…9.1, 9.2, 10.1…) counts chapters rather than parts. `COUNT(DISTINCT number)`
    -- cannot do that in SQL without a per-row Rust pass over 805k chapter rows. Browse's
    -- badge is fed from THIS column — the same expression its CHAPTERS sort orders by —
    -- because "sorted by chapters" visibly disagreeing with the number on the card is worse
    -- than disagreeing with a different page.
    en_chapter_count   INTEGER NOT NULL DEFAULT 0,

    -- The newest real upstream chapter RELEASE time across the work's MangaDex mirror and
    -- its Suwayomi sources, in EPOCH MILLISECONDS. Copied from `feed_series_updates`, so it
    -- is the same clock the Updates card labels with, normalized there for the reason 0064
    -- gives (the two source columns are ISO-8601 TEXT and 13-digit millis TEXT, and under
    -- BINARY collation every '2…' sorts above every '1…').
    --
    -- **NULLABLE, unlike the feed's**, and that is the entire point of this table: 67,000
    -- browsable works have no dated chapter. They are SORTED LAST on the recently-updated
    -- ordering rather than excluded. SQLite treats NULL as smaller than every value, so a
    -- plain `released_at DESC` already puts them last — VERIFIED on a 115,567-row copy of
    -- production rather than assumed: the first row of `ORDER BY released_at DESC, work_id
    -- DESC` is non-NULL, the last is NULL, the 30-row window straddling row 48,567 is
    -- exactly 15 dated then 15 NULL, and the plan at that boundary is still
    -- `SEARCH … USING COVERING INDEX idx_bc_order` with no temp B-tree.
    --
    -- It is also the marker for "this work has no feed row", which the rebuild relies on:
    -- `feed_series_updates.released_at` is NOT NULL, so `browse_catalogue.released_at IS
    -- NULL` is exactly the set the feed excludes.
    released_at        INTEGER,

    -- When the WORK entered our catalogue (ISO-8601, `work.created_at`). Not a Browse sort
    -- key today; it is what `Series.createdAt` is resolved from, and having it here is what
    -- lets `map_browse_rows` drop it from its per-page `work` lookup — and what gives a
    -- chapterless work an honest `updatedAt` (that field is `String!`, and the only other
    -- candidate, `released_at`, is NULL for exactly those works).
    created_at         TEXT NOT NULL
);

-- THE INDICES ARE AT THE BOTTOM OF THIS FILE, after the backfill, unlike 0064/0068 which
-- had no rows to insert. Maintaining eight indices across 115,567 random-keyed inserts
-- measured 12,877 ms; the same INSERT into an unindexed table plus building the eight
-- indices afterwards measured 6,346 + 1,484 ms. Same end state, 5.9 s cheaper, and this file
-- is applied top-to-bottom in one transaction so nothing observes the gap.

-- BACKFILL. Mirrors `catalog::browse_catalogue_select()` statement-for-statement, kept in sync
-- BY HAND the same way 0068's backfill mirrors `refresh_feed_series_updates` and 0050's
-- mirrors `series_cache::derive_latest_chapter_at`. The rebuild is authoritative and
-- overwrites all of this on its first pass; this exists only so the ~20 s between the first
-- request and that pass serves a correct catalogue instead of an empty one.
INSERT INTO browse_catalogue
    (work_id, reader_id, title, cover_url, suwayomi_thumbnail, status,
     content_rating, is_nsfw, en_chapter_count, released_at, created_at)
SELECT w.id,
       -- Copied from the feed row where there is one; otherwise the anchor test decides.
       CASE WHEN f.work_id IS NOT NULL THEN f.reader_id
            WHEN md.work_id IS NOT NULL THEN w.id
            ELSE sw.source_key END,
       CASE WHEN f.work_id IS NOT NULL THEN f.title
            ELSE COALESCE(w.title_override, w.primary_title, sw.title) END,
       CASE WHEN f.work_id IS NOT NULL THEN f.cover_url
            WHEN w.cover_cached_version IS NOT NULL
                 THEN '/covers/' || w.id || '.webp?v=' || w.cover_cached_version
            WHEN w.cover_file_name IS NOT NULL THEN '/covers/' || w.id || '.webp' END,
       CASE WHEN f.work_id IS NOT NULL THEN f.suwayomi_thumbnail
            ELSE sw.thumbnail_url END,
       CASE WHEN f.work_id IS NOT NULL THEN f.status
            ELSE CASE w.status
                     WHEN 'ON_HIATUS'           THEN 'HIATUS'
                     WHEN 'PUBLISHING_FINISHED' THEN 'COMPLETED'
                     WHEN 'LICENSED'            THEN 'COMPLETED'
                     ELSE COALESCE(w.status, 'UNKNOWN')
                 END END,
       CASE WHEN f.work_id IS NOT NULL THEN f.content_rating
            ELSE COALESCE(w.content_rating, 'safe') END,
       CASE WHEN f.work_id IS NOT NULL THEN f.is_nsfw
            ELSE COALESCE(w.is_nsfw_override, w.is_nsfw) END,
       -- The English mirror count, else the Suwayomi count, else 0 — the same precedence
       -- 0068's two UPDATEs apply to the feed, folded into one expression because COALESCE
       -- short-circuits and the fallback may only ever RAISE a zero. `NULLIF(…, 0)` is what
       -- makes "counted, but every chapter is unnumbered" fall through to the Suwayomi
       -- count, exactly as 0068's `AND en_chapter_count = 0` guard does. Verified: 0 of the
       -- 48,567 shared rows disagree with the feed's value.
       COALESCE(NULLIF(en.n, 0), NULLIF(swc.n, 0), 0),
       f.released_at,
       w.created_at
  FROM work w
  LEFT JOIN feed_series_updates f ON f.work_id = w.id
  -- The MangaDex anchor test, as a grouped semi-join rather than a correlated EXISTS: it is
  -- evaluated for all 115,569 works and again in the WHERE, and `source_series` holds
  -- 113,801 mangadex rows.
  LEFT JOIN (SELECT work_id FROM source_series
              WHERE source_type = 'mangadex' GROUP BY work_id) md ON md.work_id = w.id
  -- The work's Suwayomi source, picked DETERMINISTICALLY as the lowest-id row via
  -- SQLite's documented "bare columns with exactly one min/max aggregate" rule — every bare
  -- column comes from the row that produced `MIN(ss.id)`, so the id, title and thumbnail all
  -- describe ONE source rather than being smeared across several. (The feed half picks the
  -- newest-RELEASING source instead; there is no release time to rank by here.) LEFT JOIN to
  -- `suwayomi_series` because a `source_series` row can name a series we have not cached.
  LEFT JOIN (SELECT ss.work_id AS work_id, ss.source_key AS source_key,
                    sy.title AS title, sy.thumbnail_url AS thumbnail_url, MIN(ss.id)
               FROM source_series ss
               LEFT JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER)
              WHERE ss.source_type = 'suwayomi'
              GROUP BY ss.work_id) sw ON sw.work_id = w.id
  -- MAX over ALL the work's Suwayomi sources, which is a different aggregate from the row
  -- `sw` picks and therefore cannot share its GROUP BY without breaking the bare-column
  -- rule. This is the fallback 0068's second UPDATE applies, verbatim.
  LEFT JOIN (SELECT ss.work_id AS work_id, MAX(sy.chapter_count) AS n
               FROM source_series ss
               JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER)
              WHERE ss.source_type = 'suwayomi'
              GROUP BY ss.work_id) swc ON swc.work_id = w.id
  -- Grouped ONCE and joined, not a correlated `COUNT(DISTINCT …)` per work: the correlated
  -- form measured 5,668 ms over the 67,000 uncounted works alone, this one 3,317 ms over all
  -- 805,425 chapters (and it is the same statement 0068 uses, so the two tables cannot drift).
  LEFT JOIN (SELECT ss.work_id AS work_id, COUNT(DISTINCT c.number) AS n
               FROM chapter c
               JOIN source_series ss ON ss.id = c.source_series_id
              WHERE ss.source_type = 'mangadex' AND c.lang = 'en'
              GROUP BY ss.work_id) en ON en.work_id = w.id
 -- The two exclusions. No source means nothing to open on either path (2 works). An empty
 -- title means a card with no label and no href (`MangaCard` falls back to `slug(title)`);
 -- 0 works today, and the guard is also what makes `title NOT NULL` above safe.
 WHERE (md.work_id IS NOT NULL OR sw.work_id IS NOT NULL)
   AND COALESCE(f.title, w.title_override, w.primary_title, sw.title, '') <> '';

-- `comic_type` for the rows the feed has already had typed by the real
-- `resolve_comic_type` (48,567 of 115,567). The remaining 67,000 stay NULL until the first
-- `catalog::refresh_browse_catalogue` types them — see the column comment.
UPDATE browse_catalogue SET comic_type =
    (SELECT f.comic_type FROM feed_series_updates f WHERE f.work_id = browse_catalogue.work_id)
  WHERE EXISTS (SELECT 1 FROM feed_series_updates f WHERE f.work_id = browse_catalogue.work_id);

-- EIGHT indices: {NSFW hidden, NSFW shown} x {all formats, one format} for EACH of Browse's
-- two orderings — `released_at DESC` (the NEWEST sort, and the second phase of the RATING
-- and TRENDING sorts, which page their unranked remainder on that key) and
-- `en_chapter_count DESC` (the CHAPTERS sort). Exactly the 2x2 shape 0064 and 0068
-- established, for the same reason.
--
-- The resolver BRANCHES its WHERE text on both dimensions instead of parameterizing them:
-- `WHERE (? = 1 OR is_nsfw = 0)` is opaque to the planner, which then cannot tell `is_nsfw`
-- is constant and sorts the whole table through a temp B-tree to satisfy the ORDER BY.
-- Branched, `is_nsfw` and `comic_type` are index PREFIXES and the remainder of the index is
-- already in ORDER BY order. The two `is_nsfw`-leading indices serve the anonymous branch
-- (`WHERE is_nsfw = 0`, the hot edge-cacheable path); the two without it serve the opted-in
-- branch, which has no `is_nsfw` predicate at all and would otherwise sort all 115k rows.
--
-- `status`, `content_rating` and (on the `released_at` four) `en_chapter_count` are appended
-- as PAYLOAD, never as seek keys — they sit BELOW the sort column and hoisting them above it
-- would destroy the ordering the index exists to provide. As payload they keep the filtered
-- COUNT index-only, which is what `total` and the pager are computed from. `en_chapter_count`
-- is payload on the `released_at` four specifically so the new `hasChapters` filter does not
-- cost a table lookup per counted row; on the other four it is the sort key already.
--
-- `work_id DESC` is the mandatory tiebreaker on every one of them. Not cosmetic: 67,000 rows
-- share `released_at IS NULL` and ~76k share an `en_chapter_count` in the 0..5 range, so
-- without a total order LIMIT/OFFSET repeats or skips rows at a page boundary — and the
-- reader keys its grid `{#each}` on the row id, where a duplicate is an
-- `each_key_duplicate` throw in PRODUCTION rather than a cosmetic dupe.
--
-- VERIFIED on a copy of production (115,567 rows, `ANALYZE`d): all 13 shapes Browse can
-- emit — every NSFW x format x status x rating x hasChapters combination, on both orderings,
-- at offset 0 and at offsets up to 115,000 — plan as SEARCH/SCAN over one of these indices
-- with ZERO `USE TEMP B-TREE FOR ORDER BY`, 0.3-8.3 ms per page, and their COUNTs are
-- index-only at 0.5-10.9 ms.
CREATE INDEX IF NOT EXISTS idx_bc_order
    ON browse_catalogue(is_nsfw, released_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);
CREATE INDEX IF NOT EXISTS idx_bc_type_order
    ON browse_catalogue(is_nsfw, comic_type, released_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);
CREATE INDEX IF NOT EXISTS idx_bc_all_order
    ON browse_catalogue(released_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);
CREATE INDEX IF NOT EXISTS idx_bc_type_all_order
    ON browse_catalogue(comic_type, released_at DESC, work_id DESC,
                        status, content_rating, en_chapter_count);
CREATE INDEX IF NOT EXISTS idx_bc_chapters
    ON browse_catalogue(is_nsfw, en_chapter_count DESC, work_id DESC,
                        status, content_rating);
CREATE INDEX IF NOT EXISTS idx_bc_type_chapters
    ON browse_catalogue(is_nsfw, comic_type, en_chapter_count DESC, work_id DESC,
                        status, content_rating);
CREATE INDEX IF NOT EXISTS idx_bc_all_chapters
    ON browse_catalogue(en_chapter_count DESC, work_id DESC, status, content_rating);
CREATE INDEX IF NOT EXISTS idx_bc_type_all_chapters
    ON browse_catalogue(comic_type, en_chapter_count DESC, work_id DESC,
                        status, content_rating);

-- Give the planner statistics for the table it has the most choices about, NOW rather
-- than in five minutes.
--
-- The 13 query plans this migration's header reports were measured on an ANALYZEd copy.
-- Without `sqlite_stat1` rows SQLite assumes ~1M rows for a table and can pick the wrong
-- one of the eight indices above for a given filter combination — reintroducing the
-- full-table temp-B-tree sort they exist to prevent. `db::ANALYZE_TABLES` now includes
-- this table, but that pass is deliberately offset 300s past boot to stay clear of the
-- boot write burst, so without this statement the very first Browse requests after a
-- deploy — the ones a cache-cold edge sends straight through — would plan blind.
--
-- Costs a single-table scan on a table that was just written in this same transaction,
-- so its pages are already hot.
ANALYZE browse_catalogue;
