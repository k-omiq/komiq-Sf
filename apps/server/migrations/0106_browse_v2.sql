-- Browse v2 ("MangaDex advanced search"): the filter/sort columns, the MangaDex statistics
-- cache, and a normalized tag vocabulary.
--
-- WHY THIS MIGRATION EXISTS
--
-- Browse (`browse::browse_catalogue`, over the materialized `browse_catalogue` of 0069) could
-- filter by one format, one status, a cumulative content-rating CEILING and an ANY-match over
-- free-text genre names, and could sort five ways. The reader now asks for MangaDex's whole
-- advanced-search surface: include/exclude tags with AND/OR, several statuses and formats at
-- once, demographic, the exact content rating, original language, a year range, minimum
-- chapters, minimum rating and follows — and sorts by follows, rating, title and year in
-- either direction. Every one of those that is a property of the WORK is already stored on
-- `work` (MangaDex ingest writes `demographic`, `original_language`, `year` and
-- `content_rating` — see `mangadex::to_work_input`); they are materialized onto
-- `browse_catalogue` here for the reason 0068's header gives for `status` and
-- `en_chapter_count`: a predicate that is not a column of the paged table cannot be an index
-- prefix, and without one every filtered page sorts the whole 115k-row catalogue through a
-- temp B-tree.
--
-- Pure cache, like every other column of the table: `catalog::refresh_browse_catalogue`
-- rewrites them from `work` / `work_stats` on every rebuild, and
-- `scanner::mirror_feed_row_into_browse_catalogue` copies them on the incremental path
-- (both through `catalog::BROWSE_CATALOGUE_MUTABLE`, so they cannot drift — the scanner's
-- `incremental_write_converges_with_the_periodic_rebuild` digests them).

-- Normalized to MangaDex's four words, lowercase; NULL = no demographic ("none" upstream, or
-- unknown). The `Demographic.NONE` filter value is `demographic IS NULL`.
ALTER TABLE browse_catalogue ADD COLUMN demographic TEXT;
-- MangaDex `originalLanguage`, lowercased ("ja", "ko", "zh", "zh-hk", "en", …). NULL when
-- unknown (a Suwayomi-only work carries none).
ALTER TABLE browse_catalogue ADD COLUMN original_language TEXT;
-- Publication year. NULL when unknown; sorted LAST in both directions.
ALTER TABLE browse_catalogue ADD COLUMN year INTEGER;
-- MangaDex statistics (copied from `work_stats` below). NULL until the stats drain has
-- reached the work, and forever for a work with no MangaDex anchor; sorted LAST in both
-- directions, and excluded by any `minFollows` / `minRating` bound.
ALTER TABLE browse_catalogue ADD COLUMN follows INTEGER;
ALTER TABLE browse_catalogue ADD COLUMN rating REAL;

-- `content_rating` already exists (0069). Its MEANING is tightened by the refresh, not by
-- this file: it is now the EFFECTIVE rating — the work's MangaDex rating, except that a work
-- whose effective NSFW flag is set but whose stored rating is `safe`/`suggestive`/unknown is
-- materialized as `erotica` (see `catalog::effective_content_rating_sql` for the why). The
-- NSFW gate itself (`is_nsfw`) is untouched.

-- ---- MangaDex statistics -------------------------------------------------------------
--
-- One row per MangaDex-anchored work the statistics drain (`mangadex::backfill_work_stats`)
-- has asked about. `GET /statistics/manga?manga[]=…` answers 100 ids per call with
-- `{follows, rating: {average, bayesian}}` per id.
--
--   follows     MangaDex follow count. NULL if upstream did not answer for the id.
--   rating      the BAYESIAN rating (0-10) — what MangaDex itself sorts "rating" by, and
--               what Browse's HIGHEST_RATED / minRating use. A raw average over 3 votes is
--               not a ranking signal.
--   rating_avg  the plain average, kept for display.
--   fetched_at  when the drain last wrote the row (RFC 3339). Every pending id gets a row
--               with this set — answered or not — which is what makes the drain terminate.
--
-- Separate from `work` because it is re-fetched on its own clock (weekly), and keeping a
-- volatile counter off the 115k-row `work` table keeps the catalogue upsert's change guard
-- quiet.
CREATE TABLE IF NOT EXISTS work_stats (
    work_id    TEXT PRIMARY KEY REFERENCES work(id) ON DELETE CASCADE,
    follows    INTEGER,
    rating     REAL,
    rating_avg REAL,
    fetched_at TEXT NOT NULL
);
-- The drain's "stalest first" half: an index walk from the oldest `fetched_at`, which a
-- LIMIT short-circuits. The "never fetched" half drives from `source_series` instead.
CREATE INDEX IF NOT EXISTS idx_work_stats_fetched ON work_stats(fetched_at);

-- ---- the tag vocabulary ----------------------------------------------------------------
--
-- `work_tag.tag` is a DISPLAY name ("Boys' Love"), and the reader filters by URL-safe ids.
-- `tag_key` is that id — the slug of the canonical English name (`tags::slug`:
-- lowercased, every run of non-alphanumerics folded to one '-', trimmed: "Boys' Love" ->
-- "boys-love", "Sci-Fi" -> "sci-fi", "4-Koma" -> "4-koma"). Every writer sets it, and
-- `tags::fill_tag_keys` fills any row a writer left NULL on each rebuild, so it is never a
-- second source of truth — only a function of `tag`.
--
-- A COLUMN rather than a separate link table: `work_tag` already IS the link table (every
-- writer, the effective-genre precedence and the merge fold read it), and a second table
-- derived from it would be one more thing to rebuild and one more way for the filter and the
-- facet counts to disagree.
ALTER TABLE work_tag ADD COLUMN tag_key TEXT;

