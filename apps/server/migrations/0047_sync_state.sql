-- Source-sync pass bookkeeping (audit #3).
--
-- The source-sync scheduler's `tokio::time::interval` fires its first tick immediately on
-- boot, so every server restart re-ran a full pass (library reconcile + LATEST walks) —
-- and this checkout redeploys/restarts routinely, hammering upstream N times a day
-- regardless of the daily interval. This single-row table records when the last full pass
-- completed so the scheduler can skip the redundant immediate pass when one ran recently.
CREATE TABLE IF NOT EXISTS sync_state (
    id                INTEGER PRIMARY KEY CHECK (id = 1),  -- singleton row
    last_full_pass_at TEXT                                 -- RFC3339; NULL = never run
);
