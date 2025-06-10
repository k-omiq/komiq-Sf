-- "Latest Added" home row: track when a series was FIRST persisted into our
-- catalogue cache.
--
-- `updated_at` is touched on every upsert (scan/refresh), so it means "last seen",
-- not "first added"; `in_library_at` is Suwayomi's own library timestamp, not ours.
-- Neither answers "what did OUR catalogue gain most recently", so we record the
-- first-insert time explicitly and never overwrite it on later upserts.
ALTER TABLE suwayomi_series ADD COLUMN created_at TEXT;

-- Backfill existing rows with the best available proxy so ordering is stable for
-- the pre-existing catalogue (rows added before this column existed).
UPDATE suwayomi_series SET created_at = COALESCE(in_library_at, updated_at)
WHERE created_at IS NULL;

CREATE INDEX idx_suwayomi_series_created_at ON suwayomi_series(created_at);
