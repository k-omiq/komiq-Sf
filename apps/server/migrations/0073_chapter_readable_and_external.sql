-- Two columns MangaDex has always sent and we have always discarded: `readableAt` and
-- `externalUrl` (Phase A, findings F1).
--
-- WHAT IS BROKEN WITHOUT THEM
--
-- 1. ~35,000 mirrored chapters (4% of the mirror) are hosted somewhere else — MangaPlus,
--    Comikey, NamiComi, BiliBili — and carry an `externalUrl` instead of pages. We store
--    no such column, so the reader requests pages for a chapter that has none and renders
--    a blank screen. A substantial share of the "unstable image Worker" reports are this.
--
-- 2. `publishAt` is not a release clock. MangaDex stamps external chapters
--    `publishAt = 2037-12-31T15:00:00` as a sentinel, and every feed query here bounds on
--    `published_at <= now`, so **46 works whose every chapter is 2037-dated are
--    permanently absent from /updates**, and 21 more show a stale older chapter as their
--    latest. The real release time is `readableAt`, which is what MangaDex's own
--    latest-updates surface orders by.
--
-- TWO TRAPS, both measured against the live API before writing this:
--
--   * `pages == 0` does NOT identify an external chapter. Sampled bilibilicomics
--     externals report `pages: 45` and `pages: 50`. `externalUrl IS NOT NULL` is the only
--     valid test, which is why this stores the URL rather than a boolean.
--   * `readableAt >= publishAt` does NOT hold. Sampled bilibili chapters are readable two
--     weeks BEFORE their publishAt. So the two are independent timestamps, not a
--     scheduled/actual pair, and code must bound and sort on `readable_at` specifically.
--
-- NO BACKFILL HERE, DELIBERATELY. Both columns stay NULL on the 877,824 existing rows and
-- every reader spells the clock `COALESCE(readable_at, published_at, created_at)`, so old
-- rows keep behaving exactly as they do today and nothing needs an 877k-row rewrite inside
-- a boot migration. The real values arrive two ways: new chapters carry them from the
-- firehose immediately, and existing rows are filled by the resumable backfill in
-- `mangadex::backfill_chapter_external_urls`, which re-walks the mirror against
-- `/chapter?ids[]=` 100 at a time. That backfill is what actually returns the 46 sentinel
-- works to the feed, so this migration alone is inert by design.
ALTER TABLE chapter ADD COLUMN readable_at TEXT;
ALTER TABLE chapter ADD COLUMN external_url TEXT;

-- The backfill's work-list is "mangadex chapters not yet filled", and it re-runs that
-- query once per 100-chapter batch across ~8,800 batches. Without an index that is a full
-- scan of 877k rows each time. Partial, so it costs nothing once the backfill drains: the
-- index holds only the rows still to do, and shrinks to empty.
CREATE INDEX IF NOT EXISTS idx_chapter_needs_readable_at
    ON chapter (source_series_id)
    WHERE readable_at IS NULL;
