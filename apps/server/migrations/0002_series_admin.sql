-- Komika-native per-series admin overrides for the "manga DB" console.
-- The source status + derived scan cadence come from Suwayomi; these rows let an
-- admin override them. A NULL column means "no override — use the derived value".

CREATE TABLE series_admin (
    series_id               TEXT PRIMARY KEY,
    override_interval_hours REAL,      -- NULL => use avgIntervalHours
    poll_every_minutes      INTEGER,   -- NULL => default (30)
    paused_override         INTEGER,   -- NULL => auto (paused for completed/hiatus/cancelled), 0/1 => forced
    status_override         TEXT,      -- NULL => use source status; else ONGOING/COMPLETED/HIATUS/CANCELLED/UNKNOWN
    updated_at              TEXT NOT NULL
);
