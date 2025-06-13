-- Honest scan failures: record WHY a scan failed, not just that it did (Phase E4, F11).
--
-- THE BUG THIS SERVES. Every fetch path in `suwayomi.rs` falls back to a plain
-- `manga(id:)` / `chapters(condition:)` QUERY when its `fetch*` MUTATION fails. Those
-- queries read Suwayomi's own database, so a completely broken source returns `Ok` with
-- stale — or empty — data, `record_scan` sees an unchanged chapter id-set, and the scan is
-- written down as a success with `consecutive_failures = 0`.
--
-- Measured 2026-07-30: `en.suryascans` (rebranded "Genz Toons") 404s on every chapter
-- fetch, yet all 209 of its series reported zero failures AND zero chapters, and the whole
-- 14,098-series library had `consecutive_failures = 0` on every row. "Healthy" and
-- "completely broken" were indistinguishable, permanently and by design.
--
-- WHY TWO COLUMNS AND NOT JUST THE EXISTING COUNTER. `consecutive_failures` already
-- counts strikes, but it cannot say what KIND of strike, and the distinction is the whole
-- point of E4: a source whose fetches error outright is visibly broken, while one that
-- quietly serves cache looks fine. Aggregating `last_failure_kind` per source is what
-- turns 209 silent per-series non-events into one loud per-source alert
-- (`catalog::source_scan_health`).
--
--   last_failure_kind  NULL while healthy. 'cached_fallback' = Suwayomi answered from its
--                      local DB because the upstream fetch failed (the F11 case).
--                      'fetch_error' = the fetch failed outright, nothing to fall back to.
--                      Cleared on the next successful scan, exactly like
--                      `consecutive_failures`, so it always describes the CURRENT state
--                      rather than accumulating history.
--   last_failure_at    When that strike landed. Distinct from `last_scanned_at`, which
--                      E4 stops advancing on a failed scan — so the pair answers "when
--                      did we last have fresh data" vs "when did we last try".
--
-- Nullable with no default, so this is a pure metadata add: every existing row starts
-- NULL/NULL, which reads as "no failure recorded", and the first real scan of each series
-- fills them in truthfully. No backfill is possible or wanted — the pre-E4 history is
-- exactly the history that lied.
ALTER TABLE series_scan_state ADD COLUMN last_failure_kind TEXT;
ALTER TABLE series_scan_state ADD COLUMN last_failure_at TEXT;

-- Per-source health is a GROUP BY over `series_scan_state` joined to `source_series` on
-- (source_type='suwayomi', source_key = series_id). `source_series` is already indexed on
-- its work/source columns, but nothing indexes `source_key`, so that join was a scan of
-- 14,103 rows per health read. Cheap to fix while here; the health query runs once per
-- tick that saw a failure and once per admin page load.
CREATE INDEX IF NOT EXISTS idx_source_series_suwayomi_key
    ON source_series (source_key, source_id)
    WHERE source_type = 'suwayomi';

-- One row per source currently in a WHOLE-SOURCE outage (E4.3).
--
-- "A source-wide outage should be one loud alert, not 209 silent ones." The per-series
-- columns above make each failure honest, but honesty alone converts one broken source
-- into 209 identical warnings per park window, which is its own kind of invisible. This
-- table is the debounce and the memory: the scanner opens a row when a source's series are
-- confirmably failing, alerts ONCE (re-alerting at most daily while it persists), parks
-- that source's series so a dead source costs ~30 fetches/day instead of 209, and deletes
-- the row when a probe finally succeeds.
--
-- WHY NOT `extension_subscription`. That table is keyed by `pkg_name` and only exists for
-- SUBSCRIBED extensions — its breaker governs the discovery walk, not scanning. The
-- motivating case has no such row at all: the suryascans subscription was removed on
-- 2026-07-30 while its 209 series kept scanning (and kept reporting success). Outage state
-- therefore belongs on the SOURCE, which is what `series_scan_state` rows actually map to.
-- The subscription breaker is still tripped alongside this row when a subscription exists.
--
-- WHY NOT `maintenance_flag`. That is a one-shot latch nothing ever clears; an outage must
-- be able to end.
CREATE TABLE IF NOT EXISTS source_scan_outage (
    -- Suwayomi source id (matches `source_series.source_id` / `source_extension.source_id`).
    source_id     TEXT PRIMARY KEY,
    -- Denormalised for the alert text and the admin surface; the extension may be gone.
    pkg_name      TEXT,
    -- When the outage was first detected. Survives re-alerts, so the admin surface can
    -- say "out since" rather than "last complained".
    detected_at   TEXT NOT NULL,
    -- Rate-limits the alert to one per `SOURCE_OUTAGE_REALERT_HOURS`.
    last_alert_at TEXT NOT NULL,
    -- Counts at detection time, for the alert and the admin panel.
    series        INTEGER NOT NULL DEFAULT 0,
    failing       INTEGER NOT NULL DEFAULT 0,
    -- The dominant `FailureKind` ('cached_fallback' | 'fetch_error'), i.e. whether the
    -- source is loudly broken or quietly serving cache.
    kind          TEXT,
    -- How far out this source's series were parked, so the panel can say when the next
    -- probe is due.
    parked_until  TEXT
);
