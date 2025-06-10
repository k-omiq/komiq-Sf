-- Clamp scan schedules that were written under the old ~100-year interval ceiling, and
-- pull every non-paused series onto the new active poll ceiling.
--
-- WHY
-- ---
-- `scanner::resolve_interval` used to clamp the INFERRED rolling upload average with
-- `MAX_INTERVAL_HOURS`, which was ~100 years — an overflow guard for the `chrono` maths,
-- not a scheduling policy. A series with only 2–5 cached chapters whose upload dates span
-- years yields an "average" of tens of thousands of hours, so those rows were legitimately
-- scheduled decades out. Live examples: "The Skeleton Soldier Failed to Defend the
-- Dungeon" (avg_interval_hours = 58,309 ≈ 6.6 years) parked until 2033-03-14, and
-- "Ragna Crimson" until 2031-12-14.
--
-- The 14-day `MAX_INTERVAL_HOURS` clamp fixed new WRITES, but nothing rewrote the rows
-- already stored. The due-query is a bounded `next_scan_at <= now` range seek, so those
-- rows can never match: they are permanently invisible to the scheduler and their series
-- never gets another chapter. Measured on production before this migration: 3,578 rows
-- scheduled more than 372h out (1,676 of them on ONGOING series), every one of them last
-- written BEFORE the 2026-07-24 deploy that introduced the 14-day clamp, with
-- MAX(next_scan_at) = 2033-03-14.
--
-- Separately, `scanner::ACTIVE_MAX_INTERVAL_HOURS` now caps a non-paused series' steady
-- poll cadence at 12h. That cap only takes effect the next time a series is SCANNED, so
-- without statement 1 below the 4,966 active rows currently scheduled 14h–14d out would
-- keep their old, publication-rate schedule for up to another fortnight — i.e. the whole
-- point of the change would not land for two weeks.
--
-- COST / SAFETY
-- -------------
-- Both UPDATEs are driven by `idx_scan_state_next_scan` (a bounded `next_scan_at > ?`
-- range seek) over a 13,789-row table and touch ~6,900 rows: sub-second, no long lock.
-- Both are idempotent — after they run nothing matches their predicates, and the running
-- code cannot re-create either class (a non-paused series caps at 12h + 10% jitter, a
-- paused one at PAUSED_PARK_HOURS + 10% jitter <= 369h). `scanner::reclaim_absurd_schedules`
-- is the belt-and-braces read-side net for the same invariant.
--
-- Timestamps are written in the same RFC3339-with-offset shape the Rust writers use
-- (`DateTime::<Utc>::to_rfc3339`), so string ordering against existing rows is correct.
-- Every rescheduled row gets a random offset so a reclaimed cohort arrives spread out
-- instead of as one thundering herd — the same reason `jitter_interval_hours` exists.

