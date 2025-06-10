-- Covers the crawl could NOT process. Two jobs:
--   1. Stop the crawl re-attempting DETERMINISTIC failures every tick — an
--      unsupported/oversized source will fail identically forever, so retrying it
--      wastes an upstream fetch + decode each cycle (and spams the logs). Works with
--      a row here are excluded from the crawl's SELECTs.
--   2. Feed the admin "Bugs" panel, which lists these for manual retry / cover upload.
--
-- Lives in the MAIN (replicated) DB, not the un-replicated covers DB: it's small
-- metadata keyed by work_id that we want backed up and visible to every replica.
-- A transient UPSTREAM-FETCH failure is deliberately NOT recorded here (it may
-- succeed next tick); only a deterministic decode/encode/size/store failure is.
CREATE TABLE IF NOT EXISTS work_cover_issue (
    work_id    TEXT PRIMARY KEY REFERENCES work(id) ON DELETE CASCADE,
    -- Machine code: 'too_large' | 'unsupported' | 'empty' | 'encode' | 'store'.
    reason     TEXT NOT NULL,
    -- Human-readable error from the most recent attempt (for the admin panel).
    detail     TEXT,
    attempts   INTEGER NOT NULL DEFAULT 1,
    first_seen TEXT NOT NULL,
    last_seen  TEXT NOT NULL
);

-- The admin panel lists most-recent failures first.
CREATE INDEX IF NOT EXISTS idx_work_cover_issue_last_seen
    ON work_cover_issue (last_seen DESC);
