-- Real "newest chapter" timestamp for a cached Suwayomi series.
--
-- `Series.updatedAt` was sourced from `last_fetched_at` — Suwayomi's `lastFetchedAt`,
-- which our own scanner stamps to "now" on every poll (it fetches with fetchManga:
-- true). So "recently updated" orderings (discovery POPULAR, Browse) actually sorted
-- by "recently polled": measured Spearman correlation against real chapter recency was
-- -0.06, median 480 days off. `updatedAt` keeps its honest meaning (last metadata
-- touch); this column is the real signal, and feeds now order by it.
--
-- Stored as a NORMALIZED millisecond-epoch string. The source `upload_date` is dirty —
-- mostly 13-digit millis but ~2% 10-digit seconds, plus a couple of corrupt values
-- (negative, a year-2326 outlier). We normalize to millis so ordering is numerically
-- correct rather than coincidentally-correct on string sort, and so `to_iso` (which
-- also auto-scales) is fed a consistent unit. NULL when a series has no datable
-- chapter yet. Runtime maintenance in `series_cache::put_chapters` applies the same
-- normalization (see `normalize_epoch_millis`).
ALTER TABLE suwayomi_series ADD COLUMN latest_chapter_at TEXT;

-- Backfill from existing chapters — a pure DB derivation, no re-scan needed.
-- Normalization mirrors `normalize_epoch_millis`:
--   * only numeric upload_date values (GLOB screens out non-digits / negatives),
--   * seconds-epoch (< 1e12) scaled to millis, millis passed through,
--   * far-future values (> 2100-01-01) dropped so a corrupt year-2326 upload_date
--     can't win the numeric MAX and pin its series to the top of every feed.
UPDATE suwayomi_series
SET latest_chapter_at = (
    SELECT CAST(MAX(
        CASE WHEN CAST(c.upload_date AS INTEGER) < 1000000000000
             THEN CAST(c.upload_date AS INTEGER) * 1000
             ELSE CAST(c.upload_date AS INTEGER) END
    ) AS TEXT)
    FROM suwayomi_chapter c
    WHERE c.manga_id = suwayomi_series.id
      AND c.upload_date GLOB '[0-9]*'
      AND CAST(c.upload_date AS INTEGER) > 0
      AND (CASE WHEN CAST(c.upload_date AS INTEGER) < 1000000000000
                THEN CAST(c.upload_date AS INTEGER) * 1000
                ELSE CAST(c.upload_date AS INTEGER) END) <= 4102444800000
)
WHERE EXISTS (
    SELECT 1 FROM suwayomi_chapter c
    WHERE c.manga_id = suwayomi_series.id
      AND c.upload_date GLOB '[0-9]*'
      AND CAST(c.upload_date AS INTEGER) > 0
);

-- Index carries the full feed ORDER BY (latest_chapter_at DESC, id DESC) under the
-- in_library filter, so Browse/discovery can read it in order without a temp B-tree.
-- NULLs sort last under DESC (SQLite treats NULL as smallest), which is exactly where
-- chapterless series belong — so the query needs no explicit `IS NULL` ordering term.
CREATE INDEX IF NOT EXISTS idx_suwayomi_series_latest_chapter
    ON suwayomi_series(in_library, latest_chapter_at DESC, id DESC);
