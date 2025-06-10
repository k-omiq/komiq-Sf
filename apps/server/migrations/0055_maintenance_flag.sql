-- One-time maintenance markers (idempotency for boot-time backfills).
--
-- A tiny key→timestamp table so a one-shot maintenance pass (e.g. the MangaDex
-- catalogue backfill that fills series the original seed missed) runs exactly once
-- across restarts: the boot task checks for its key, runs if absent, and records the
-- key only on success — so a failed/interrupted pass simply retries on the next boot.
CREATE TABLE IF NOT EXISTS maintenance_flag (
    key     TEXT PRIMARY KEY,
    done_at TEXT NOT NULL
);
