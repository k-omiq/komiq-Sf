-- Browse, repointed from the Suwayomi library subset to the canonical catalogue: the
-- filter/sort columns that move needs, plus a materialized genre facet.
--
-- WHY THIS MIGRATION EXISTS
--
-- Browse's empty-query path (`graphql::search` -> `series_cache::search_catalogue`) read
-- `suwayomi_series WHERE in_library = 1` — 13,847 rows of the 48,526 readable canonical
-- works, keyed by numeric Suwayomi id, with one fixed ORDER BY and no format/status/
-- rating filter possible at all. `feed_series_updates` (migration 0064) already
-- materializes exactly the readable set, so Browse moves onto it. What 0064 does NOT
-- carry is the three columns a browse UI filters and sorts on, and every one of them has
-- to be MATERIALIZED rather than joined at read time for the same reason 0064 gives for
-- `comic_type`: a predicate that is not a column cannot be an index prefix, and without
-- an index prefix the 48k-row ORDER BY degrades to a temp B-tree on every page load.
--
-- All three are pure cache, derivable from `work` / `chapter` / `suwayomi_series`, and are
-- rewritten wholesale by `catalog::refresh_feed_series_updates` — same contract as every
-- other column of this table. They are backfilled here anyway (bottom of this file) so
-- the columns are correct from the instant the migration lands, instead of leaving Browse
-- with an empty status filter and an all-zero chapter sort for the gap between migrate and
-- the boot refresh.

-- The work's publication status, NORMALIZED to the `SeriesStatus` enum's five words.
--
-- Normalization is not cosmetic. `graphql::types::komika_status` — the only parser for
-- this column's values — returns `None` for `ON_HIATUS` and `PUBLISHING_FINISHED`, and
-- the GraphQL enum has no such members, so an un-normalized column makes those rows
-- unreachable by ANY `status:` filter value the client can send: 7 `ON_HIATUS` + 1
-- `PUBLISHING_FINISHED` feed rows today (51 + 150 across all 115,531 works, so the number
-- grows as more of the catalogue becomes readable). Folding them at WRITE time makes the
-- filter one equality over the same vocabulary the enum has.
--
-- The fold is `status_from`'s, the codebase's existing mapping for these upstream words
-- (`PUBLISHING_FINISHED`/`LICENSED` -> Completed, `ON_HIATUS` -> Hiatus). `LICENSED` has 0
-- rows in `work` today, so which side it lands on is a vocabulary decision rather than a
-- data change — and it must be the same side `status_from` picks, or one upstream word
-- would mean two different things on two Komika surfaces.
--
-- NULLABLE, unlike the two below: a NULL here is "no work row", which the `work_id`
-- foreign key makes impossible, so the column exists as NOT-NULL-in-practice without
-- paying an ALTER-with-default rewrite of 48k rows. `COALESCE(w.status, 'UNKNOWN')` covers
-- the NULL-status case (0 rows live).
ALTER TABLE feed_series_updates ADD COLUMN status TEXT;

-- MangaDex's four-level content rating, materialized with its NULL collapsed.
--
-- `NOT NULL DEFAULT 'safe'` rather than a nullable copy of `work.content_rating`, because
-- 711 of the 48,526 feed rows have no rating upstream and `WHERE COALESCE(content_rating,
-- 'safe') IN (…)` is not indexable — the COALESCE makes the column an expression, so the
-- planner cannot use it as a seek key or as index payload, which is precisely what keeps
-- the filtered COUNT index-only (measured 4.38 ms vs 24.31 ms). Defaulting to the SAFEST
-- tier is also the right direction for an unrated work: it is included in the narrowest
-- filter rather than excluded from every one.
--
-- This column NARROWS within the viewer's NSFW gate; it never widens it. Verified on
-- production: no `is_nsfw = 0` row carries `erotica` or `pornographic`, so for an
-- opted-OUT viewer the EROTICA and PORNOGRAPHIC tiers return exactly the SUGGESTIVE set.
-- The converse is NOT clean — 27 rows are `is_nsfw = 1` with a NULL rating (so they
-- materialize as 'safe') and 1 is `is_nsfw = 1` + 'suggestive' — which only an opted-IN
-- viewer can ever see, and for whom the request really was "filter by rating".
ALTER TABLE feed_series_updates ADD COLUMN content_rating TEXT NOT NULL DEFAULT 'safe';

