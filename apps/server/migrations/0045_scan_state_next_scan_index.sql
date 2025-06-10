-- Index for the DB-driven scan scheduler (scanner §work-selection).
--
-- The scanner used to list the ENTIRE Suwayomi library every tick and filter due-ness
-- in app code — O(library) upstream + O(library) serial DB reads per tick, painful at
-- 100k+ series. It now selects only the due rows straight from the DB:
--   SELECT series_id FROM series_scan_state WHERE next_scan_at <= ? ORDER BY next_scan_at ... LIMIT ?
-- "Due now" is stored as a far-past sentinel (migration 0048), not NULL, so this is a
-- single `<= ?` range the index both ORDERS (no temp b-tree sort) and EARLY-TERMINATES at
-- the first future-dated row — future rows are never visited, so tick cost is O(due), not
-- O(catalogue).
CREATE INDEX IF NOT EXISTS idx_scan_state_next_scan
    ON series_scan_state(next_scan_at);
