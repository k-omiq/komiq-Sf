-- Phase E3 — per-source scanners.
--
-- Today there is ONE global scan loop with ONE due-set across the whole library and one
-- SCAN_CONCURRENCY = 3. A single misbehaving source can occupy all three slots with
-- 30s-timeout stalls and starve every other source for the batch. E3 gives each source its
-- own loop, due-set, and health — the win is ISOLATION, not throughput.
--
-- `series_scan_state` is keyed by `series_id` (the Suwayomi manga id) and carries NO
-- `source_id`, so a per-source due-query would need a join to `source_series`. Denormalise
-- it on and index `(source_id, next_scan_at)` so each loop keeps the exact bounded range
-- seek the global one has today (O(due), terminating at the first future-dated row).
--
-- Backfill from `source_series` (the `source_type='suwayomi'` anchor whose `source_key` IS
-- the `series_id`). Rows that don't map stay NULL and are swept by the supervisor's
-- "unassigned" loop, so nothing is ever orphaned — see `scanner::run_source_loop`.
ALTER TABLE series_scan_state ADD COLUMN source_id TEXT;

UPDATE series_scan_state
   SET source_id = (
       SELECT ss.source_id FROM source_series ss
        WHERE ss.source_type = 'suwayomi'
          AND ss.source_key = series_scan_state.series_id
        LIMIT 1
   )
 WHERE source_id IS NULL;

-- The per-source due-query index. `source_id IS NULL` is a valid range on this index too,
-- so the unassigned-sweeper loop uses the same seek.
CREATE INDEX IF NOT EXISTS idx_scan_state_source_next
    ON series_scan_state(source_id, next_scan_at);
