-- Per-series view (chapter-read) tracking.
--
-- A "view" is recorded once per chapter open, for everyone — logged-in or anonymous —
-- since popularity is a public engagement signal (the product decision is to count all
-- reads, not dedupe per user). Counts are keyed by a NORMALISED series key so a series
-- read under either identity — its canonical `w_` work id or a numeric Suwayomi series
-- id — accrues to one total (a numeric id that maps to a work folds into that work's
-- key; see `views::view_key`).

-- All-time total per series key (one row per series). Read directly for the
-- series-page "all-time views" stat; incremented on every recorded view.
CREATE TABLE IF NOT EXISTS series_views (
    series_key TEXT PRIMARY KEY,
    total      INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT    NOT NULL
);

-- Hourly view counts per series key, for the last-24h / last-7d windows and for
-- ranking Trending. `hour_ts` is a Unix epoch hour (seconds / 3600). Pruned beyond the
-- 7-day retention window by the GC sweep, so this table stays small and bounded.
CREATE TABLE IF NOT EXISTS series_view_bucket (
    series_key TEXT    NOT NULL,
    hour_ts    INTEGER NOT NULL,
    views      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (series_key, hour_ts)
);

-- Trending ranks by SUM(views) over the recent buckets across ALL series, and the GC
-- sweep deletes by age — both scan by `hour_ts`, so index it.
CREATE INDEX IF NOT EXISTS idx_series_view_bucket_hour ON series_view_bucket (hour_ts);
