-- Error-backoff tracking for the DB-driven scan scheduler (audit #1/#4).
--
-- A due series whose upstream fetch fails (Suwayomi/FlareSolverr down, an upstream
-- manga that 404s, a deleted numeric id) never advanced `next_scan_at`, so it stayed
-- pinned at the front of the due-ordering forever. At scale that (a) let >=DUE_BATCH_LIMIT
-- perpetually-failing rows starve every healthy series behind them, and (b) let a sustained
-- outage spin the drain loop with no backoff. `scan_due` now records a failure here and
-- pushes `next_scan_at` out with an exponential, capped backoff; a successful scan resets
-- the counter to 0.
--
-- ADD COLUMN with a constant default is an O(1) metadata change in SQLite (no table
-- rewrite), so this is safe on a live 120k-row DB.
ALTER TABLE series_scan_state
    ADD COLUMN consecutive_failures INTEGER NOT NULL DEFAULT 0;