-- The filter's two access paths, both covering:
--   (tag_key, work_id)  include-AND / exclude-AND: "works carrying these tags", one seek per
--                       tag, then GROUP BY work_id … HAVING COUNT(DISTINCT tag_key) = n.
--   (work_id, tag_key)  include-OR / exclude-OR: "does THIS work carry any of these",
--                       a correlated probe per candidate Browse row.
-- `browse::tests::tag_filters_use_the_tag_indexes` asserts both with EXPLAIN QUERY PLAN.
CREATE INDEX IF NOT EXISTS idx_work_tag_key_work ON work_tag(tag_key, work_id);
CREATE INDEX IF NOT EXISTS idx_work_tag_work_key ON work_tag(work_id, tag_key);

-- One row per distinct `tag_key`, rebuilt inside `refresh_browse_catalogue`'s transaction
-- (`tags::rebuild_tag_table`) so a count can never describe a generation that no longer
-- exists. Counts are over `browse_catalogue`, i.e. the works the filter can return, with the
-- NSFW-gated count beside the full one exactly like `feed_genre_facet`.
--
--   name          canonical display name — MangaDex's English name where any MangaDex row
--                 carries the tag, else the first name seen
--   tag_group     genre | theme | format | content (MangaDex's four groups; a tag with no
--                 known group — admin free text — is filed under `genre`)
--   mangadex_id   the MangaDex tag uuid, where known
CREATE TABLE IF NOT EXISTS tag (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    tag_group       TEXT NOT NULL DEFAULT 'genre',
    mangadex_id     TEXT,
    work_count      INTEGER NOT NULL DEFAULT 0,
    safe_work_count INTEGER NOT NULL DEFAULT 0
);

-- ---- browse_catalogue indexes ----------------------------------------------------------
--
-- Same two-shape convention as 0069/0102: one index per sort key with the NSFW gate's
-- `is_nsfw` as a leading equality (the opted-out, edge-cacheable path) and one without it
-- (the opted-in path, which emits no NSFW predicate at all). Each ends in `work_id` so the
-- mandatory tiebreaker is served by the index too, and a DESC sort reads the index forwards
-- while its ASC mirror reads the same index backwards. NULL keys collate first in an ASC
-- index, so a DESC walk meets them LAST — which is the "NULLs last" contract for free in the
-- DESC direction; the ASC direction gets it from `browse`'s two-phase split instead.
CREATE INDEX IF NOT EXISTS idx_bc_follows
    ON browse_catalogue(is_nsfw, follows DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_bc_all_follows
    ON browse_catalogue(follows DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_bc_rating
    ON browse_catalogue(is_nsfw, rating DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_bc_all_rating
    ON browse_catalogue(rating DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_bc_year
    ON browse_catalogue(is_nsfw, year DESC, work_id DESC);
CREATE INDEX IF NOT EXISTS idx_bc_all_year
    ON browse_catalogue(year DESC, work_id DESC);
-- TITLE sorts case-insensitively (`ORDER BY f.title COLLATE NOCASE`), so the index carries
-- the same collation or the planner cannot use it for the ORDER BY.
CREATE INDEX IF NOT EXISTS idx_bc_title
    ON browse_catalogue(is_nsfw, title COLLATE NOCASE, work_id);
CREATE INDEX IF NOT EXISTS idx_bc_all_title
    ON browse_catalogue(title COLLATE NOCASE, work_id);

-- Filter columns. Selective filters ("Josei", "ko", "erotica", "CANCELLED") are cheaper to
-- drive from their own index than to test row by row along a sort index; `is_nsfw` second so
-- an opted-out viewer's filter is still one seek.
CREATE INDEX IF NOT EXISTS idx_bc_demographic
    ON browse_catalogue(demographic, is_nsfw);
CREATE INDEX IF NOT EXISTS idx_bc_language
    ON browse_catalogue(original_language, is_nsfw);
CREATE INDEX IF NOT EXISTS idx_bc_content_rating
    ON browse_catalogue(content_rating, is_nsfw);
CREATE INDEX IF NOT EXISTS idx_bc_status
    ON browse_catalogue(status, is_nsfw);

-- ---- backfill ----------------------------------------------------------------------------
--
-- The new columns, from `work`, so they are right from the instant this lands instead of
-- after the boot refresh. Mirrors `catalog::BROWSE_*_SQL` BY HAND, the same arrangement
-- 0068/0069's backfills have with the refresh. (`follows`/`rating` stay NULL: `work_stats`
-- was created empty above.)
UPDATE browse_catalogue
   SET demographic = (SELECT CASE lower(trim(w.demographic))
                                 WHEN 'shounen' THEN 'shounen'
                                 WHEN 'shoujo'  THEN 'shoujo'
                                 WHEN 'seinen'  THEN 'seinen'
                                 WHEN 'josei'   THEN 'josei' END
                        FROM work w WHERE w.id = browse_catalogue.work_id),
       original_language = (SELECT NULLIF(lower(trim(w.original_language)), '')
                              FROM work w WHERE w.id = browse_catalogue.work_id),
       year = (SELECT CASE WHEN w.year BETWEEN 1800 AND 2200 THEN w.year END
                 FROM work w WHERE w.id = browse_catalogue.work_id),
       content_rating = CASE WHEN is_nsfw = 1
                              AND content_rating IN ('safe', 'suggestive')
                             THEN 'erotica' ELSE content_rating END;

ANALYZE browse_catalogue;
