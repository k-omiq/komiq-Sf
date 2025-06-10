-- Reader-cache TTL refresh (S1 follow-up). The reader used to serve a cached
-- Suwayomi series/chapter list forever on a cache HIT — a series a user browses but
-- that isn't in the scan rotation would show stale metadata/chapters indefinitely.
--
-- `suwayomi_series.updated_at` can't drive a series-metadata TTL because
-- `put_chapters` also bumps it (chapter-count sync), so a series whose chapters are
-- refreshed hourly would look "freshly fetched" and its metadata would never
-- revalidate. This dedicated column records ONLY when the series METADATA itself was
-- last fetched from upstream (set exclusively by `put_series`), so the 6h metadata
-- TTL is measured against the right event. Chapter freshness uses
-- `MAX(suwayomi_chapter.updated_at)`, which `put_series` never touches, so it needs
-- no new column.
ALTER TABLE suwayomi_series ADD COLUMN series_fetched_at TEXT;

-- Backfill existing rows with the best available proxy so already-cached series get
-- a sane initial freshness anchor (else every row would look infinitely stale and
-- stampede a refetch on first read).
UPDATE suwayomi_series SET series_fetched_at = updated_at
WHERE series_fetched_at IS NULL;
