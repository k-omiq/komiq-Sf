-- Adaptive scan scheduler state, one row per federated series (keyed by the
-- Suwayomi manga id as TEXT). The scanner (src/scanner.rs) maintains these rows;
-- map_series folds them back into Series.scan for the client/admin console.
-- Distinct from series_admin, which holds human overrides — this is derived state.

CREATE TABLE series_scan_state (
    series_id           TEXT PRIMARY KEY,
    avg_interval_hours  REAL    NOT NULL DEFAULT 0,  -- rolling avg gap between chapter uploads
    known_chapter_count INTEGER NOT NULL DEFAULT 0,  -- last observed chapter count (for new-chapter detection)
    last_scanned_at     TEXT,                        -- ISO 8601, last time the scanner fetched this series
    next_scan_at        TEXT,                        -- ISO 8601, last_scanned_at + effective interval
    last_new_chapter_at TEXT,                        -- ISO 8601, last time a new chapter was detected
    updated_at          TEXT NOT NULL                -- ISO 8601, last row write
);