-- The English chapter count: `COUNT(DISTINCT chapter.number)` over the work's English
-- MangaDex mirror, falling back to the Suwayomi source count for a work with no mirrored
-- English chapter.
--
-- A SEPARATE COLUMN, NOT `chapter_count`. 0064's `chapter_count` is deliberately NULL on
-- every mirror-half row (47,514 of 48,526 rows are NULL today) because the Updates card
-- renders `Ch. {latest_chapter ?? chapter_count}` — filling it would silently relabel
-- every mirror card from "Ch. 1054" (the newest chapter's number) to a total. That column
-- is a LABEL; this one is a SORT KEY and a badge. Overloading them would couple the two
-- surfaces forever.
--
-- `NOT NULL DEFAULT 0` for the same indexability reason as `content_rating`, and because
-- 0 sorts last under DESC, which is where a work with no countable chapter belongs.
--
-- KNOWN DIVERGENCE, deliberate: the series DETAIL page shows
-- `catalog::main_chapter_count`, which groups by `floor(number)` so that a split-numbered
-- series (…9.1, 9.2, 10.1…) counts real chapters rather than parts. `COUNT(DISTINCT
-- number)` cannot do that in SQL without a per-row Rust pass over 805k chapter rows. The
-- badge Browse renders is fed from THIS column — the same expression the CHAPTERS sort
-- orders by — because "sorted by chapters" visibly disagreeing with the number printed on
-- the card is a worse failure than disagreeing with a different page.
ALTER TABLE feed_series_updates ADD COLUMN en_chapter_count INTEGER NOT NULL DEFAULT 0;

-- FOUR indices for the CHAPTERS sort, mirroring 0064's 2x2 shape exactly: {NSFW hidden,
-- NSFW shown} x {all formats, one format}. The resolver BRANCHES its WHERE string on both
-- dimensions rather than parameterizing them, for the reason 0064's header and
-- `graphql::updates_feed` both spell out — `WHERE (? = 1 OR is_nsfw = 0)` is opaque to the
-- planner, which then cannot tell `is_nsfw` is constant and sorts all 48k rows through a
-- temp B-tree to satisfy the ORDER BY. Branched, `is_nsfw` and `comic_type` are index
-- PREFIXES and the rest of the index is already in ORDER BY order.
--
-- `status, content_rating` are appended as PAYLOAD, not as seek keys. They cannot be seek
-- keys: they sit BELOW the sort columns, and moving them above would destroy the ordering
-- the index exists to provide. As payload they keep the filtered COUNT index-only —
-- measured 4.38 ms with them versus 24.31 ms without, where the difference is 48k table
-- lookups to read two small strings.
--
-- `work_id DESC` is the mandatory tiebreaker, same as 0064: `en_chapter_count` has huge
-- ties (35,984 works share a value in the 0-5 range), and without a total order
-- LIMIT/OFFSET repeats or skips rows between pages.
CREATE INDEX IF NOT EXISTS idx_fsu_browse_chapters
    ON feed_series_updates(is_nsfw, en_chapter_count DESC, work_id DESC, status, content_rating);
CREATE INDEX IF NOT EXISTS idx_fsu_browse_type_chapters
    ON feed_series_updates(is_nsfw, comic_type, en_chapter_count DESC, work_id DESC, status, content_rating);
CREATE INDEX IF NOT EXISTS idx_fsu_browse_all_chapters
    ON feed_series_updates(en_chapter_count DESC, work_id DESC, status, content_rating);
CREATE INDEX IF NOT EXISTS idx_fsu_browse_type_all_chapters
    ON feed_series_updates(comic_type, en_chapter_count DESC, work_id DESC, status, content_rating);

-- TWO payload-widened twins of 0064's newest-first indices, for the NEWEST sort (which
-- Browse shares with `updatesFeed`) and for the second phase of the RATING and TRENDING
-- sorts, which page their unranked remainder on the same key.
--
-- 0064's `idx_fsu_order` / `idx_fsu_all_order` already avoid the temp B-tree, so these add
-- nothing to the PAGE query — they exist so the filtered COUNT stays index-only once a
-- status or content-rating predicate is present. `comic_type` is in the payload here
-- rather than in the seek prefix on purpose: that lets ONE index cover the format filter
-- AND both new filters for a given NSFW branch, instead of needing a 2x2x2 fan-out. When
-- no status/rating filter is present the planner is free to prefer 0064's narrower
-- `idx_fsu_type_order`, whose `comic_type` IS a seek prefix; both plans are sort-free.
--
-- 0064's four are LEFT ALONE: `updatesFeed` reads them, they are the narrower (cheaper to
-- maintain) shape for the no-extra-filter case, and dropping them would make that
-- resolver's measured plans stale.
CREATE INDEX IF NOT EXISTS idx_fsu_browse_order
    ON feed_series_updates(is_nsfw, released_at DESC, work_id DESC, comic_type, status, content_rating);
CREATE INDEX IF NOT EXISTS idx_fsu_browse_all_order
    ON feed_series_updates(released_at DESC, work_id DESC, comic_type, status, content_rating);

