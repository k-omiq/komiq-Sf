-- Extension-level sync subscriptions (source-sync §3).
--
-- A one-time bulk ingest ("add all from extension") walks a source's listing ONCE;
-- series added to that extension later are never discovered, and single-added series
-- historically drifted out of the Suwayomi library entirely. This table records the
-- admin's explicit intent to KEEP an extension synced: the background source-sync job
-- re-walks each subscribed extension's sources (LATEST) every interval, auto-enrolls
-- any newly-appeared series, and reconciles library membership so enrolled series keep
-- getting scanned for new chapters.
--
-- Keyed by `pkg_name` (the extension package id) because the subscribe toggle is
-- per-extension, and one extension backs many per-language Suwayomi sources.
CREATE TABLE IF NOT EXISTS extension_subscription (
    -- NOT NULL is explicit: a non-INTEGER SQLite PRIMARY KEY otherwise permits NULLs
    -- (a legacy quirk), which would let duplicate "ghost" subscriptions slip past the
    -- ON CONFLICT(pkg_name) upsert.
    pkg_name       TEXT PRIMARY KEY NOT NULL,  -- extension package id (joins SuwayomiSource.pkg_name)
    created_at     TEXT NOT NULL,      -- when the subscription was enabled (RFC3339)
    last_synced_at TEXT,               -- last completed sync pass (RFC3339); NULL = never
    last_added     INTEGER NOT NULL DEFAULT 0,  -- new series enrolled on the last pass
    last_error     TEXT                -- last pass's error, NULL on success
);
