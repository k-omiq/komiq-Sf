-- Cursor state for the recurring MangaDex catalogue/chapter sync (CATALOGUE.md §5).
-- One row per job. last_synced_at is stored in MangaDex's `since` format
-- (YYYY-MM-DDTHH:MM:SS) and fed straight back as `updatedAtSince` on the next
-- incremental cycle. Absent row => the job has never run => do a full createdAt seed.
CREATE TABLE catalogue_sync_state (
    job            TEXT PRIMARY KEY,   -- 'catalogue' | 'chapters'
    last_synced_at TEXT NOT NULL,      -- updatedAtSince for the next run (MangaDex since format)
    updated_at     TEXT NOT NULL       -- ISO 8601, last row write
);