-- The genre facet list Browse renders as filter chips, materialized.
--
-- TWO REASONS, and the second matters more than the first.
--
-- (1) COST. Counting genres live over `work_tag` joined to the feed measures 300-465 ms,
--     on every Browse page load, for a list that changes twice a day (the MangaDex
--     catalogue sync cadence).
--
-- (2) HONESTY. The facets came from `series_cache::genre_facets`, which JSON-parsed all
--     13,847 `suwayomi_series.genre` blobs per request: 301 distinct entries, ungated for
--     NSFW, whose top entry was literally "Japanese" (an origin tag, not a genre), and
--     whose counts described the Suwayomi subset while the FILTER they drove ran against
--     something else entirely. Deriving both the facet counts and the filter from
--     `work_tag` joined to this exact table makes the number on a chip equal the number of
--     results clicking it returns — which was never true before.
--
-- `safe_count` / `all_count` rather than one count, so the facet list is NSFW-gated the
-- same way every other reader surface is: an opted-out viewer sees the counts they would
-- actually get. Both are stored because the gate is per-viewer and cannot be baked in.
--
-- ~100 rows once MangaDex tags are ingested, so `ORDER BY count DESC, tag ASC` at read
-- time needs no index of its own.
--
-- EMPTY UNTIL TAGS EXIST. `work_tag` holds 0 rows in production right now (migration 0066
-- added the `source` column and the reverse index; the MangaDex tag INGEST that fills it
-- ships alongside). Until that ingest runs, this table rebuilds to 0 rows and Browse
-- offers no genre chips — deliberately preferred over keeping 301 wrong ones.
CREATE TABLE IF NOT EXISTS feed_genre_facet (
    tag        TEXT PRIMARY KEY,
    safe_count INTEGER NOT NULL,
    all_count  INTEGER NOT NULL
);

-- BACKFILL. Mirrors `catalog::refresh_feed_series_updates`' phases exactly, so the columns
-- are populated before the first request rather than after the first boot refresh. Kept in
-- sync by hand with that function, the same way 0050's backfill mirrors
-- `series_cache::derive_latest_chapter_at`; the rebuild is authoritative and overwrites
-- all of this on the next sync cycle.
UPDATE feed_series_updates SET
    status = COALESCE((SELECT CASE w.status
                                  WHEN 'ON_HIATUS'           THEN 'HIATUS'
                                  WHEN 'PUBLISHING_FINISHED' THEN 'COMPLETED'
                                  WHEN 'LICENSED'            THEN 'COMPLETED'
                                  ELSE COALESCE(w.status, 'UNKNOWN')
                              END
                       FROM work w WHERE w.id = feed_series_updates.work_id), 'UNKNOWN'),
    content_rating = COALESCE((SELECT w.content_rating FROM work w
                               WHERE w.id = feed_series_updates.work_id), 'safe');

-- English mirror count first. Grouped once into a derived table and JOINED rather than
-- written as a correlated `COALESCE((SELECT COUNT(DISTINCT …)), 0)` per feed row. Not for
-- speed — measured on a copy of production (48,526 feed rows over 805,307 chapters) the two
-- forms are indistinguishable, 3.08 s grouped vs 2.95 s correlated. The join form is chosen
-- because it TOUCHES ONLY the rows it has a count for, leaving a work with no English
-- mirror at the column's 0 default; the correlated form writes an explicit 0 over every
-- such row, which is what makes the Suwayomi fallback below expressible as a second,
-- narrower UPDATE instead of having to be folded into one COALESCE chain.
UPDATE feed_series_updates SET en_chapter_count = en.n
    FROM (SELECT ss.work_id AS work_id, COUNT(DISTINCT c.number) AS n
            FROM chapter c
            JOIN source_series ss ON ss.id = c.source_series_id
           WHERE ss.source_type = 'mangadex' AND c.lang = 'en'
           GROUP BY ss.work_id) AS en
    WHERE en.work_id = feed_series_updates.work_id;

-- Then the Suwayomi fallback, for a work whose English mirror counted nothing. `MAX` over
-- the work's sources matches how the scanner half of 0064 picks its row (the
-- newest-releasing source's own numbers); `> 0` so the fallback can only ever raise a 0,
-- never lower a real mirror count.
UPDATE feed_series_updates SET en_chapter_count = sw.n
    FROM (SELECT ss.work_id AS work_id, MAX(sy.chapter_count) AS n
            FROM source_series ss
            JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER)
           WHERE ss.source_type = 'suwayomi'
           GROUP BY ss.work_id) AS sw
    WHERE sw.work_id = feed_series_updates.work_id
      AND feed_series_updates.en_chapter_count = 0
      AND sw.n > 0;

-- The facet table's first generation. Identical to the rebuild's statement in
-- `refresh_feed_series_updates`; see there for why there is no `source` predicate.
INSERT INTO feed_genre_facet(tag, safe_count, all_count)
SELECT t.tag,
       SUM(CASE WHEN f.is_nsfw = 0 THEN 1 ELSE 0 END),
       COUNT(*)
  FROM work_tag t
  JOIN feed_series_updates f ON f.work_id = t.work_id
 GROUP BY t.tag;
