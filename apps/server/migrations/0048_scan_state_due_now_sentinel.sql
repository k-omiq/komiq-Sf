-- "Due now" sentinel for the DB-driven scan scheduler (audit #6).
--
-- The due-query used `next_scan_at IS NULL OR next_scan_at <= ?`, which SQLite can't apply
-- as a range terminator — so an idle tick was a FULL index-only scan of series_scan_state
-- rather than a bounded O(due) seek. Storing "due now" as a far-past sentinel timestamp
-- instead of NULL lets the query be a single `next_scan_at <= ?` range that the index both
-- orders AND early-terminates (future rows never visited).
--
-- COMPLETENESS INVARIANT: under `<= ?` a NULL next_scan_at can never match, so a NULL row
-- would silently never be scanned. Backfill every legacy NULL to the sentinel; all app
-- writers now write the sentinel (crate::scanner::DUE_NOW_SENTINEL) or a real time, never
-- NULL. Idempotent — a re-run matches nothing once the NULLs are gone.
UPDATE series_scan_state
   SET next_scan_at = '1970-01-01T00:00:00+00:00'
 WHERE next_scan_at IS NULL;