-- 1) Non-paused series scheduled beyond the active ceiling (+2h of slack over the 12h
--    cap plus its 10% jitter) come due at a random point in the next 12h.
--
--    "Paused" mirrors `scanner::is_paused` + `scanner::effective_status` — i.e.
--    `graphql::types::status_from` / `paused_for_status` / `komika_status` plus the
--    `series_admin` overrides — exactly. Paused series must NOT be pulled in here: they
--    belong on the 14-day park, and dragging 7,397 of them onto a 12h sweep would
--    multiply the scan budget. A row whose series_id is not a Suwayomi manga id (no
--    `suwayomi_series` row) is left alone by the inner join.
--
--    Two subtleties the obvious `COALESCE(paused_override, 0) = 0 AND COALESCE(
--    status_override, <upstream>) NOT IN (...)` spelling gets WRONG, both verified
--    against the Rust by `scanner::tests::migration_0057_pause_predicate_matches_rust`:
--
--      a) `paused_override` is three-valued. `Option::map(|v| v != 0).unwrap_or_else(...)`
--         means a NON-NULL 0 is an explicit "keep scanning this" that BEATS the status —
--         which is exactly what `setSeriesPaused(paused: false)` writes on a COMPLETED
--         series. `COALESCE(paused_override, 0) = 0` folds that case in with "no override
--         at all" and then lets the COMPLETED status veto it, stranding a series the admin
--         deliberately un-paused at its legacy multi-year `next_scan_at`.
--      b) `komika_status` returns None for any word outside its five, so an unrecognised
--         `status_override` falls BACK to the upstream status in Rust. A bare COALESCE
--         instead lets the unrecognised word through, and since it can't be one of the
--         three paused words the row is treated as active — pulling a genuinely paused
--         series onto the 12h sweep.
--
--    Live today this changes nothing (all 4 `series_admin` rows carry NULL overrides), but
--    the migration runs against whatever state the deploy finds.
UPDATE series_scan_state
   SET next_scan_at = strftime('%Y-%m-%dT%H:%M:%S', 'now', '+' || (ABS(RANDOM()) % 720) || ' minutes') || '+00:00',
       updated_at   = strftime('%Y-%m-%dT%H:%M:%S', 'now') || '+00:00'
 WHERE next_scan_at > strftime('%Y-%m-%dT%H:%M:%S', 'now', '+14 hours') || '+00:00'
   AND series_id IN (
       SELECT ss.series_id
         FROM series_scan_state ss
         JOIN suwayomi_series s ON s.id = CAST(ss.series_id AS INTEGER)
         LEFT JOIN series_admin a ON a.series_id = ss.series_id
        WHERE CASE
                WHEN a.paused_override IS NOT NULL THEN a.paused_override = 0
                ELSE COALESCE(
                       -- `komika_status`: anything else parses to None and is ignored.
                       CASE WHEN a.status_override IN
                                 ('ONGOING', 'COMPLETED', 'HIATUS', 'CANCELLED', 'UNKNOWN')
                            THEN a.status_override
                       END,
                       -- `status_from` over the cached upstream status.
                       CASE s.status
                           WHEN 'ONGOING'             THEN 'ONGOING'
                           WHEN 'COMPLETED'           THEN 'COMPLETED'
                           WHEN 'PUBLISHING_FINISHED' THEN 'COMPLETED'
                           WHEN 'LICENSED'            THEN 'COMPLETED'
                           WHEN 'CANCELLED'           THEN 'CANCELLED'
                           WHEN 'ON_HIATUS'           THEN 'HIATUS'
                           ELSE 'UNKNOWN'
                       END
                     ) NOT IN ('COMPLETED', 'HIATUS', 'CANCELLED')
              END
   );

-- 2) Whatever is STILL parked past the absurd horizon after statement 1 is, by
--    construction, a paused (or unjoinable) row carrying a legacy multi-decade schedule.
--    Bring it back onto the 14-day park window rather than the 12h sweep: it costs one
--    fetch per row per fortnight, restores upstream-reopen detection (COMPLETED -> ONGOING
--    auto-resumes scanning), and spreads the arrivals across the whole window.
--    16 days is the horizon because no writer can exceed PAUSED_PARK_HOURS + its jitter.
UPDATE series_scan_state
   SET next_scan_at = strftime('%Y-%m-%dT%H:%M:%S', 'now', '+' || (ABS(RANDOM()) % 20160) || ' minutes') || '+00:00',
       updated_at   = strftime('%Y-%m-%dT%H:%M:%S', 'now') || '+00:00'
 WHERE next_scan_at > strftime('%Y-%m-%dT%H:%M:%S', 'now', '+384 hours') || '+00:00';

-- 3) Retire the absurd stored averages themselves, so the value the API and the admin
--    console surface agrees with what the scheduler will do with it, and so nothing
--    downstream can read "this series publishes every 6.6 years" as a fact. This mirrors
--    the write-time clamp now applied in `scanner::record_scan_once`; the scanner
--    recomputes the average from live chapters on every scan anyway, so this only fixes
--    the window before each row's next scan. Idempotent by construction.
UPDATE series_scan_state
   SET avg_interval_hours = 336.0,
       updated_at         = strftime('%Y-%m-%dT%H:%M:%S', 'now') || '+00:00'
 WHERE avg_interval_hours > 336.0;
