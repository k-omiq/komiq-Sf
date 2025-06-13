//! Adaptive scan scheduler.
//!
//! A background tokio task that keeps the federated library catalog fresh. Every
//! `SCAN_TICK_SECONDS` it selects — straight from the DB, indexed on `next_scan_at` —
//! only the series that have reached their persisted `next_scan_at` and re-scans those.
//! It does NOT fetch the whole Suwayomi library per tick, so tick cost scales with the
//! DUE set, not the catalogue size (which matters at 100k+ series). Enrolment and the
//! daily source-sync reconcile (`crate::sync`) guarantee every enrolled series has a
//! `series_scan_state` row for this query to find. That schedule is adaptive:
//!
//!   steady_interval = admin override_interval_hours  (if set)
//!                     else rolling avg gap between chapter uploads
//!                     (clamped into `[MIN_INTERVAL_HOURS, ACTIVE_MAX_INTERVAL_HOURS]`)
//!
//! POLL CADENCE IS NOT PUBLICATION CADENCE. The scheduler used to poll a series at
//! its own rolling upload gap, which guarantees an average detection lag on the order
//! of that gap: a fortnightly series was polled fortnightly, so its chapter was found,
//! on average, a week late. Production measured a p50 discovery lag of 28.8h against a
//! 300s tick that was nowhere near saturated. The inferred average is now used ONLY to
//! judge *lateness*; the poll cadence of a non-paused series is capped at
//! `ACTIVE_MAX_INTERVAL_HOURS`, which bounds worst-case discovery lag by construction.
//! Paused series still park at `PAUSED_PARK_HOURS` (see `park_paused`).
//!
//! After a scan finds a new chapter (or on first observation) the next scan is
//! scheduled a full `steady_interval` out. A series whose next chapter is GENUINELY
//! late — its newest upload is older than `LATE_FACTOR x` its own trustworthy
//! publication cadence, but not so old that the series has plainly died
//! (`LATE_BAND_MAX x`) — enters an "awaiting" state and is re-polled at the (clamped)
//! admin `poll_every_minutes` cadence. The accelerated poll runs for
//! `min(publication_interval, AWAITING_MAX_HOURS)` and then falls back to the steady
//! (<= 12h) cadence; the streak start is retained as a cool-down marker so the window
//! cannot immediately re-open, and is re-armed after `AWAITING_REARM_HOURS`.
//!
//! Lateness is deliberately NOT "a scheduled scan found nothing": `due_series_ids`
//! only ever returns rows that are already due, so that test is true by construction
//! and used to route ~73% of the fetch budget to a few hundred series.
//!
//! Due series get their chapters re-fetched; new chapters (detected by chapter
//! count OR max chapter number vs. the last known values) are logged. All derived
//! state is persisted in `series_scan_state`, which `graphql::map_series` folds
//! back into `Series.scan` for the client and admin console.
//!
//! Resilience: a per-series error is logged and skipped — it never aborts the tick
//! or the loop. The loop exits cleanly when the provided shutdown signal fires.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rand_core::RngCore;
use sqlx::SqlitePool;
use tokio::sync::{Notify, Semaphore};
use tokio::time::{interval, interval_at, MissedTickBehavior};

use crate::graphql::types::{
    komika_status, paused_for_status, status_from, ComicType, SeriesStatus,
};
use crate::graphql::AppState;
use crate::suwayomi::{SuwayomiChapter, SuwayomiManga};

/// Fallback cadence when no interval can be inferred yet (e.g. a series with
/// fewer than two dated chapters). Without this, the steady interval would be 0.0
/// and the series would be re-scheduled for immediate re-fetch on every tick.
///
/// Equal to `ACTIVE_MAX_INTERVAL_HOURS`: with the poll cadence decoupled from the
/// publication cadence there is no reason to poll a cadence-less series (3,257 of
/// 13,789 live rows carry `avg_interval_hours = 0`) any slower than the ceiling
/// everything else is capped at.
const DEFAULT_INTERVAL_HOURS: f64 = ACTIVE_MAX_INTERVAL_HOURS;

/// Upper bound on any effective interval.
///
/// This was ~100 years, which was only ever meant as an overflow guard for the
/// `chrono::Duration` math — but `resolve_interval` also clamps the INFERRED rolling
/// average with it, so a series whose sparse history implies a multi-year gap was
/// legitimately parked beyond any horizon we care about. In production that left 217
/// rows scheduled past 2027 and one series parked until 2033-03-14, i.e. never
/// rescanned again. 14 days matches `PAUSED_PARK_HOURS`: if a deliberately PAUSED
/// series is still worth re-checking fortnightly, an active one certainly is.
const MAX_INTERVAL_HOURS: f64 = PAUSED_PARK_HOURS as f64; // 14 days

/// HARD ceiling on the steady poll cadence of a NON-paused series, independent of
/// whatever cadence its uploads imply.
///
/// `MAX_INTERVAL_HOURS` is an overflow/absurdity guard, not a scheduling policy. Using
/// it as the steady ceiling meant the poll cadence tracked the *publication* cadence all
/// the way out to 14 days, which sets a floor under the discovery lag: production had
/// 5,242 series pinned at the 14-day ceiling, 1,840 at 7–14d, and 51% of ONGOING series
/// (3,189 of 6,255) scheduled more than a week apart — with a measured p50 discovery lag
/// of 28.8h and a p90 of 11.4 days.
///
/// 12h is chosen against the measured throughput budget. The STEADY floor is 6,392
/// non-paused series at 12h = 12,784 fetches/day, plus ~528/day for the 7,397 paused
/// series on their 14-day park — about 46 per 300s tick. That floor is NOT the whole
/// bill: the accelerated `awaiting` poll below (30 min for up to `AWAITING_MAX_HOURS`,
/// re-armable every `AWAITING_REARM_HOURS`) rides on top of it. Replaying the live
/// `suwayomi_chapter` upload history through this module's own scheduling maths for 20
/// simulated days puts 724 of the 6,392 inside the lateness band at any moment and lands
/// the real bill at:
///
///   steady state   ~16-21k scans/day  (p50 65/tick, p99 168/tick)
///   post-0057 peak  ~45k scans/day    (194 scans in the worst single tick, during the
///                                      48h after migration 0057 pulls the whole active
///                                      set onto one 12h window)
///
/// versus the ~105/tick (30,119/day) the scheduler was already spending and a
/// demonstrated capacity of ~295/tick (a 766-series drain tick completed in ~778s at
/// `SCAN_CONCURRENCY = 3`). So it still costs less than the status quo in steady state
/// and stays inside capacity at the peak — but the headroom at the peak is ~1.5x, not the
/// ~6x the 46/tick figure alone suggests. Tighten this constant only against the ~194
/// number, not the 46 one.
///
/// (The 240h re-arm is an un-jittered absolute offset from `awaiting_since`, so a cohort
/// that entered `awaiting` together re-arms together — visible in the replay as a
/// 60 -> 105/tick bump around day 10-12 that decays over subsequent cycles. Bounded and
/// self-dispersing, so it is left alone; jittering it would be the lever if it ever bites.)
const ACTIVE_MAX_INTERVAL_HOURS: f64 = 12.0;

/// Floor on an *inferred* (rolling-avg) interval. A same-day upload burst yields
/// sub-hour gaps, which would make the series overdue on essentially every 300s
/// tick and refetched every tick — needless source/FlareSolverr load. Real series
/// rarely sustain more than a few updates a day, so 6h is a safe floor for the
/// steady cadence (the accelerated overdue poll is a separate knob, see SC1).
const MIN_INTERVAL_HOURS: f64 = 6.0;

/// Hard floor applied to an explicit admin `override_interval_hours`. A deliberate
/// human override is allowed below `MIN_INTERVAL_HOURS` (that's the point of an
/// override), but never below this — it still protects upstreams from a 0.01h
/// hammer typo'd into the console.
const HARD_MIN_INTERVAL_HOURS: f64 = 1.0;

/// Resolve the steady effective POLL interval (hours) from an optional admin override
/// and the inferred rolling average (SC5):
///   - an explicit override wins and is clamped to `[HARD_MIN, MAX]` — a human who
///     deliberately parks a series for a week gets a week;
///   - otherwise the inferred avg (or the default when none) is clamped to
///     `[MIN, ACTIVE_MAX]`, so a burst series isn't refetched every tick AND a
///     slow-publishing series is still *checked* twice a day.
///
/// The upper bound is `ACTIVE_MAX_INTERVAL_HOURS`, NOT `MAX_INTERVAL_HOURS`: polling at
/// the publication rate guarantees an average detection lag on the order of the
/// publication interval, which is the single largest contributor to the observed 28.8h
/// median discovery lag. The long 14-day ceiling is reserved for the deliberate
/// `park_paused` path.
fn resolve_interval(override_interval_hours: Option<f64>, inferred_avg: f64) -> f64 {
    match override_interval_hours.filter(|v| *v > 0.0) {
        Some(o) => o.clamp(HARD_MIN_INTERVAL_HOURS, MAX_INTERVAL_HOURS),
        None => {
            let base = if inferred_avg > 0.0 {
                inferred_avg
            } else {
                DEFAULT_INTERVAL_HOURS
            };
            base.clamp(MIN_INTERVAL_HOURS, ACTIVE_MAX_INTERVAL_HOURS)
        }
    }
}

// ── Phase E: status- and format-aware cadence (F6) ───────────────────────────────
//
// This REPLACES the flat de-facto 12h that `resolve_interval` produced. §4.7 measured it:
// 93% of series landed on exactly 12h because `clamp(inferred_avg, 6, 12)` capped almost
// everything down to the ceiling and the "no cadence" default WAS the ceiling. The tier
// values below are returned as a TARGET, verbatim — no floor/ceiling clamp on the policy
// path (an admin override is still clamped) — which is the change that lets a manhwa reach
// 3h at all.
//
// Under E5 these tiers are the SAFETY-NET baseline, not the discovery mechanism: E5's
// LATEST-diff push signal detects a new chapter within ~15 min regardless of the tier. So
// the accelerated `awaiting` poll — 43% of all scan traffic (§8e), and by construction the
// least productive — is switched OFF entirely on the policy path; E5 is its replacement.

/// Active manhwa/manhua/webtoon — a chapter released within `DORMANT_AFTER_DAYS`.
const TIER_FAST_ACTIVE_HOURS: f64 = 3.0;
/// Manhwa/manhua/webtoon that has gone quiet (no release in `DORMANT_AFTER_DAYS`).
const TIER_FAST_DORMANT_HOURS: f64 = 24.0;
/// Manga and Western comics — a slower publication rhythm.
const TIER_MANGA_HOURS: f64 = 12.0;
/// A fast-format series with no release inside this window is treated as dormant and
/// slowed to `TIER_FAST_DORMANT_HOURS`. Crosses at most once per transition, so a scan-time
/// recompute (every 3h for an active series) catches it well within a day.
const DORMANT_AFTER_DAYS: i64 = 14;

/// Phase E (F6). The steady TARGET interval (hours) for a NON-paused series, by format and
/// recency. Paused statuses (COMPLETED/HIATUS/CANCELLED) never reach here — `scan_due`
/// parks them and E2/E5 triggers wake them — so this covers only ONGOING/UNKNOWN.
///
/// An explicit admin override still wins, clamped to `[HARD_MIN, MAX]`. Otherwise the value
/// is the tier target, returned WITHOUT the `[MIN, ACTIVE_MAX]` clamp that produced the flat
/// 12h — the whole point of the phase.
fn scan_interval_hours(
    comic_type: ComicType,
    last_release: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    override_interval_hours: Option<f64>,
) -> f64 {
    if let Some(o) = override_interval_hours.filter(|v| *v > 0.0) {
        return o.clamp(HARD_MIN_INTERVAL_HOURS, MAX_INTERVAL_HOURS);
    }
    match comic_type {
        // Webtoon rides with manhwa/manhua: same fast-serialised cadence, and
        // `content_type_word` already collapses Webtoon→"MANHWA" for the reader.
        ComicType::Manhwa | ComicType::Manhua | ComicType::Webtoon => {
            // Exact boundary: `Duration::num_days()` truncates toward zero, so
            // `num_days() <= 14` is really "< 15 days" and a series stayed on the 3h tier
            // for a whole extra day past the documented cutoff. Compare the durations
            // themselves so `DORMANT_AFTER_DAYS` means what it says: released **at or
            // within** 14 days is active, one second past 14 days is dormant.
            let active = last_release
                .map(|r| now.signed_duration_since(r) <= chrono::Duration::days(DORMANT_AFTER_DAYS))
                .unwrap_or(false);
            if active {
                TIER_FAST_ACTIVE_HOURS
            } else {
                TIER_FAST_DORMANT_HOURS
            }
        }
        ComicType::Manga | ComicType::Comic => TIER_MANGA_HOURS,
    }
}

/// Spread applied to every scheduled `next_scan_at`, as a fraction of the interval.
const SCHEDULE_JITTER_FRACTION: f64 = 0.10;

// ── Phase E instrumentation: is the TARGET actually being achieved? ──────────────
//
// §10 requires "achieved-vs-target drift < 10% per tier (proves the target isn't slipping)",
// and §7 Phase E names the failure it detects: with a hard target, achieved interval =
// target + queue delay, so "the target silently becomes a lie" when demand exceeds drain
// capacity. Nothing measured it (§8j), which means a tier slipping under load and a tier
// meeting its target produced identical output — the exact invisibility Phase E exists to
// remove.
//
// Deliberately in-process atomics, not a table: the sample is taken on the scan write path
// (~15k/day) where an extra query would be a real cost, and §8i already had to throttle a
// library-wide health check for running 5× too often on the 60 s reconcile beat. Counters
// reset on restart; this is an operational signal, not an audit trail.

/// The three Phase E tiers, in `TIER_DRIFT`'s index order.
const TIER_TARGETS: [f64; 3] = [
    TIER_FAST_ACTIVE_HOURS,
    TIER_MANGA_HOURS,
    TIER_FAST_DORMANT_HOURS,
];

/// Where a tier target sits in [`TIER_TARGETS`]. `None` for anything that is not one of the
/// three tiers — an admin override, or the legacy inferred-cadence path — because those have
/// no target to drift from.
fn tier_index(target_hours: f64) -> Option<usize> {
    TIER_TARGETS
        .iter()
        .position(|t| (t - target_hours).abs() < f64::EPSILON)
}

/// Per-tier achieved-interval accumulator: summed achieved seconds and a sample count.
/// Drained each time the drift is reported, so every report covers one window.
#[derive(Default)]
struct TierDrift {
    achieved_secs: AtomicU64,
    samples: AtomicU64,
}

static TIER_DRIFT: [std::sync::LazyLock<TierDrift>; 3] = [
    std::sync::LazyLock::new(TierDrift::default),
    std::sync::LazyLock::new(TierDrift::default),
    std::sync::LazyLock::new(TierDrift::default),
];

/// Drift beyond this fraction of the target is reported at `warn` (§10's "< 10%").
const TIER_DRIFT_WARN_FRACTION: f64 = 0.10;

/// Samples a tier needs in a window before its drift is worth reporting. Below this the mean
/// is dominated by individual outliers (one series that was parked, unparked and rescanned).
const TIER_DRIFT_MIN_SAMPLES: u64 = 20;

/// Floor on how often the drift is reported. Pure atomics, so the work is trivial — this
/// exists to keep the LOG readable at the 60 s reconcile beat, and to give each window enough
/// samples to clear `TIER_DRIFT_MIN_SAMPLES` at realistic scan rates.
const TIER_DRIFT_REPORT_MIN_SECONDS: u64 = 900;

/// Record one achieved interval against the tier that was targeted.
///
/// `achieved_hours` is wall-clock between this scan and the series' previous one, so it
/// includes every delay the schedule actually suffered: queue depth, tick granularity, the
/// jitter, and any drain backlog. That is the point — the target is what we asked for and
/// this is what we got.
///
/// Callers must exclude trigger-driven scans (see `record_scan_once`): E2/E5 set the due-now
/// sentinel, so a triggered scan legitimately arrives early and would report as a large
/// negative drift, hiding a real positive one in the mean.
fn record_tier_achieved(target_hours: f64, achieved_hours: f64) {
    if achieved_hours <= 0.0 {
        return;
    }
    let Some(i) = tier_index(target_hours) else {
        return;
    };
    TIER_DRIFT[i]
        .achieved_secs
        .fetch_add((achieved_hours * 3600.0) as u64, Ordering::Relaxed);
    TIER_DRIFT[i].samples.fetch_add(1, Ordering::Relaxed);
}

/// Log each tier's achieved-vs-target drift and drain the window.
///
/// The drain is `load` then `fetch_sub` of exactly what was read, not `swap(0)` — §8i's
/// correction: a concurrent sample landing between the read and the drain is carried into the
/// next window rather than discarded.
fn report_tier_drift() {
    for (i, &target) in TIER_TARGETS.iter().enumerate() {
        let samples = TIER_DRIFT[i].samples.load(Ordering::Relaxed);
        if samples < TIER_DRIFT_MIN_SAMPLES {
            continue;
        }
        let secs = TIER_DRIFT[i].achieved_secs.load(Ordering::Relaxed);
        TIER_DRIFT[i].samples.fetch_sub(samples, Ordering::Relaxed);
        TIER_DRIFT[i]
            .achieved_secs
            .fetch_sub(secs, Ordering::Relaxed);
        let achieved = secs as f64 / 3600.0 / samples as f64;
        let drift = (achieved - target) / target;
        if drift.abs() > TIER_DRIFT_WARN_FRACTION {
            tracing::warn!(
                target_hours = target,
                achieved_hours = achieved,
                drift_pct = drift * 100.0,
                samples,
                "scan: tier is not achieving its target cadence"
            );
        } else {
            tracing::info!(
                target_hours = target,
                achieved_hours = achieved,
                drift_pct = drift * 100.0,
                samples,
                "scan: tier cadence on target"
            );
        }
    }
}

/// Consecutive saturated ticks before the drain-capacity alert fires. One saturated batch is
/// an ordinary cold-start drain; two ticks in a row is a backlog that is not clearing.
const SATURATED_TICK_ALERT: u32 = 2;

/// Per-tier jitter fraction (Phase E). ±10% of a 3h target is a 36-minute window — 36× the
/// tick lag and enough to dominate every other source of variance — so the fast tier gets a
/// tighter ±5% (±9 min). Never zero on any tier: `jitter_interval_hours` records that
/// without spread a ~745-series cohort re-clustered every 35 min and drove 154 GB of egress.
fn tier_jitter_fraction(interval_hours: f64) -> f64 {
    if interval_hours <= TIER_FAST_ACTIVE_HOURS {
        0.05
    } else {
        SCHEDULE_JITTER_FRACTION
    }
}

/// Apply ±`SCHEDULE_JITTER_FRACTION` of random spread to a scheduling interval.
///
/// Without this, `next_scan_at = now + interval` is fully deterministic, so any set
/// of series that once became due together stays together forever: they are scanned
/// in the same batch, rescheduled by the same delta, and re-cluster on the next
/// cycle. Production showed exactly that — a self-sustaining cohort of ~745 series
/// arriving every 35 minutes (01:51 → 02:26 → 03:01 → 03:35 → …), which drove the
/// scanner to a 43% duty cycle and Suwayomi to 154 GB of egress, while the long tail
/// of the catalogue got whatever capacity was left.
///
/// The park path already jittered for this reason (audit #9); the far more frequent
/// steady and awaiting paths did not. Randomising each reschedule lets a cohort decay
/// into a smooth arrival rate over a few cycles.
#[cfg(test)]
fn jitter_interval_hours(hours: f64) -> f64 {
    jitter_interval_hours_with(hours, SCHEDULE_JITTER_FRACTION)
}

/// As `jitter_interval_hours`, with an explicit spread fraction so the Phase E fast tier can
/// use a tighter ±5% (see `tier_jitter_fraction`).
fn jitter_interval_hours_with(hours: f64, fraction: f64) -> f64 {
    if hours <= 0.0 {
        return hours;
    }
    // Off by default under test so the scheduling tests can assert exact cadences
    // (several of them advance the clock to precisely `now + interval` to make a
    // series due, which a +10% spread would silently push past). `jitter_interval_hours`
    // itself is covered by `jitter_stays_within_band_and_varies`, which opts in.
    #[cfg(test)]
    if !tests::jitter_enabled() {
        return hours;
    }
    // Uniform in [-1.0, 1.0), scaled by the jitter fraction.
    let unit = (rand_core::OsRng.next_u64() as f64 / u64::MAX as f64) * 2.0 - 1.0;
    (hours * (1.0 + unit * fraction)).max(0.0)
}

/// Default accelerated re-poll cadence (minutes) when the admin hasn't set
/// `poll_every_minutes`. Mirrors the API default surfaced in `map_series`.
const DEFAULT_POLL_MINUTES: f64 = 30.0;

/// Floor on the accelerated re-poll cadence. The scan loop itself ticks every
/// `SCAN_TICK_SECONDS` (3600s / 1h default), so a `poll_every_minutes` below the tick
/// cadence can't actually poll faster than the tick anyway — with the hourly default,
/// the tick is the effective floor for overdue re-checks; 15min stays a gentle floor
/// for deployments that lower the tick.
const MIN_POLL_MINUTES: f64 = 15.0;

/// Absolute ceiling on how long a series stays in the accelerated poll cadence
/// before falling back to the steady cadence (SC1). Without a bound, a
/// stalled-but-ONGOING series (never auto-paused) — or one whose inferred interval
/// underestimates its true cadence — would poll every `poll_every_minutes`
/// indefinitely. A chapter that's actually coming almost always lands within a
/// couple of days of its cadence; past that the series is treated as steady again.
/// The effective window is `clamp(publication_interval, MIN_INTERVAL_HOURS, this)`
/// — sized on the series' own PUBLICATION cadence, so a fast series doesn't poll
/// aggressively for many multiples of its own interval. (It used to be sized on the
/// steady *poll* interval, which now caps at 12h and would have collapsed every
/// window to 12h.)
const AWAITING_MAX_HOURS: f64 = 48.0;

/// Cool-down before a closed awaiting streak may re-open a fresh accelerated window.
///
/// `awaiting_since` used to be pinned to the ORIGINAL streak start forever — only an
/// actual new chapter cleared it — so once the window expired the series could never
/// re-accelerate. Live: 2,201 series awaiting, 1,679 (76%) awaiting for more than 48h
/// and therefore permanently de-accelerated. But simply clearing the field when the
/// window closes is worse: the very next steady scan finds the series still late,
/// re-opens the window, and the duty cycle becomes ~80% — several thousand series at a
/// 30-minute poll, which is exactly the runaway this scheduler already suffered from.
///
/// So the field is retained as a cool-down marker: accelerate for
/// `min(publication_interval, AWAITING_MAX_HOURS)`, run at the steady (<= 12h) cadence
/// for the remainder of `AWAITING_REARM_HOURS`, then re-arm. With 48h windows that is a
/// 20% duty cycle. The harm the old behaviour caused — "poll hard for 2 days, then go
/// blind for 2 weeks" — is gone regardless, because the post-window cadence is now
/// bounded by `ACTIVE_MAX_INTERVAL_HOURS` rather than by the publication gap.
const AWAITING_REARM_HOURS: f64 = 240.0; // 10 days

/// How far past its own publication cadence a series' newest upload must be before the
/// next chapter counts as genuinely LATE (and the accelerated poll is worth paying for).
const LATE_FACTOR: f64 = 1.25;

/// Upper edge of the lateness band, as a multiple of the publication cadence. Past this
/// the series has not "got a late chapter", it has stopped publishing — accelerating it
/// buys nothing and it is left on the steady cadence (where a resumption is still picked
/// up within `ACTIVE_MAX_INTERVAL_HOURS`). Without this bound every dead-but-ONGOING
/// series in the catalogue would sit in the accelerated poll forever.
const LATE_BAND_MAX: f64 = 3.0;

/// Minimum number of inter-upload GAPS (i.e. dated chapters minus one) before the
/// inferred rolling average is trusted to judge lateness.
///
/// This is the root cause of the absurd stored averages: a series with 2–5 cached
/// chapters whose upload dates span years yields an "average" of 58,309 hours (6.6
/// years) — which parked "The Skeleton Soldier Failed to Defend the Dungeon" until
/// 2033-03-14. Four gaps is cheap to satisfy for a real series and excludes the sparse
/// histories entirely. 4,881 of the 6,392 live non-paused series clear it.
const MIN_TRUSTED_GAPS: usize = 4;

/// Resolve the accelerated re-poll cadence (minutes) for an *awaiting* series —
/// one that's overdue for a new chapter that hasn't landed yet (SC1). Clamped to
/// at least `MIN_POLL_MINUTES` and never above the steady interval (a poll slower
/// than the steady cadence would be no acceleration at all).
fn resolve_poll_minutes(poll_every_minutes: Option<i64>, steady_interval_hours: f64) -> f64 {
    let requested = poll_every_minutes
        .filter(|v| *v > 0)
        .map(|v| v as f64)
        .unwrap_or(DEFAULT_POLL_MINUTES);
    let steady_minutes = steady_interval_hours * 60.0;
    // `max` then `min` (not `clamp`) so a degenerate steady_minutes < MIN can't
    // panic; it just collapses the poll cadence onto the steady one.
    requested.max(MIN_POLL_MINUTES).min(steady_minutes)
}

/// Persisted scan state row (mirrors `series_scan_state`).
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct ScanState {
    pub avg_interval_hours: f64,
    pub known_chapter_count: i64,
    /// Highest chapter number seen at the last scan. `None` = not yet observed
    /// (pre-`0012` rows), which suppresses number-based new-chapter detection
    /// until a baseline is recorded (SC4).
    pub known_max_chapter: Option<f64>,
    pub last_scanned_at: Option<String>,
    pub next_scan_at: Option<String>,
    pub last_new_chapter_at: Option<String>,
    /// When the current overdue-awaiting streak began (SC1). `None` = not awaiting
    /// (a chapter is on schedule). Bounds how long the accelerated poll runs.
    pub awaiting_since: Option<String>,
    /// Snapshot of the chapter identity set at the last scan (sorted, comma-joined
    /// chapter ids). Drives set-difference new-chapter detection so a removal that
    /// offsets an insertion at/below the current max is still flagged. `None` =
    /// not yet captured (pre-migration row / first observation), which seeds a
    /// baseline without flagging.
    pub known_chapter_ids: Option<String>,
}

/// The column list for a `ScanState` row, shared by `scan_state` (pooled read)
/// and `record_scan`'s in-transaction read so the two can't drift out of sync
/// with the struct.
const SCAN_STATE_SELECT: &str =
    "SELECT avg_interval_hours, known_chapter_count, known_max_chapter, \
     last_scanned_at, next_scan_at, last_new_chapter_at, awaiting_since, known_chapter_ids \
     FROM series_scan_state WHERE series_id = ?";

/// Read the persisted scan state for a series, if any. The DB-driven scheduler selects
/// due ids directly and `record_scan` re-reads state in its own transaction, so this
/// standalone pooled read is now only used by tests.
#[cfg(test)]
pub async fn scan_state(pool: &SqlitePool, series_id: &str) -> Option<ScanState> {
    sqlx::query_as::<_, ScanState>(SCAN_STATE_SELECT)
        .bind(series_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// Admin overrides the scanner cares about (interval + poll cadence + pause),
/// read from `series_admin`.
#[derive(Default, sqlx::FromRow)]
struct ScanAdmin {
    override_interval_hours: Option<f64>,
    /// Accelerated re-poll cadence (minutes) used once a series is overdue for a
    /// new chapter that hasn't landed yet (SC1). `None` -> `DEFAULT_POLL_MINUTES`.
    poll_every_minutes: Option<i64>,
    paused_override: Option<i64>,
    status_override: Option<String>,
}

async fn scan_admin(pool: &SqlitePool, series_id: &str) -> ScanAdmin {
    sqlx::query_as::<_, ScanAdmin>(
        "SELECT override_interval_hours, poll_every_minutes, paused_override, status_override \
         FROM series_admin WHERE series_id = ?",
    )
    .bind(series_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

/// Coerce a Suwayomi epoch timestamp (seconds or millis, as a string) to millis.
///
/// Delegates to `series_cache::normalize_epoch_millis` so the scheduler reads
/// `upload_date` through exactly the same lens that `latest_chapter_at` is derived
/// with — one definition of "what this dirty column means", rather than two that can
/// drift. Concretely this adds the far-future guard the local copy lacked: one live
/// chapter row carries `57766321270698` (~year 3800), and feeding that to
/// `upload_cadence` yields a `latest_upload` in the far future, so
/// `is_genuinely_late` sees a negative age and the series can never be flagged late.
fn epoch_millis(v: Option<&str>) -> Option<i64> {
    crate::series_cache::normalize_epoch_millis(v?)
}

/// Compute the rolling average interval (hours) between chapter uploads.
///
/// Sorts upload timestamps descending, diffs consecutive pairs, and averages them.
/// Garbage/missing timestamps are dropped. Returns `None` when fewer than two usable
/// timestamps exist (no cadence can be inferred).
/// Now a thin projection of `upload_cadence`, which the scheduler uses directly (it also
/// needs the gap count and the newest upload). Kept for the cadence assertions.
#[cfg(test)]
pub fn avg_interval_hours(chapters: &[SuwayomiChapter]) -> Option<f64> {
    upload_cadence(chapters).map(|c| c.avg_hours)
}

/// What a chapter list says about a series' publication rhythm: the rolling average
/// gap, how many usable gaps it was averaged over, and when the newest chapter landed.
///
/// The gap COUNT is the part the scheduler cares about beyond the average itself — an
/// "average" derived from one or two gaps spanning years is noise, and treating it as a
/// cadence is what produced multi-year `avg_interval_hours` values in production.
struct UploadCadence {
    avg_hours: f64,
    gaps: usize,
    latest_upload: DateTime<Utc>,
}

/// Compute `UploadCadence` from a chapter list, or `None` when fewer than two usable
/// upload timestamps exist (no rhythm can be inferred).
fn upload_cadence(chapters: &[SuwayomiChapter]) -> Option<UploadCadence> {
    let mut ts: Vec<i64> = chapters
        .iter()
        .filter_map(|c| epoch_millis(c.upload_date.as_deref()))
        .collect();
    if ts.len() < 2 {
        return None;
    }
    ts.sort_unstable_by(|a, b| b.cmp(a)); // desc
    let mut total_ms: i64 = 0;
    let mut gaps: usize = 0;
    for pair in ts.windows(2) {
        let diff = pair[0] - pair[1]; // newer - older >= 0
        if diff > 0 {
            total_ms += diff;
            gaps += 1;
        }
    }
    if gaps == 0 {
        return None;
    }
    let avg_ms = total_ms as f64 / gaps as f64;
    Some(UploadCadence {
        avg_hours: avg_ms / 3_600_000.0,
        gaps,
        latest_upload: DateTime::from_timestamp_millis(ts[0])?,
    })
}

/// When the newest chapter in the list was uploaded, independent of whether a *cadence*
/// can be inferred from it.
///
/// Phase E's dormancy test needs "when did this series last release?", which is a strictly
/// weaker question than `upload_cadence`'s "what rhythm does it publish on?". Taking it
/// from `UploadCadence::latest_upload` inherited that function's `ts.len() >= 2` / `gaps > 0`
/// preconditions, so a series with **one** dated chapter — a brand-new manhwa, exactly the
/// cohort the 3h tier exists for — reported "no last release" and was filed as DORMANT on
/// the 24h tier. Same for a series whose chapters all share one upload timestamp (a bulk
/// import), which yields zero gaps.
///
/// Identical to `UploadCadence::latest_upload` whenever that exists (both are the maximum
/// of the same parsed timestamps), so this only ever widens the input.
fn newest_upload(chapters: &[SuwayomiChapter]) -> Option<DateTime<Utc>> {
    chapters
        .iter()
        .filter_map(|c| epoch_millis(c.upload_date.as_deref()))
        .max()
        .and_then(DateTime::from_timestamp_millis)
}

/// Is the next chapter genuinely LATE?
///
/// True only inside the band `(LATE_FACTOR, LATE_BAND_MAX] x publication_interval` past
/// the newest upload: below it the chapter isn't due yet, above it the series has
/// stopped publishing rather than slipped. This replaces the old test, which was
/// `is_due(next_scan_at, now)` — always true for a scheduler-driven scan, since
/// `due_series_ids` only returns rows that are already due — so "awaiting" meant nothing
/// more than "a scheduled scan found nothing new", the ordinary case for 2,009 of the
/// 2,127 series scanned in a live 24h window.
fn is_genuinely_late(publication_hours: f64, hours_since_latest_upload: f64) -> bool {
    hours_since_latest_upload > publication_hours * LATE_FACTOR
        && hours_since_latest_upload <= publication_hours * LATE_BAND_MAX
}

/// The latest (highest) chapter number seen, or `None` for an empty list.
/// Test-only helper for the SC4 new-chapter-detection assertions.
#[cfg(test)]
fn latest_number(chapters: &[SuwayomiChapter]) -> Option<f64> {
    chapters
        .iter()
        .map(|c| c.chapter_number)
        .fold(None, |acc, n| Some(acc.map_or(n, |a: f64| a.max(n))))
}

/// The highest *real* chapter number in the current list, dropping obvious
/// sentinels/outliers — Suwayomi's `-1.0` "unnumbered" marker, NaN, and absurdly
/// large values. Unlike a monotonic all-time high-water mark, this is derived
/// from the CURRENT list, so `known_max_chapter` self-heals once a garbage number
/// disappears upstream instead of being pinned forever (SC4).
fn robust_max_number(chapters: &[SuwayomiChapter]) -> Option<f64> {
    chapters
        .iter()
        .map(|c| c.chapter_number)
        .filter(|n| n.is_finite() && *n >= 0.0 && *n < 100_000.0)
        .fold(None, |acc, n| Some(acc.map_or(n, |a: f64| a.max(n))))
}

/// Encode a set of chapter ids as a stable, sorted, comma-joined string for the
/// `known_chapter_ids` snapshot (SC set-diff detection).
fn encode_id_set(ids: &std::collections::HashSet<i64>) -> String {
    let mut v: Vec<i64> = ids.iter().copied().collect();
    v.sort_unstable();
    v.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse a `known_chapter_ids` snapshot back into a set.
fn parse_id_set(s: &str) -> std::collections::HashSet<i64> {
    s.split(',')
        .filter_map(|t| t.trim().parse::<i64>().ok())
        .collect()
}

fn parse_iso(v: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v?)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Decide whether a series is due for a scan by gating on its persisted
/// `next_scan_at` (SC1/SC7). A series with no scheduled scan (never scanned, or a
/// pre-existing row without one) is always due. Gating and the admin console's
/// "next due" now read the same stored value, so they can't disagree.
fn is_due(next_scan_at: Option<&str>, now: DateTime<Utc>) -> bool {
    match parse_iso(next_scan_at) {
        None => true,
        Some(next) => now >= next,
    }
}

/// Resolve the effective status for a series, honoring an admin status override.
fn effective_status(m: &SuwayomiManga, admin: &ScanAdmin) -> SeriesStatus {
    admin
        .status_override
        .as_deref()
        .and_then(komika_status)
        .unwrap_or_else(|| status_from(&m.status))
}

/// Resolve whether the series is paused: forced admin override wins, else auto by status.
fn is_paused(status: SeriesStatus, admin: &ScanAdmin) -> bool {
    admin
        .paused_override
        .map(|v| v != 0)
        .unwrap_or_else(|| paused_for_status(status))
}

/// Best-effort: record each installed extension's coordinates against its source id
/// (§2.1), so a native device can install the exact extension a `source_series` came
/// from. Runs once per tick and is fully non-fatal — any failure (Suwayomi down, an
/// upstream schema mismatch, a write error) is logged and swallowed so it never
/// affects the scan. Additive: it only writes `source_extension` rows. Called from the
/// daily source-sync reconcile (extensions change rarely, so a per-scan-tick refresh was
/// wasteful once the scan tick stopped touching Suwayomi).
pub(crate) async fn record_source_extensions(state: &AppState) {
    let extensions = match state.suwayomi.fetch_extensions().await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "scan tick: failed to fetch Suwayomi extensions");
            return;
        }
    };
    for ext in extensions {
        // An extension with no repo can't be installed from coordinates; skip it.
        let Some(repo_url) = ext.repo.clone() else {
            continue;
        };
        let input = crate::catalog::SourceExtensionInput {
            pkg_name: ext.pkg_name.clone(),
            repo_url,
            apk_name: ext.apk_name.clone(),
            version_code: ext.version_code,
            lang: ext.lang.clone(),
            is_nsfw: ext.is_nsfw,
        };
        for source_id in &ext.source_ids {
            if let Err(e) =
                crate::catalog::upsert_source_extension(&state.pool, source_id, &input).await
            {
                tracing::warn!(source_id, error = %e, "scan tick: failed to record source extension");
            }
        }
    }
}

/// Max due series scanned in a single tick — bounds a cold-start/backlog tick so it
/// can't run unbounded; the remainder drains on later ticks (ordered oldest-due first,
/// so nothing starves).
///
/// Lowered from 5,000 (P1-2): at the measured ~90–300 scans per 300s tick a 5,000-row
/// batch runs for hours, and every series in it was being rescheduled off the SINGLE
/// `Utc::now()` captured when the batch was selected — so anything processed more than
/// its own interval after the batch started got a `next_scan_at` already in the past,
/// was immediately re-selected, and the drain never converged. `scan_due` now takes its
/// own timestamp (see `tick`), and a smaller batch keeps the due-ordering fresh so the
/// oldest-due rows genuinely go first after downtime.
const DUE_BATCH_LIMIT: i64 = 1000;

/// Anything scheduled further out than this could not have been written by this module:
/// a non-paused series caps at `ACTIVE_MAX_INTERVAL_HOURS` (+10% jitter) and a paused one
/// at `PAUSED_PARK_HOURS` (+`PAUSED_PARK_HOURS/10` jitter) — the true ceiling across every
/// writer is `MAX_SCHEDULED_HOURS`, currently 369 h. Rows beyond it
/// are legacy, written under the old ~100-year ceiling before `MAX_INTERVAL_HOURS`
/// landed; they can never satisfy `next_scan_at <= now` and are permanently invisible to
/// `due_series_ids`. Migration 0057 clamps the existing ones; this horizon is the
/// read-side net that keeps the class extinct without another migration.
///
/// **THE PARK-BELOW-HORIZON INVARIANT.** Every writer's worst case — *including its
/// jitter* — must land strictly below this, or `reclaim_absurd_schedules` drags each newly
/// written row straight back and the schedule silently does nothing. This is not
/// hypothetical: §8a records that raising `PAUSED_PARK_HOURS` to 60 days without moving
/// this constant would have defeated the park entirely, and the same class of mistake
/// produced the series parked until 2033-03-14. The invariant is now MECHANICAL — see
/// `MAX_SCHEDULED_HOURS` and the `const _` assertion below it, which fail the BUILD rather
/// than the deploy — and is re-checked against the live RNG by
/// `every_park_writer_stays_below_the_absurd_horizon`.
const ABSURD_HORIZON_HOURS: i64 = 16 * 24; // 16 days

/// Width of the uniform jitter window `park_paused` draws from, in hours. Centred on
/// `PAUSED_PARK_HOURS`, so the spread is ±`PAUSED_PARK_HOURS/10`.
///
/// NEVER ZERO (handoff constraint 3): with no spread a cohort parked in one drain comes
/// back due in one instant a fortnight later — the ~745-series/35-minute herd that drove a
/// 43% duty cycle and 154 GB of Suwayomi egress. The `.max(1)` floor is what guarantees a
/// non-degenerate `% spread`, and `park_jitter_is_never_degenerate` pins it.
const PARK_JITTER_SPREAD_HOURS: i64 = if PAUSED_PARK_HOURS / 5 > 1 {
    PAUSED_PARK_HOURS / 5
} else {
    1
};

/// Worst case of `park_paused`: the base park plus the top of its jitter window.
/// `x % spread` yields `[0, spread)`, so the offset is `PAUSED_PARK_HOURS + [-spread/2,
/// spread-1-spread/2]`. Today: 336 + 33 = **369 h**.
const MAX_PARKED_HOURS: i64 =
    PAUSED_PARK_HOURS + (PARK_JITTER_SPREAD_HOURS - 1) - PARK_JITTER_SPREAD_HOURS / 2;

/// Worst case of `park_source_series` (E4.3's outage park), in hours. That writer's jitter
/// is one-sided — `base + ABS(RANDOM()) % spread`, spread = base/5 — so it overshoots by up
/// to +20%, not ±10%. Today: 168 + 33 = **201 h**.
const MAX_OUTAGE_PARKED_HOURS: i64 = SOURCE_OUTAGE_PARK_HOURS + SOURCE_OUTAGE_PARK_HOURS / 5;

/// Worst case of the ordinary (non-park) scheduling paths. `record_scan` /
/// `record_scan_policy` clamp any interval to `MAX_INTERVAL_HOURS` — an admin override is
/// the only way to reach it — and then apply at most +`SCHEDULE_JITTER_FRACTION`.
/// Today: 336 × 1.1 = **369.6 h**.
const MAX_SCHEDULED_INTERVAL_HOURS: f64 = MAX_INTERVAL_HOURS * (1.0 + SCHEDULE_JITTER_FRACTION);

/// The single number the horizon must dominate: the furthest out ANY writer in this module
/// can schedule a row.
const MAX_SCHEDULED_HOURS: i64 = {
    let a = if MAX_PARKED_HOURS > MAX_OUTAGE_PARKED_HOURS {
        MAX_PARKED_HOURS
    } else {
        MAX_OUTAGE_PARKED_HOURS
    };
    // `MAX_SCHEDULED_INTERVAL_HOURS` is f64; round up to the next whole hour.
    let b = MAX_SCHEDULED_INTERVAL_HOURS as i64 + 1;
    if a > b {
        a
    } else {
        b
    }
};

/// **The invariant, enforced at COMPILE time.** Raise `PAUSED_PARK_HOURS` (or
/// `MAX_INTERVAL_HOURS`, or `SOURCE_OUTAGE_PARK_HOURS`, or any jitter fraction) without
/// raising `ABSURD_HORIZON_HOURS` to match and this stops the build. Nothing guarded it
/// before; §8a is what it costs when it is only guarded by a doc comment.
const _: () = assert!(
    MAX_SCHEDULED_HOURS < ABSURD_HORIZON_HOURS,
    "park-below-horizon invariant violated: a scheduled row would exceed \
     ABSURD_HORIZON_HOURS and reclaim_absurd_schedules would drag it straight back, \
     silently defeating the park (see §8a, the 2033-parking bug)"
);

/// How many absurd-horizon rows to reclaim per tick. Deliberately small: reclaiming is
/// pure upside but it adds unscheduled work, so it trickles (25/tick x 288 ticks/day
/// clears the live 3,578-row backlog in about half a day) instead of dumping thousands
/// of series into one due-set.
const ABSURD_RECLAIM_PER_TICK: i64 = 25;
/// Short breather between back-to-back drain batches, so a continuous backlog drain
/// doesn't peg the CPU/DB in a tight loop between 5k-series batches.
const DRAIN_BATCH_DELAY_MS: u64 = 250;
/// How far out a paused (COMPLETED/HIATUS/CANCELLED or admin-paused) series is parked so
/// it drops out of the frequent due-set — the whole point of pause. It's still
/// re-evaluated this often, so an upstream status flip / cleared override eventually
/// heals on its own without a full-library sweep. (Admin unpause resets `next_scan_at`
/// for promptness — see `set_series_paused` / `update_series_admin`.)
///
/// STAYS AT 14 DAYS. Two other invariants are load-bearing on this value:
///
///   * `ABSURD_HORIZON_HOURS` (16 days) is the read-side net that pulls legacy far-future
///     rows back into the due-set — production had 3,578 of them, one parked until 2033.
///     A 60-day park sits PAST that horizon, so `reclaim_absurd_schedules` would drag every
///     newly-parked series straight back, defeating the park entirely. That coupling is now
///     enforced by `MAX_SCHEDULED_HOURS`'s `const _` assertion, so the two constants can
///     only ever move together.
///   * `MAX_INTERVAL_HOURS` is defined AS this constant, so raising it would also raise the
///     ceiling `record_scan` clamps an inferred cadence to — re-admitting the 58,309-hour
///     "averages" that migration 0057 exists to have cleaned up.
///
/// §7 E2's proposed 60-day backstop was RE-EXAMINED against the post-E5 build and is
/// deliberately NOT applied. E2 specified 60 days as an improvement over *never polling*
/// paused series; as built every paused series parks here at 14 days, which is strictly
/// better coverage than the backstop it would be replaced by. What is left is a cost
/// argument, and it is weak: 7,601 paused Suwayomi series / 14 d = ~543 fetches/day, of
/// which **6,753 (89%) are `all.mangadex`** rows that Phase F retires outright. Phase F
/// therefore takes the park bill to ~61/day for free, versus the ~416/day (2.4% of the
/// ~17k/day budget) a 60-day park would save while quadrupling the safety-net latency of
/// the 96 series — athreascans (94) and suryascans (2) — that sit on UNsubscribed sources
/// and so receive no E2/E5 LATEST trigger at all. Measured on the 2026-07-31 snapshot.
const PAUSED_PARK_HOURS: i64 = 24 * 14; // 14 days

/// Base delay (minutes) for the first failed scan's error-backoff. Subsequent
/// consecutive failures double this (30m, 1h, 2h, …) up to `ERROR_BACKOFF_MAX_HOURS`,
/// so a permanently-failing series (deleted upstream, 404) leaves the hot front of the
/// due-set instead of being retried every tick and starving healthy series (audit #4).
const ERROR_BACKOFF_BASE_MINUTES: i64 = 30;

/// How many times to re-run the whole `record_scan` transaction when it fails with a
/// transient SQLite lock. Mirrors `mangadex::UPSERT_LOCK_RETRIES` — the same
/// contention, from the other side of the single writer.
const SCAN_LOCK_RETRIES: u32 = 4;
/// Cap on the error-backoff so a permanently-failing series stops growing its delay and
/// settles into a daily re-check (it might come back). Independent of `PAUSED_PARK_HOURS`:
/// a dead/erroring id is retried ~daily, more often than a genuinely paused series' 14-day
/// park, because a fetch error (unlike a clean paused status) may be transient.
const ERROR_BACKOFF_MAX_HOURS: i64 = 24;

/// "Due now" sentinel for `next_scan_at`. A never-scanned / freshly-enrolled series is
/// stored with this far-past timestamp instead of NULL so the due-query can be a single
/// bounded `next_scan_at <= ?` range seek (index-early-terminating, O(due)) rather than an
/// `IS NULL OR <=` full index scan. It sorts before every real timestamp (RFC3339 string
/// order) so due-now rows come first. COMPLETENESS INVARIANT: every enrolled series must
/// have a NON-NULL `next_scan_at` — a stray NULL would never match `<= ?` and the series
/// would silently never scan. Every writer here (and `catalog::backfill_pending_scan_states`)
/// therefore writes a real time or this sentinel; migration 0048 backfilled legacy NULLs.
pub const DUE_NOW_SENTINEL: &str = "1970-01-01T00:00:00+00:00";

/// Outcome of one source tick: enough to drive the drain loop and honest health reporting.
/// (The `series_scan_state` row count for the health "library size" is now set once per
/// reconcile by the supervisor, not per tick — see `run_source_loop` / `supervisor`.)
struct TickOutcome {
    /// Rows the due-query selected this batch (== `DUE_BATCH_LIMIT` ⇒ likely more waiting).
    due: usize,
    /// Scans that completed and advanced `next_scan_at` (progress signal for the drain).
    ok: usize,
    /// Scans that errored and were backed off.
    failed: usize,
}

/// Due series ids from the DB (uses `idx_scan_state_next_scan`). A single bounded
/// `next_scan_at <= ?` range seek: the index supplies the order (`ORDER BY next_scan_at
/// ASC`, no temp-b-tree sort) AND terminates the scan at the first future-dated row, so
/// future rows are never visited — cost is O(due), not O(catalogue). Never-scanned /
/// freshly-enrolled rows carry the far-past `DUE_NOW_SENTINEL` (not NULL), so they sort
/// first as due-now, then oldest-scheduled first — a cold-start backlog drains fairly
/// across ticks. See `DUE_NOW_SENTINEL` for the completeness invariant this relies on.
///
/// Library-wide variant, retained as the scheduling tests' oracle; production sharded onto
/// `due_series_ids_for_source` in Phase E3.
#[cfg(test)]
async fn due_series_ids(pool: &SqlitePool, now_iso: &str, limit: i64) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT series_id FROM series_scan_state \
         WHERE next_scan_at <= ? \
         ORDER BY next_scan_at ASC LIMIT ?",
    )
    .bind(now_iso)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "scan: due-series query failed");
        Vec::new()
    })
}

/// Pull a few rows scheduled past `ABSURD_HORIZON_HOURS` back into the next hour.
///
/// No writer in this module can produce such a row (see `ABSURD_HORIZON_HOURS`), so any
/// that exist are legacy rows parked years out and permanently invisible to the
/// due-query — production had 3,578 of them, 1,676 on ONGOING series, with a maximum of
/// 2033-03-14. Bounded by `ABSURD_RECLAIM_PER_TICK`, index-backed (a `next_scan_at > ?`
/// range seek off `idx_scan_state_next_scan`), and self-extinguishing: once the tail is
/// drained the subselect matches nothing and this is a no-op. Non-fatal — a failure is
/// logged and the tick proceeds. Returns how many rows were reclaimed.
async fn reclaim_absurd_schedules(pool: &SqlitePool, now: DateTime<Utc>) -> u64 {
    let horizon = (now + chrono::Duration::hours(ABSURD_HORIZON_HOURS)).to_rfc3339();
    // Spread over the next hour so a reclaimed cohort doesn't arrive as one herd.
    let res = sqlx::query(
        "UPDATE series_scan_state \
            SET next_scan_at = strftime('%Y-%m-%dT%H:%M:%S', 'now', '+' || (ABS(RANDOM()) % 60) || ' minutes') || '+00:00', \
                updated_at   = strftime('%Y-%m-%dT%H:%M:%S', 'now') || '+00:00' \
          WHERE series_id IN ( \
            SELECT series_id FROM series_scan_state \
             WHERE next_scan_at > ? ORDER BY next_scan_at DESC LIMIT ?)",
    )
    .bind(&horizon)
    .bind(ABSURD_RECLAIM_PER_TICK)
    .execute(pool)
    .await;
    match res {
        Ok(r) => {
            let n = r.rows_affected();
            if n > 0 {
                tracing::info!(
                    reclaimed = n,
                    horizon_hours = ABSURD_HORIZON_HOURS,
                    "scan: pulled legacy far-future schedules back into the due-set"
                );
            }
            n
        }
        Err(e) => {
            tracing::warn!(error = %e, "scan: absurd-schedule reclaim failed");
            0
        }
    }
}

/// Count of tracked series — the health snapshot's "library size".
async fn scan_state_count(pool: &SqlitePool) -> usize {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM series_scan_state")
        .fetch_one(pool)
        .await
        .unwrap_or(0) as usize
}

/// Scheduler entry point: scan ONE due series by id.
///
/// A single combined upstream fetch (fresh status + chapters), then record the scan —
/// *unconditionally*, even for a paused series. Scanning first is deliberate: it
/// baselines a never-observed series so even a COMPLETED/HIATUS series gets a real
/// chapter count (a paused series that had never been scanned otherwise shows 0 chapters
/// forever), and it refreshes status so an upstream *reopen* (COMPLETED → ONGOING)
/// auto-resumes scanning without waiting for an admin. If the series is *still* paused
/// afterwards, it is PARKED far out — overriding the steady next-scan `record_scan` just
/// set — so it leaves the frequent due-set. Net cost of a steady paused series is thus
/// one fetch per park window (~14d), which also catches the rare late chapter on a
/// "completed" series. (Zero-cost "never fetch paused" was rejected: it reintroduces the
/// 0-chapter bug and loses reopen detection the old live-`library()` sweep had.)
async fn scan_due(state: &AppState, series_id: &str, now: DateTime<Utc>) -> anyhow::Result<bool> {
    let Ok(id) = series_id.parse::<i64>() else {
        // A non-numeric id can't be a Suwayomi series; park it so it stops recurring.
        park_paused(&state.pool, series_id, now).await?;
        return Ok(false);
    };
    let admin = scan_admin(&state.pool, series_id).await;
    // ONE combined fetch for fresh status + chapters (falls back to two calls internally
    // on an older engine). On failure, back the series off (exponential, capped) so a
    // deleted/404'ing id or a transient outage doesn't leave it pinned at the front of
    // the due-ordering, re-tried every tick and starving healthy series (audit #1/#4).
    let (m, chapters, provenance) =
        match state.suwayomi.series_and_chapters_with_provenance(id).await {
            Ok(v) => v,
            Err(e) => {
                if let Err(be) =
                    record_scan_failure(&state.pool, series_id, now, FailureKind::FetchError).await
                {
                    tracing::warn!(series_id, error = %be, "scan: failed to record backoff");
                }
                return Err(e);
            }
        };
    // E4.2 — A CACHED FALLBACK IS NOT A SUCCESSFUL SCAN (F11).
    //
    // `series_and_chapters_with_provenance` returns `Ok` even when the upstream fetch
    // failed, because the client then reads Suwayomi's local DB (correct for the reader,
    // wrong here — see `suwayomi::Provenance`). Persisting it would write down a scan that
    // never happened: `last_scanned_at` advanced, `next_scan_at` pushed out on the normal
    // cadence, `consecutive_failures` reset to 0, and `new_found = false` recorded as
    // "confirmed no new chapters" when the truth is "we did not look". That is precisely
    // how 209 Genz Toons series sat frozen at 0 chapters while reporting perfect health.
    //
    // So: back it off like any other upstream failure and return `Err` BEFORE `persist_scan`
    // — the fetch produced no new information, and the cache it fell back to is by
    // definition already in our DB. The tick logs it and counts it as failed, and
    // `check_source_health` aggregates the streak into one per-source alert.
    if provenance.is_cached_fallback() {
        if let Err(be) =
            record_scan_failure(&state.pool, series_id, now, FailureKind::CachedFallback).await
        {
            tracing::warn!(series_id, error = %be, "scan: failed to record backoff");
        }
        return Err(anyhow::anyhow!(
            "suwayomi served CACHED chapters for series {series_id}: the upstream fetch failed, \
             so this scan proves nothing about new chapters"
        ));
    }
    let found = match persist_scan(state, &m, &chapters, &admin, now).await {
        Ok(v) => v,
        Err(e) => {
            // A LOCAL write-lock failure is not an upstream failure. Routing it into
            // `record_scan_failure` bumped `consecutive_failures` and pushed
            // `next_scan_at` out 30m → 1h → 2h, punishing a perfectly healthy series
            // for losing a race against our own background writers. Observed on
            // series 207 and 284, which sat at consecutive_failures = 1 purely from
            // lock contention. The fetch above already succeeded, so leave the
            // schedule untouched and let the next tick retry at the normal cadence.
            if crate::db::is_locked_error(&e) {
                tracing::warn!(
                    series_id,
                    error = %e,
                    "scan: persist lost the write race after retries; NOT counting as an upstream failure"
                );
                return Err(e);
            }
            if let Err(be) =
                record_scan_failure(&state.pool, series_id, now, FailureKind::PersistError).await
            {
                tracing::warn!(series_id, error = %be, "scan: failed to record backoff");
            }
            return Err(e);
        }
    };
    // Park AFTER the scan (overriding the steady cadence `record_scan` set) so a paused
    // series drops out of the frequent due-set but was still baselined + status-checked.
    if is_paused(effective_status(&m, &admin), &admin) {
        park_paused(&state.pool, series_id, now).await?;
    }
    Ok(found)
}

/// Why a scan failed, persisted to `series_scan_state.last_failure_kind` (migration 0072).
///
/// `consecutive_failures` counts strikes but cannot say what kind, and the distinction is
/// the whole of E4: a source whose fetches error outright is visibly broken, whereas one
/// quietly served from Suwayomi's local cache looks perfectly healthy. Aggregating this
/// per source is what turns 209 silent per-series non-events into one loud per-source
/// alert — see `catalog::source_scan_health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The upstream fetch failed outright and there was nothing to fall back to.
    FetchError,
    /// The upstream fetch failed and Suwayomi answered from its LOCAL database instead —
    /// the F11 case. The data may be stale or empty; either way it cannot contain a
    /// chapter we have not already seen, so it proves nothing about new chapters.
    CachedFallback,
    /// Fetch succeeded, but persisting the result failed for a non-lock reason. Recorded
    /// distinctly because it is OUR bug, not the source's, and must not read as a source
    /// outage in the per-source health view.
    PersistError,
}

impl FailureKind {
    /// Stable identifier for the DB column. Kept short and snake_case; nothing parses it
    /// back into the enum, so adding a variant cannot break existing rows.
    fn as_str(self) -> &'static str {
        match self {
            Self::FetchError => "fetch_error",
            Self::CachedFallback => "cached_fallback",
            Self::PersistError => "persist_error",
        }
    }
}

/// Record a failed scan: bump `consecutive_failures` and push `next_scan_at` out with an
/// exponential, capped backoff (30m, 1h, 2h, … up to `ERROR_BACKOFF_MAX_HOURS`). Upsert so
/// a never-scanned series with no row yet is still backed off. A successful `record_scan`
/// resets `consecutive_failures` to 0. This is what keeps a permanently-failing series from
/// starving every healthy series behind it in the due-ordering (audit #1/#4).
///
/// `kind` is persisted alongside the counter (migration 0072) so the per-source health view
/// can tell a source that ERRORS from one that silently serves cache — see [`FailureKind`].
///
/// Read-modify-write, so it runs inside `BEGIN IMMEDIATE` with the same whole-transaction
/// retry as `record_scan` (P2-1). It was the one scan-state writer left on a bare pooled
/// read + upsert: harmless in practice (an interleaved writer could only lose a failure
/// increment), but there is no reason for it to be the odd one out, and the IMMEDIATE
/// lock is what lets `busy_timeout` absorb contention instead of returning BUSY_SNAPSHOT.
async fn record_scan_failure(
    pool: &SqlitePool,
    series_id: &str,
    now: DateTime<Utc>,
    kind: FailureKind,
) -> anyhow::Result<()> {
    let mut delay_ms = 50_u64;
    for attempt in 0..=SCAN_LOCK_RETRIES {
        match record_scan_failure_once(pool, series_id, now, kind).await {
            Ok(()) => return Ok(()),
            Err(e) if crate::db::is_locked_error(&e) && attempt < SCAN_LOCK_RETRIES => {
                tracing::debug!(
                    series_id,
                    attempt = attempt + 1,
                    delay_ms,
                    "scan: backoff write lock contention; retrying transaction"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                let spread = (rand_core::OsRng.next_u64() % (delay_ms / 2).max(1)) as u64;
                delay_ms = (delay_ms * 2 + spread).min(2_000);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop returns on the final attempt")
}

/// One attempt at the failure-backoff read-modify-write. See `record_scan_failure`.
async fn record_scan_failure_once(
    pool: &SqlitePool,
    series_id: &str,
    now: DateTime<Utc>,
    kind: FailureKind,
) -> anyhow::Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let prior: i64 = sqlx::query_scalar(
        "SELECT consecutive_failures FROM series_scan_state WHERE series_id = ?",
    )
    .bind(series_id)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(0);
    let failures = prior.saturating_add(1);
    // base * 2^(failures-1), capped. `checked_shl` guards the shift; the `.min` caps it
    // well below any overflow, so a huge streak just pins at the cap.
    let shift = (failures - 1).clamp(0, 40) as u32;
    let backoff_minutes = ERROR_BACKOFF_BASE_MINUTES
        .saturating_mul(1i64.checked_shl(shift).unwrap_or(i64::MAX))
        .min(ERROR_BACKOFF_MAX_HOURS * 60);
    let next = (now + chrono::Duration::minutes(backoff_minutes)).to_rfc3339();
    let now_iso = now.to_rfc3339();
    sqlx::query(
        "INSERT INTO series_scan_state \
           (series_id, next_scan_at, consecutive_failures, last_failure_kind, last_failure_at, \
            updated_at) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(series_id) DO UPDATE SET \
           next_scan_at = excluded.next_scan_at, \
           consecutive_failures = excluded.consecutive_failures, \
           last_failure_kind = excluded.last_failure_kind, \
           last_failure_at = excluded.last_failure_at, \
           updated_at = excluded.updated_at",
    )
    .bind(series_id)
    .bind(&next)
    .bind(failures)
    .bind(kind.as_str())
    .bind(&now_iso)
    .bind(&now_iso)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// How far out (hours) to park a paused series: `PAUSED_PARK_HOURS` jittered by
/// ±`PARK_JITTER_SPREAD_HOURS/2`, drawn uniformly.
///
/// Jitter is load-bearing, not cosmetic: without it a cold-start cohort parked in the same
/// drain all comes back due in the same window a fortnight later and re-clusters into a
/// thundering herd (audit #9 / handoff constraint 3).
///
/// Extracted from `park_paused` so the horizon invariant can be asserted against the code
/// that actually runs rather than against a copy of its arithmetic — see
/// `every_park_writer_stays_below_the_absurd_horizon`. Its worst case is `MAX_PARKED_HOURS`.
fn park_offset_hours() -> i64 {
    let jitter = (rand_core::OsRng.next_u64() % PARK_JITTER_SPREAD_HOURS as u64) as i64
        - PARK_JITTER_SPREAD_HOURS / 2;
    PAUSED_PARK_HOURS + jitter
}

/// Park a paused series' next scan far out so it drops out of the frequent due-set,
/// leaving its other scan-state fields intact. Upsert so a never-scanned paused series
/// (a pending row, or none yet) is parked too.
async fn park_paused(pool: &SqlitePool, series_id: &str, now: DateTime<Utc>) -> anyhow::Result<()> {
    let next = (now + chrono::Duration::hours(park_offset_hours())).to_rfc3339();
    let now_iso = now.to_rfc3339();
    // Also clear any stale `awaiting_since` (a paused series is not "awaiting a late
    // chapter") and reset the failure counter — the fetch that led here succeeded.
    sqlx::query(
        "INSERT INTO series_scan_state \
           (series_id, next_scan_at, awaiting_since, consecutive_failures, last_failure_kind, \
            last_failure_at, updated_at) \
         VALUES (?, ?, NULL, 0, NULL, NULL, ?) \
         ON CONFLICT(series_id) DO UPDATE SET \
           next_scan_at = excluded.next_scan_at, \
           awaiting_since = NULL, \
           consecutive_failures = 0, \
           last_failure_kind = NULL, \
           last_failure_at = NULL, \
           updated_at = excluded.updated_at",
    )
    .bind(series_id)
    .bind(&next)
    .bind(&now_iso)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Phase E2: LATEST as a push signal ────────────────────────────────────────────

/// How far out a series must be scheduled to count as PARKED for trigger purposes.
/// Comfortably past every ordinary cadence (the slowest tier is 24 h) and comfortably
/// below `PAUSED_PARK_HOURS`, so it selects parked series without catching a series that
/// merely has a long interval.
pub const TRIGGER_PARKED_AFTER_HOURS: i64 = 72;

/// Cooldown between triggers for the same series. Longer than the daily source-sync
/// interval ON PURPOSE: a paused series can sit in its source's top 30 for days
/// (measured: 3.3–7.5 days at real churn rates), and re-scanning it on every walk is
/// exactly the polling this phase exists to remove. One scan settles the question — a real
/// reopen flips the series to ONGOING and it stops being paused at all.
pub const TRIGGER_COOLDOWN_HOURS: i64 = 24 * 7;

/// Wake parked series that their source has just published for: schedule them due-now and
/// let the ordinary scheduler pick them up on its next tick.
///
/// ENQUEUE, NEVER SCAN INLINE. The source-sync pass is a long walk of upstream fetches;
/// scanning here would make it longer and would bypass every piece of failure handling the
/// scheduler already has (error backoff, lock retries, cached-fallback honesty). Writing
/// the due-now sentinel costs one indexed UPDATE and reuses all of it.
pub async fn trigger_due_now(pool: &SqlitePool, series_ids: &[String]) -> u64 {
    if series_ids.is_empty() {
        return 0;
    }
    let mut woken = 0;
    let now_iso = Utc::now().to_rfc3339();
    for id in series_ids {
        let res = sqlx::query(
            "UPDATE series_scan_state SET next_scan_at = ?, updated_at = ? WHERE series_id = ?",
        )
        .bind(DUE_NOW_SENTINEL)
        .bind(&now_iso)
        .bind(id)
        .execute(pool)
        .await;
        match res {
            Ok(r) => woken += r.rows_affected(),
            Err(e) => tracing::warn!(series_id = %id, error = %e, "latest-trigger: wake failed"),
        }
    }
    woken
}

/// Ensure a series has a scan-state row so the DB-driven scheduler will pick it up.
/// Inserts a minimal "due now" row (`next_scan_at = DUE_NOW_SENTINEL`) without disturbing
/// an existing one. Used by enrol paths that DON'T scan-on-enrol (federated search) and by
/// the daily reconcile to backfill any pre-existing enrolled series that lacks a row. The
/// sentinel (not NULL) is required by the due-query's `<= ?` completeness invariant.
pub async fn ensure_pending(pool: &SqlitePool, series_id: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO series_scan_state (series_id, next_scan_at, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(series_id) DO NOTHING",
    )
    .bind(series_id)
    .bind(DUE_NOW_SENTINEL)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// Re-fetch one series' chapters, detect new ones, and persist its scan state
/// (rolling avg, chapter count, `last_scanned_at`, next `next_scan_at`). Returns
/// whether new chapters were found.
///
/// The enrol paths (`add_source_series`, `ingest_source_series`) and the admin
/// `triggerScan` / unpause mutations call this with a manga they already hold; the
/// scheduler uses `scan_due` (which fetches manga + chapters together). It re-reads the
/// admin override so it's self-contained; fetch + persist is delegated to `persist_scan`.
pub async fn scan_series(
    state: &AppState,
    m: &SuwayomiManga,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let series_id = m.id.to_string();
    let admin = scan_admin(&state.pool, &series_id).await;
    let (chapters, provenance) = state.suwayomi.chapters_with_provenance(m.id).await?;
    // Same honesty rule as `scan_due` (E4.2/F11): a list read out of Suwayomi's local DB
    // because the upstream fetch failed is not a scan. Persisting it here would be worse
    // than in the scheduler, because this is the ENROL path — a brand-new series has an
    // empty cache, so the fallback returns `[]` and we would baseline it at 0 chapters and
    // call that a success. Every caller already treats an error as non-fatal (log and let
    // the scheduler retry) except the admin `triggerScan` / unpause mutations, which SHOULD
    // surface it: "the scan you asked for did not actually reach the source".
    if provenance.is_cached_fallback() {
        if let Err(be) =
            record_scan_failure(&state.pool, &series_id, now, FailureKind::CachedFallback).await
        {
            tracing::warn!(series_id, error = %be, "scan: failed to record backoff");
        }
        return Err(anyhow::anyhow!(
            "suwayomi served CACHED chapters for series {series_id}: the upstream fetch failed, \
             so this scan proves nothing about new chapters"
        ));
    }
    persist_scan(state, m, &chapters, &admin, now).await
}

/// The format Phase E tiers this series on, resolved from the MATERIALIZED `comic_type`
/// the reader's own badge is drawn from.
///
/// ## Why this replaced `catalog::work_comic_type_word` (deleted 2026-07-31, §8i)
///
/// That function selected `w.comic_type` — **a column that does not exist**. `work` carries
/// `content_type_override` (migration 0030) and nothing else; `comic_type` is materialized
/// onto `feed_series_updates` (0064) and `browse_catalogue` (0069) by `catalog::fill_comic_types`
/// running the real `resolve_comic_type`. The query therefore failed with
/// `no such column: w.comic_type` on *every* call, and its `.ok()?` swallowed the error into
/// `None`, so `persist_scan` fell back to `ComicType::Manga` for the entire library.
///
/// The visible effect was that **the whole Phase E tier engine was inert**: every series took
/// `TIER_MANGA_HOURS`, no manhwa/manhua/webtoon ever reached the 3 h tier and no dormant one
/// ever reached 24 h — while the unit tests passed, because they call `record_scan_policy`
/// with a hand-built `ComicType` and never exercise this lookup. `the_tier_lookup_resolves_a_real_type`
/// is the end-to-end guard that was missing.
///
/// Two PRIMARY-KEY probes off one `(source_type, source_key)` seek, and the feed row is
/// preferred because this module keeps it filled itself (`fill_missing_feed_comic_type`).
/// An unmapped series, or one whose feed row has not been typed yet, falls back to Manga —
/// the slow, safe tier, and self-healing once the rebuild types it.
async fn scan_comic_type(pool: &SqlitePool, series_id: &str) -> ComicType {
    let word = sqlx::query_scalar::<_, Option<String>>(
        "SELECT COALESCE(f.comic_type, b.comic_type) \
           FROM source_series ss \
           LEFT JOIN feed_series_updates f ON f.work_id = ss.work_id \
           LEFT JOIN browse_catalogue b ON b.work_id = ss.work_id \
          WHERE ss.source_type = 'suwayomi' AND ss.source_key = ? LIMIT 1",
    )
    .bind(series_id)
    .fetch_optional(pool)
    .await;
    match word {
        Ok(w) => w
            .flatten()
            .and_then(|w| crate::graphql::types::comic_type_from_word(&w))
            .unwrap_or(ComicType::Manga),
        Err(e) => {
            // Loudly, unlike the `.ok()?` that hid the dead column for a whole phase.
            tracing::warn!(series_id, error = %e, "scan: comic-type lookup failed; tiering as Manga");
            ComicType::Manga
        }
    }
}

/// Persist a scan from an ALREADY-FETCHED manga + chapters: refresh the DB caches (S1 —
/// so reader requests serve from SQLite instead of live-fetching), then detect new
/// chapters + schedule the next scan (`record_scan`). Cache writes are best-effort — a
/// cache hiccup must never fail the scan. Shared by `scan_series` and `scan_due`.
async fn persist_scan(
    state: &AppState,
    m: &SuwayomiManga,
    chapters: &[SuwayomiChapter],
    admin: &ScanAdmin,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let series_id = m.id.to_string();
    if let Err(e) = crate::series_cache::put_series(&state.pool, m).await {
        tracing::warn!(series_id, error = %e, "scan: series cache write failed");
    }
    if let Err(e) = crate::series_cache::put_chapters(&state.pool, m.id, chapters).await {
        tracing::warn!(series_id, error = %e, "scan: chapter cache write failed");
    }
    // Phase E: resolve the work's format for the cadence tier. Missing (unmapped series or
    // no stored type) defaults to Manga, which is both the safe slow tier and the same
    // default `resolve_comic_type` lands on. Passing `Some` also switches this scan onto the
    // policy path (tier target + `awaiting` off), which is what makes it cheaper than the
    // legacy flat-12h cadence.
    let comic_type = scan_comic_type(&state.pool, &series_id).await;
    let new_found = record_scan_policy(
        &state.pool,
        &series_id,
        &m.title,
        admin,
        chapters,
        Some(comic_type),
        now,
    )
    .await?;
    // Both columns the merged Updates feed reads for this series are now written —
    // `suwayomi_series.latest_chapter_at` (by `put_chapters`, above) and
    // `series_scan_state.last_new_chapter_at` (by `record_scan`) — so fold the
    // detection straight into the feed instead of waiting for the next rebuild.
    touch_feed_series_update(&state.pool, &series_id, new_found).await;
    Ok(new_found)
}

/// Incrementally refresh ONE series' row in the materialized merged Updates feed
/// (`feed_series_updates`, migration 0064) after a scan, so a chapter we just detected
/// is visible in `/updates` immediately.
///
/// WHY THIS EXISTS. That table is rebuilt wholesale by
/// `catalog::refresh_feed_series_updates`, which runs at boot and after each catalogue
/// sync. That cadence is right for its MangaDex mirror half (canonical chapters only
/// change then) but not for its SCANNER half, which is exactly this module's output and
/// changes continuously: without this call a series we detect a chapter for does not
/// appear in — or move up — the feed until the next refresh, up to a full sync interval
/// late. The previous resolver read `suwayomi_series` live and had no such lag, so
/// skipping this would be a freshness REGRESSION against the behaviour the feed
/// replaced — and late chapters are the complaint the whole scan-cadence effort exists
/// to fix.
///
/// GATED ON `new_found`, not on "a scan happened". `persist_scan` runs on every scan
/// (~475+/hr in steady state) but only a detection can change anything this feed sorts
/// or labels by: `released_at` comes from `suwayomi_series.latest_chapter_at`, which
/// `series_cache::derive_latest_chapter_at` only advances when a chapter appears, and
/// `detected_at` is `series_scan_state.last_new_chapter_at`, which `record_scan` only
/// stamps on a detection. So an unchanged scan has nothing worth a write — this turns
/// ~475 writes/hr into a handful.
///
/// WHAT THE GATE MISSES, exhaustively. Each is bounded by the next rebuild, and none of
/// them is worse than the rebuild-only behaviour this call was added to:
///
/// * an upstream edit to an EXISTING chapter's `upload_date`. The max-merge below would
///   refuse to move the row backwards for a correction anyway;
/// * a pure upstream DELETION. It lowers `suwayomi_series.chapter_count` without
///   producing a detection, and that column IS a label for a scanner-only card (the
///   reader renders `Ch. {latest_chapter ?? chapter_count}`), so such a card can read one
///   chapter high until the rebuild — the same single stale number it would have read for
///   the whole interval without this call;
/// * display metadata the periodic rebuild owns outright (`title`, `reader_id`,
///   `latest_chapter`), which the conflict clause never rewrites in any case.
///
/// BEST-EFFORT, like the cache writes above: logged and swallowed, never propagated. A
/// feed row is a pure cache and the periodic rebuild reconstructs it; failing a scan
/// (and with it the scan-state write and the reschedule) over one would be strictly
/// worse than being one refresh stale.
async fn touch_feed_series_update(pool: &SqlitePool, series_id: &str, new_found: bool) {
    if !new_found {
        return;
    }
    if let Err(e) = upsert_feed_series_update(pool, series_id).await {
        tracing::warn!(series_id, error = %e, "scan: updates-feed row upsert failed");
    }
}

/// The one-row UPSERT behind [`touch_feed_series_update`]. Keyed by the feed's primary
/// key, so it is a single indexed write.
///
/// The field mapping MIRRORS the scanner half of `catalog::refresh_feed_series_updates`
/// statement-for-statement — same joins, same guards, same `GROUP BY ss.work_id` (a work
/// can have several Suwayomi sources; SQLite's bare-columns-with-one-MAX rule then takes
/// every column from the work's NEWEST-RELEASING source, not smeared across sources), and
/// the same `ON CONFLICT` merge. Divergence would be a latent bug where a row's contents
/// depend on which writer touched it last, so the agreement is PROVEN rather than
/// asserted: `incremental_write_converges_with_the_periodic_rebuild` drives this path and
/// then runs the real `catalog::refresh_feed_series_updates` over the result and demands a
/// byte-identical row. (That test therefore also pins this function to the rebuild's
/// current text — if the rebuild's field mapping is edited, it fails here.)
///
/// The agreement covers every column this feed SORTS or LABELS by. It does NOT cover the
/// display metadata the conflict clause deliberately never rewrites — `title`,
/// `reader_id`, `latest_chapter`, and a `cover_url` whose `?v=` has since been bumped: on
/// a row THIS path created, those stay frozen at the first write until the next rebuild.
/// That is the same staleness the rebuild-only behaviour had (nothing here makes a column
/// worse than not running at all), and it is the price of the mirror-wins rule below —
/// this statement cannot tell "the existing row is the mirror half" from "the existing row
/// is my own earlier write", and clobbering a mirror row's title would be the worse bug.
///
/// The only narrowing is the `IN (SELECT …)`, which restricts the rebuild's query to the
/// work this series maps to. Load-bearing details, all inherited:
///
/// * `released_at` is INTEGER EPOCH-MILLIS, never TEXT. The two halves' clocks are stored
///   in incompatible text encodings (ISO-8601 vs 13-digit millis) and under BINARY
///   collation every `'2…'` sorts above every `'1…'`, so a TEXT key would silently sort
///   the entire mirror half above the entire scanner half. See migration 0064.
/// * `MAX(existing, excluded)` on `released_at`: an incremental write may only move a row
///   FORWARD in time, and can never pull it back below a fresher mirror-half value.
/// * `reader_id` is the canonical `w_…` id when the work is mangadex-anchored (the anchor
///   test in the SELECT) and the numeric Suwayomi id otherwise, and is left untouched on
///   CONFLICT — so a mangadex-anchored work navigates to its canonical page whether the
///   mirror half won the insert race or (a takedown) never fired at all.
/// * rows with no release time are excluded (`latest_chapter_at IS NOT NULL`) rather than
///   inserted with a NULL into a NOT NULL column.
///
/// Two conflict-clause assignments are stated in the *converged* direction rather than
/// copied verbatim, because the rebuild's clause is written for a table whose only
/// pre-existing row is the mirror half inserted moments earlier in the same transaction,
/// and here the pre-existing row may be an older SCANNER row:
///
/// * `chapter_count` takes `excluded` when we have one — and we ALWAYS have one, because
///   `suwayomi_series.chapter_count` is `INTEGER NOT NULL DEFAULT 0` (migration 0022), so
///   the fallback arm is dead and this is unconditionally the current Suwayomi count. In
///   the rebuild the existing value at conflict time is always the mirror half's literal
///   NULL (the table is DELETEd first, and the mirror insert writes NULL into this
///   column), so `COALESCE(existing, excluded)` there lands that same count. The two
///   writers therefore converge exactly. Copying it literally would instead pin the count at whatever
///   the first incremental write saw, and the reader renders
///   `Ch. {latest_chapter ?? chapter_count}` — a scanner-only card would announce a new
///   chapter while still printing the old number.
/// * `comic_type` is filled ONLY when missing (below), never changed. The rebuild's
///   `fill_feed_series_updates_types` pass owns it; but that pass cannot see a row that
///   did not exist when it ran, and a NULL type is invisible to the reader's format
///   filter — so a series whose FIRST-ever detection lands here would appear in the
///   unfiltered feed and vanish from every format tab until the next rebuild.
/// * `status` / `content_rating` (migration 0068) ARE assigned on conflict, unlike every
///   other display field, because they are properties of the WORK rather than of the half
///   that published the row: both writers derive them from the same `work` row through the
///   same `catalog::FSU_STATUS_SQL`, so `excluded` and the existing value always agree.
///   They must not be left out: `status` is nullable, and a NULL is invisible to Browse's
///   status filter for exactly the same reason a NULL `comic_type` is invisible to the
///   format tabs — a series whose first detection lands here would otherwise sit in the
///   unfiltered catalogue and vanish from every status filter until the next rebuild.
/// * `en_chapter_count` cannot be expressed in the conflict clause at all and is filled by
///   [`fill_feed_en_chapter_count`] immediately after; see there.
///
/// Finally, [`mirror_feed_row_into_browse_catalogue`] copies the finished row onto Browse's
/// own table (migration 0069). Browse stopped reading THIS table, so without that copy the
/// detection would be invisible on the surface that pages the whole catalogue.
async fn upsert_feed_series_update(pool: &SqlitePool, series_id: &str) -> anyhow::Result<u64> {
    let status_sql = crate::catalog::FSU_STATUS_SQL;
    let n = sqlx::query(&format!(
        "INSERT INTO feed_series_updates \
             (work_id, reader_id, title, cover_url, suwayomi_thumbnail, comic_type, \
              latest_chapter, latest_chapter_title, chapter_count, released_at, \
              detected_at, is_nsfw, status, content_rating) \
         SELECT ss.work_id, \
                CASE WHEN EXISTS (SELECT 1 FROM source_series md \
                                   WHERE md.work_id = ss.work_id \
                                     AND md.source_type = 'mangadex') \
                     THEN ss.work_id ELSE CAST(sy.id AS TEXT) END, \
                COALESCE(w.title_override, w.primary_title, sy.title), \
                CASE WHEN w.cover_cached_version IS NOT NULL \
                     THEN '/covers/' || w.id || '.webp?v=' || w.cover_cached_version \
                     END, \
                sy.thumbnail_url, NULL, \
                printf('%g', lc.chapter_number), lc.name, sy.chapter_count, \
                MAX(CAST(sy.latest_chapter_at AS INTEGER)), \
                sss.last_new_chapter_at, \
                COALESCE(w.is_nsfw_override, w.is_nsfw), \
                {status_sql}, COALESCE(w.content_rating, 'safe') \
         FROM source_series ss \
         JOIN work w ON w.id = ss.work_id \
         JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
         LEFT JOIN series_scan_state sss ON sss.series_id = ss.source_key \
         LEFT JOIN (SELECT manga_id, chapter_number, name, \
                            ROW_NUMBER() OVER (PARTITION BY manga_id \
                                ORDER BY CAST(upload_date AS INTEGER) DESC, \
                                         chapter_number DESC) AS rn \
                       FROM suwayomi_chapter \
                      WHERE chapter_number >= 0 AND chapter_number <= 5000) lc \
                ON lc.manga_id = sy.id AND lc.rn = 1 \
         WHERE ss.source_type = 'suwayomi' AND sy.in_library = 1 \
           AND sy.latest_chapter_at IS NOT NULL \
           AND ss.work_id IN (SELECT work_id FROM source_series \
                              WHERE source_type = 'suwayomi' AND source_key = ?) \
         GROUP BY ss.work_id \
         ON CONFLICT(work_id) DO UPDATE SET \
             released_at = MAX(feed_series_updates.released_at, excluded.released_at), \
             detected_at = COALESCE(excluded.detected_at, feed_series_updates.detected_at), \
             chapter_count = COALESCE(excluded.chapter_count, feed_series_updates.chapter_count), \
             cover_url = COALESCE(NULLIF(feed_series_updates.cover_url, ''), excluded.cover_url), \
             suwayomi_thumbnail = COALESCE(feed_series_updates.suwayomi_thumbnail, excluded.suwayomi_thumbnail), \
             is_nsfw = MAX(feed_series_updates.is_nsfw, excluded.is_nsfw), \
             status = excluded.status, \
             content_rating = excluded.content_rating"
    ))
    .bind(series_id)
    .execute(pool)
    .await?
    .rows_affected();
    if n > 0 {
        fill_missing_feed_comic_type(pool, series_id).await?;
        fill_feed_en_chapter_count(pool, series_id).await?;
        // The release clock, from the ledger (Phase C2) — the same statement the rebuild's
        // pass (5) runs, so the two cannot disagree. BEFORE the Browse mirror below, which
        // copies this row's columns verbatim: projecting afterwards would leave Browse
        // holding the pre-projection clock until the next rebuild.
        //
        // This is what makes the `released_at = MAX(existing, excluded)` above harmless.
        // That clause can still move the clock forward on its own — it is the F7 bug — but
        // whatever it computes is immediately replaced by `MAX(release_event.first_seen_at)`,
        // which a duplicate chapter cannot move because it creates no event. The clause is
        // left in place rather than deleted so this function still converges with the
        // rebuild's scanner half while the ledger is still filling and the projection is
        // inert.
        let work_id: Option<String> = sqlx::query_scalar(
            "SELECT work_id FROM source_series \
             WHERE source_type = 'suwayomi' AND source_key = ? LIMIT 1",
        )
        .bind(series_id)
        .fetch_optional(pool)
        .await?;
        if let Some(w) = work_id {
            crate::catalog::project_feed_from_ledger_for_work(pool, &w).await?;
        }
        mirror_feed_row_into_browse_catalogue(pool, series_id).await?;
    }
    Ok(n)
}

/// The `browse_catalogue` columns [`mirror_feed_row_into_browse_catalogue`] re-assigns on
/// conflict — everything except the primary key and `comic_type` (which
/// `catalog::fill_comic_types` owns).
///
/// **This IS `catalog::BROWSE_CATALOGUE_MUTABLE`, not a copy of it.**
///
/// It used to be a hand-copied duplicate, because that const was private to `catalog`. A copy is
/// a drift hazard and it had already drifted once: `latest_chapter` (migration 0095) was added to
/// the rebuild's list and not to this one, so Browse cards froze on the chapter number they held
/// at first insert until the next wholesale rebuild (§8h) — the COUNT advanced while the NUMBER
/// did not, which is F4 wearing a different hat.
///
/// The interim guard parsed the other const out of `catalog/mod.rs` with `include_str!` and
/// asserted element-for-element equality. That worked, but it detected drift rather than
/// preventing it. The const is now `pub(crate)`, so the two lists are the same object and the
/// drift class no longer exists — which is why that test is gone rather than merely passing.
use crate::catalog::BROWSE_CATALOGUE_MUTABLE as BROWSE_MIRROR_MUTABLE;

/// Propagate the feed row this scan just wrote into `browse_catalogue` (migration 0069).
///
/// WHY. Everything above keeps `/updates` fresh within one scan. Browse reads a DIFFERENT
/// table as of 0069, rebuilt only at boot and once per catalogue-sync cycle — so without this
/// the detection we just recorded would not move the series on Browse's "Recently updated"
/// ordering (its DEFAULT sort's second phase) for up to a full sync interval. That is exactly
/// the freshness regression `touch_feed_series_update`'s own doc argues against, transplanted
/// onto the surface that pages the whole catalogue.
///
/// A COPY, not a second derivation. `catalog::refresh_browse_catalogue` takes every shared
/// column verbatim from `feed_series_updates` for a work that has a row there, and this work
/// does (we just wrote it), so copying is not an approximation of the rebuild — it IS the
/// rebuild's rule, narrowed to one work. That is what keeps the two writers convergent without
/// duplicating the `CASE`-per-column SELECT.
///
/// `comic_type` is deliberately absent from the conflict clause, matching the rebuild:
/// `catalog::fill_comic_types` owns it. It IS carried on the INSERT path, where the row is new
/// and would otherwise be untyped — a NULL type is invisible to Browse's format tabs, so a
/// series whose first-ever detection lands here would sit in the unfiltered grid and vanish
/// from every format chip until the next rebuild. The value is the one
/// `fill_missing_feed_comic_type` just guaranteed, computed by the same function over the same
/// title the type pass would have read.
///
/// `created_at` comes from `work`, the only column here the feed does not carry.
///
/// The mutable-column set lives in [`BROWSE_MIRROR_MUTABLE`], which is pinned equal to
/// `catalog::BROWSE_CATALOGUE_MUTABLE`.
async fn mirror_feed_row_into_browse_catalogue(
    pool: &SqlitePool,
    series_id: &str,
) -> anyhow::Result<()> {
    // Same mutable columns, same "write only when something differs" guard as
    // `catalog::refresh_browse_catalogue` — `IS NOT`, not `<>`, because several of them are
    // nullable and `NULL <> NULL` is NULL, which would rewrite the row's index entries on
    // every scan.
    //
    // `latest_chapter` (migration 0095) MUST stay in this list: it is the chapter NUMBER
    // Browse prints beside the count ("12 ch · Ch. 151"), and it changes on exactly the
    // scans this function mirrors. Omitting it left the number stale until the next
    // wholesale `refresh_browse_catalogue` (§8h).
    //
    // THIS LIST MUST EQUAL `catalog::BROWSE_CATALOGUE_MUTABLE`, ELEMENT FOR ELEMENT. It is a
    // copy only because that const is private to `catalog` and this module may not widen it;
    // `the_browse_mirror_column_list_matches_the_rebuilds` reads the other list out of
    // `catalog/mod.rs` at compile time (`include_str!`) and asserts exact equality, so the
    // next column added to either list and not the other fails the build's test run rather
    // than silently freezing a Browse card on a stale value the way §8h's `latest_chapter`
    // did. See `BROWSE_MIRROR_MUTABLE`.
    const MUT: &[&str] = BROWSE_MIRROR_MUTABLE;
    let set = MUT
        .iter()
        .map(|c| format!("{c} = excluded.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let changed = MUT
        .iter()
        .map(|c| format!("browse_catalogue.{c} IS NOT excluded.{c}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    sqlx::query(&format!(
        "INSERT INTO browse_catalogue \
             (work_id, reader_id, title, cover_url, suwayomi_thumbnail, comic_type, status, \
              content_rating, is_nsfw, en_chapter_count, latest_chapter, released_at, created_at) \
         SELECT f.work_id, f.reader_id, f.title, f.cover_url, f.suwayomi_thumbnail, \
                f.comic_type, f.status, f.content_rating, f.is_nsfw, f.en_chapter_count, \
                f.latest_chapter, f.released_at, w.created_at \
           FROM feed_series_updates f \
           JOIN work w ON w.id = f.work_id \
          WHERE f.work_id IN (SELECT work_id FROM source_series \
                              WHERE source_type = 'suwayomi' AND source_key = ?) \
         ON CONFLICT(work_id) DO UPDATE SET {set} WHERE {changed}"
    ))
    .bind(series_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Give a feed row this scan touched the `en_chapter_count` (migration 0068) the rebuild's
/// phase 4 would have given it — Browse's CHAPTERS sort key and the number its cards print.
///
/// NOT expressible in the upsert's `ON CONFLICT` clause, which is why it is a separate pass.
/// The rebuild's precedence is "the English MangaDex mirror count where it is non-zero, else
/// the Suwayomi source count", and a conflict clause cannot tell which of those produced the
/// value already stored: `excluded.<count>` would clobber a real mirror count with the
/// Suwayomi one, while `CASE WHEN existing > 0 THEN existing` would freeze a
/// scanner-half row at whatever the FIRST detection saw — the exact bug
/// `incremental_write_converges_with_the_periodic_rebuild` was written for, where the card
/// announces a new chapter while still printing the old count.
///
/// So this re-runs the rebuild's two statements verbatim, scoped to the one work. Both are
/// indexed single-work lookups and this only runs on a scan that actually DETECTED something
/// (~5% of ~475 scans/hr), so the cost is negligible; correctness here is the point, because
/// the two writers are proven byte-identical by that same test.
async fn fill_feed_en_chapter_count(pool: &SqlitePool, series_id: &str) -> anyhow::Result<()> {
    let scope = "work_id IN (SELECT work_id FROM source_series \
                             WHERE source_type = 'suwayomi' AND source_key = ?)";
    // The English mirror count. `COALESCE(…, 0)` and not a `WHERE EXISTS` guard: a work whose
    // English chapters were all UNLINKED must fall back to 0 and then to the Suwayomi count
    // below, exactly as a full rebuild would compute it from scratch.
    sqlx::query(&format!(
        "UPDATE feed_series_updates SET en_chapter_count = COALESCE( \
             (SELECT COUNT(DISTINCT c.number) FROM chapter c \
                JOIN source_series ss ON ss.id = c.source_series_id \
               WHERE ss.work_id = feed_series_updates.work_id \
                 AND ss.source_type = 'mangadex' AND c.lang = 'en'), 0) \
         WHERE {scope}"
    ))
    .bind(series_id)
    .execute(pool)
    .await?;
    // Then the Suwayomi fallback, which may only ever RAISE a zero.
    sqlx::query(&format!(
        "UPDATE feed_series_updates SET en_chapter_count = \
             (SELECT MAX(sy.chapter_count) FROM source_series ss \
                JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
               WHERE ss.work_id = feed_series_updates.work_id \
                 AND ss.source_type = 'suwayomi') \
         WHERE {scope} AND en_chapter_count = 0 \
           AND COALESCE((SELECT MAX(sy.chapter_count) FROM source_series ss \
                           JOIN suwayomi_series sy ON sy.id = CAST(ss.source_key AS INTEGER) \
                          WHERE ss.work_id = feed_series_updates.work_id \
                            AND ss.source_type = 'suwayomi'), 0) > 0"
    ))
    .bind(series_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Give a feed row this scan just CREATED the `comic_type` the rebuild's
/// `fill_feed_series_updates_types` pass would have given it.
///
/// Runs the real [`crate::graphql::types::resolve_comic_type`] over the same inputs, in
/// the same precedence (curated `work_tag` wins outright over source genres), and stores
/// the same COLLAPSED word (`WEBTOON → MANHWA`, `COMIC → MANGA`) the reader's `toViewType`
/// uses — a SQL approximation would disagree with every other format badge.
///
/// Scoped by `comic_type IS NULL`, so it costs one scalar read on the overwhelmingly
/// common path (the row already existed and already has a type) and never overwrites a
/// type the rebuild computed. The rebuild derives the type from the FEED row's title,
/// which on conflict stays the mirror half's; leaving an existing type alone keeps this
/// path from disagreeing with it.
async fn fill_missing_feed_comic_type(pool: &SqlitePool, series_id: &str) -> anyhow::Result<()> {
    let Some((work_id, title, override_word, original_language)) =
        sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
            "SELECT f.work_id, f.title, w.content_type_override, w.original_language \
             FROM feed_series_updates f JOIN work w ON w.id = f.work_id \
             WHERE f.comic_type IS NULL \
               AND f.work_id IN (SELECT work_id FROM source_series \
                                 WHERE source_type = 'suwayomi' AND source_key = ?)",
        )
        .bind(series_id)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(());
    };

    let curated = sqlx::query_scalar::<_, String>(
        "SELECT tag FROM work_tag WHERE work_id = ? ORDER BY ord, tag",
    )
    .bind(&work_id)
    .fetch_all(pool)
    .await?;
    let genres = if curated.is_empty() {
        // Source genres, deduped in source order. The CAST is on `ss.source_key`
        // (unindexed TEXT), never on `sw.id` — casting the indexed side makes the join
        // opaque to the planner (see `work_effective_genres`).
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for json in sqlx::query_scalar::<_, String>(
            "SELECT sw.genre FROM source_series ss \
             JOIN suwayomi_series sw ON sw.id = CAST(ss.source_key AS INTEGER) \
             WHERE ss.source_type = 'suwayomi' AND ss.work_id = ? AND sw.genre IS NOT NULL \
             ORDER BY ss.id",
        )
        .bind(&work_id)
        .fetch_all(pool)
        .await?
        {
            let Ok(list) = serde_json::from_str::<Vec<String>>(&json) else {
                continue;
            };
            for g in list {
                let g = g.trim().to_string();
                if !g.is_empty() && seen.insert(g.clone()) {
                    out.push(g);
                }
            }
        }
        out
    } else {
        curated
    };

    let word = crate::graphql::types::collapsed_comic_type_word(
        crate::graphql::types::resolve_comic_type(
            override_word.as_deref(),
            original_language.as_deref(),
            &genres,
            &title,
        ),
    );
    sqlx::query(
        "UPDATE feed_series_updates SET comic_type = ? WHERE work_id = ? AND comic_type IS NULL",
    )
    .bind(word)
    .bind(&work_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Detect new chapters from a freshly-fetched chapter list and persist the
/// series' scan state. Split out of `scan_series` so the bookkeeping is testable
/// without a live Suwayomi (it reads its own `prior` row, so the read-modify-write
/// is self-contained).
///
/// First observation (no prior row) records the baseline chapter count *without*
/// stamping `last_new_chapter_at` — otherwise a fresh deploy's first tick flags the
/// entire seeded back catalogue as just-updated and floods the `updates` feed [SC3].
///
/// The prior read and the upsert run in one transaction [SC6]: an admin
/// `triggerScan` overlapping a scheduler tick can no longer interleave their
/// read-modify-write and double-count or clobber `known_chapter_count`.
///
/// The transaction is `BEGIN IMMEDIATE`, not the default DEFERRED, and the whole
/// thing is retried on a lock error — see `record_scan` below for why.
/// Legacy record path (no Phase E policy): the interval comes from `resolve_interval` over
/// the inferred cadence, and the `awaiting` accelerated poll is live. This is the entry the
/// scheduling tests exercise; every production path (`persist_scan`, incl. enrol via
/// `scan_series`) goes through [`record_scan_policy`] with a real `comic_type`.
#[cfg(test)]
async fn record_scan(
    pool: &SqlitePool,
    series_id: &str,
    title: &str,
    admin: &ScanAdmin,
    chapters: &[SuwayomiChapter],
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    record_scan_policy(pool, series_id, title, admin, chapters, None, now).await
}

/// Phase E record path. When `comic_type` is `Some`, the steady interval is the format/
/// recency TARGET from [`scan_interval_hours`] and `awaiting` is off (E5's LATEST-diff push
/// signal replaces it). `None` falls back to the legacy inferred-cadence behaviour.
async fn record_scan_policy(
    pool: &SqlitePool,
    series_id: &str,
    title: &str,
    admin: &ScanAdmin,
    chapters: &[SuwayomiChapter],
    comic_type: Option<ComicType>,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    // Retry the ENTIRE transaction on a transient lock. A DEFERRED transaction that
    // read first and then upgraded to a write was returning SQLITE_BUSY_SNAPSHOT
    // (code 517) whenever another writer committed in between — 25 times in a 11.5h
    // window in production. `busy_timeout` cannot help there: the read snapshot is
    // already stale, so waiting changes nothing and only re-running the read can
    // recover. `BEGIN IMMEDIATE` (below) takes the write lock up front, which lets
    // `busy_timeout` do its job and makes 517 rare; this loop covers the remainder
    // plus plain BUSY (code 5) when the 15s timeout is exhausted.
    let mut delay_ms = 50_u64;
    for attempt in 0..=SCAN_LOCK_RETRIES {
        match record_scan_once(pool, series_id, title, admin, chapters, comic_type, now).await {
            Ok(found) => return Ok(found),
            Err(e) if crate::db::is_locked_error(&e) && attempt < SCAN_LOCK_RETRIES => {
                tracing::debug!(
                    series_id,
                    attempt = attempt + 1,
                    delay_ms,
                    "scan: write lock contention; retrying transaction"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                // Exponential with a small jittered spread, so several scan workers
                // that collided do not line up and collide again on the same beat.
                let spread = (rand_core::OsRng.next_u64() % (delay_ms / 2).max(1)) as u64;
                delay_ms = (delay_ms * 2 + spread).min(2_000);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop returns on the final attempt")
}

/// One attempt at the scan-state read-modify-write. See `record_scan_policy` for retries.
async fn record_scan_once(
    pool: &SqlitePool,
    series_id: &str,
    title: &str,
    admin: &ScanAdmin,
    chapters: &[SuwayomiChapter],
    comic_type: Option<ComicType>,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    // BEGIN IMMEDIATE, not the default DEFERRED: take the write lock at the start of
    // the transaction rather than upgrading to it after the read. That converts the
    // unretryable-by-timeout SQLITE_BUSY_SNAPSHOT into ordinary BUSY, which
    // `busy_timeout` (15s, see db::init) absorbs.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let prior_opt = sqlx::query_as::<_, ScanState>(SCAN_STATE_SELECT)
        .bind(series_id)
        .fetch_optional(&mut *tx)
        .await?;
    let first_observation = prior_opt.is_none();
    let prior = prior_opt.unwrap_or_default();

    let count = chapters.len() as i64;
    let cadence = upload_cadence(chapters);
    // Clamp the inferred average at WRITE time, not only where it is turned into a
    // schedule (P0-2). A sparse history spanning years used to persist an average of
    // 58,309 hours; the schedule-time clamp kept the NEXT scan sane but the absurd value
    // stayed in the row, was surfaced by the API, and is the number every downstream
    // heuristic reads. Nothing above `MAX_INTERVAL_HOURS` is a cadence.
    let computed_avg = cadence
        .as_ref()
        .map(|c| c.avg_hours)
        .unwrap_or(prior.avg_interval_hours)
        .clamp(0.0, MAX_INTERVAL_HOURS);
    // `known_max` is derived from the CURRENT list (sentinels dropped) so it heals
    // downward once garbage disappears upstream, instead of being pinned forever by
    // a single bad number. Falls back to the prior value only when the current list
    // has no real numbers at all (SC4).
    let latest = robust_max_number(chapters);
    let known_max = latest.or(prior.known_max_chapter);

    // New-chapter detection is a set-difference of chapter identities against the
    // prior snapshot: a chapter present now but absent before is new — even when
    // upstream simultaneously removed one (count stays flat) or the new chapter is
    // numbered at/below the current max. A missing prior snapshot (pre-migration
    // row, or first observation) seeds a baseline WITHOUT flagging, so neither an
    // upgrade nor a fresh series floods the `updates` feed (SC3).
    let current_ids: std::collections::HashSet<i64> = chapters.iter().map(|c| c.id).collect();
    let prior_ids = prior
        .known_chapter_ids
        .as_deref()
        .map(parse_id_set)
        .unwrap_or_default();
    let have_prior_snapshot = prior.known_chapter_ids.is_some();
    let new_found = !first_observation
        && have_prior_snapshot
        && current_ids.iter().any(|id| !prior_ids.contains(id));
    let known_chapter_ids = encode_id_set(&current_ids);
    // A shrinking count means upstream removed chapters; surface it (the count is
    // still overwritten downward, but it's no longer silent) (SC4).
    if !first_observation && count < prior.known_chapter_count {
        tracing::info!(
            series_id,
            title,
            prior = prior.known_chapter_count,
            total = count,
            "scan: chapter count regressed (upstream removed chapters)"
        );
    }
    let now_iso = now.to_rfc3339();
    // Steady cadence. Phase E policy path (`comic_type = Some`): the format/recency TARGET
    // from `scan_interval_hours`, returned verbatim (no MIN/ACTIVE_MAX clamp — that clamp is
    // what produced the flat 12h, §4.7). Legacy path (`None`, used by enrol + tests): the
    // inferred-cadence `resolve_interval`. Admin override wins in both.
    let policy_mode = comic_type.is_some();
    let steady_interval = match comic_type {
        // `newest_upload`, NOT `cadence.latest_upload`: the tier only needs "when did it
        // last release", and gating that on a two-gap cadence filed every single-chapter
        // (or single-timestamp) fast-format series as dormant. See `newest_upload`.
        Some(ct) => scan_interval_hours(
            ct,
            newest_upload(chapters),
            now,
            admin.override_interval_hours,
        ),
        None => resolve_interval(admin.override_interval_hours, computed_avg),
    };

    // ── awaiting: accelerate only a genuinely-late series ────────────────────────
    //
    // A baseline scan is never "late": the FIRST real scan of a series records a
    // starting point, not a missed chapter. That needs the prior chapter SNAPSHOT, not
    // just `first_observation` — `ensure_pending`/backfill pre-seed a row (due-now
    // sentinel, no `known_chapter_ids`), so a row can exist before any real scan.
    let baseline = first_observation || !have_prior_snapshot;
    // The publication cadence is trustworthy only when it was averaged over enough real
    // gaps; 3,257 live rows carry no cadence at all and would otherwise be judged
    // against a fabricated one.
    let publication_hours = cadence
        .as_ref()
        .filter(|c| c.gaps >= MIN_TRUSTED_GAPS)
        .map(|_| computed_avg)
        .filter(|h| *h > 0.0);
    // Lateness is measured against the newest UPLOAD — upstream's own clock — not
    // against our `next_scan_at` (which is due by construction here) nor against
    // `last_new_chapter_at` (which is NULL for 12,797 of 13,789 live rows because it
    // only records detections we happened to make).
    let genuinely_late = match (publication_hours, cadence.as_ref()) {
        (Some(pub_hours), Some(c)) => {
            let since = (now - c.latest_upload).num_seconds() as f64 / 3600.0;
            is_genuinely_late(pub_hours, since)
        }
        _ => false,
    };
    // Due-ness is retained as a guard (not as the signal): it is true by construction for
    // every scheduler-driven scan, but it still stops an admin `triggerScan` fired well
    // before a series' cadence from flipping it into the accelerated poll.
    let due_now = is_due(prior.next_scan_at.as_deref(), now);
    // Phase E: `awaiting` is OFF on the policy path. It re-polls a "genuinely late" series
    // every 30 min and is 43% of all scan traffic (§8e) — the single largest consumer, and
    // by construction it polls the LEAST productive series (ones already known to be late)
    // hardest. E5's LATEST-diff push signal makes it pointless: a real late chapter shows up
    // in the source's LATEST within one discovery tick. Retained only on the legacy path.
    let awaiting = !policy_mode && due_now && genuinely_late && !new_found && !baseline;
    // The streak start doubles as the cool-down marker (see `AWAITING_REARM_HOURS`):
    // preserved while the cool-down runs so the accelerated window can't immediately
    // re-open, re-armed once it elapses, and cleared outright by a landed chapter or by
    // the series leaving the lateness band.
    let prior_streak_hours = parse_iso(prior.awaiting_since.as_deref())
        .map(|start| (now - start).num_seconds() as f64 / 3600.0);
    let awaiting_since = if !awaiting {
        None
    } else {
        match prior_streak_hours {
            None => Some(now_iso.clone()),
            Some(h) if h < AWAITING_REARM_HOURS => prior.awaiting_since.clone(),
            Some(_) => Some(now_iso.clone()),
        }
    };
    // Re-poll fast only within a bounded window from the streak start; beyond it the
    // series drops back to the steady cadence (SC1) — which is now itself capped at
    // `ACTIVE_MAX_INTERVAL_HOURS`, so "falling back" costs at most 12h of latency
    // instead of the 7–14 days it used to. The window is sized on the PUBLICATION
    // cadence, floored so a fast series still gets a usable one.
    let awaiting_window_hours = publication_hours
        .unwrap_or(MIN_INTERVAL_HOURS)
        .clamp(MIN_INTERVAL_HOURS, AWAITING_MAX_HOURS);
    let awaited_hours = parse_iso(awaiting_since.as_deref())
        .map(|start| (now - start).num_seconds() as f64 / 3600.0)
        .unwrap_or(0.0);
    let next_interval_hours = if awaiting && awaited_hours < awaiting_window_hours {
        resolve_poll_minutes(admin.poll_every_minutes, steady_interval) / 60.0
    } else {
        steady_interval
    };
    // Jittered so a batch scanned together does not come due together again — see
    // `jitter_interval_hours`. This is the steady/awaiting path, which is where the
    // overwhelming majority of reschedules happen. Phase E uses a per-tier fraction so the
    // 3h tier gets a tighter ±5% (±9 min) window; the legacy path keeps the flat ±10%.
    let jitter_fraction = if policy_mode {
        tier_jitter_fraction(next_interval_hours)
    } else {
        SCHEDULE_JITTER_FRACTION
    };
    let next_scan_at = (now
        + chrono::Duration::milliseconds(
            (jitter_interval_hours_with(next_interval_hours, jitter_fraction) * 3_600_000.0) as i64,
        ))
    .to_rfc3339();
    let last_new_chapter_at = if new_found {
        Some(now_iso.clone())
    } else {
        prior.last_new_chapter_at.clone()
    };

    // Phase E instrumentation (§10): what interval did this series ACTUALLY achieve, versus
    // the tier we targeted for it? Only on the policy path (the legacy path has no target),
    // only when a previous scan exists to measure from, and only when the previous schedule
    // was a real timestamp — a due-now sentinel means E2/E5 TRIGGERED this scan, so it
    // legitimately arrived early and is not evidence about the tier's cadence.
    if policy_mode && prior.next_scan_at.as_deref() != Some(DUE_NOW_SENTINEL) {
        if let Some(prev) = parse_iso(prior.last_scanned_at.as_deref()) {
            record_tier_achieved(steady_interval, (now - prev).num_seconds() as f64 / 3600.0);
        }
    }

    if new_found {
        tracing::info!(
            series_id,
            title,
            added = count - prior.known_chapter_count,
            total = count,
            latest = latest.unwrap_or(f64::NAN),
            avg_interval_hours = computed_avg,
            "scan: new chapters detected"
        );
    } else {
        tracing::debug!(
            series_id,
            title,
            total = count,
            avg_interval_hours = computed_avg,
            first_observation,
            "scan: no new chapters"
        );
    }

    sqlx::query(
        "INSERT INTO series_scan_state \
           (series_id, avg_interval_hours, known_chapter_count, known_max_chapter, \
            last_scanned_at, next_scan_at, last_new_chapter_at, awaiting_since, \
            known_chapter_ids, consecutive_failures, last_failure_kind, last_failure_at, \
            updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, NULL, NULL, ?) \
         ON CONFLICT(series_id) DO UPDATE SET \
           avg_interval_hours = excluded.avg_interval_hours, \
           known_chapter_count = excluded.known_chapter_count, \
           known_max_chapter = excluded.known_max_chapter, \
           last_scanned_at = excluded.last_scanned_at, \
           next_scan_at = excluded.next_scan_at, \
           last_new_chapter_at = excluded.last_new_chapter_at, \
           awaiting_since = excluded.awaiting_since, \
           known_chapter_ids = excluded.known_chapter_ids, \
           consecutive_failures = 0, \
           last_failure_kind = NULL, \
           last_failure_at = NULL, \
           updated_at = excluded.updated_at",
    )
    .bind(series_id)
    .bind(computed_avg)
    .bind(count)
    .bind(known_max)
    .bind(&now_iso)
    .bind(&next_scan_at)
    .bind(&last_new_chapter_at)
    .bind(&awaiting_since)
    .bind(&known_chapter_ids)
    .bind(&now_iso)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(new_found)
}

// ── Whole-source outage detection (Phase E4.3) ───────────────────────────────────
//
// E4.2 makes each failure honest. That alone converts one broken source into hundreds of
// identical per-series warnings, which is its own kind of invisible — "a source-wide outage
// should be one loud alert, not 209 silent ones". This is that aggregation, plus the two
// actions worth taking automatically.

/// How many of a source's series must be confirmably failing before it counts as a
/// whole-source outage. Small sources are excluded from the automatic PARK (they can be one
/// dead series away from 100% failing), but they still appear in the health view — the
/// admin surface reports everything; only the automatic action has a floor.
const SOURCE_OUTAGE_MIN_SERIES: i64 = 5;

/// …and what share of the source's tracked series that must be. A genuine site
/// move/rebrand breaks every series at once (Genz Toons: 209 of 209), whereas a handful of
/// deleted entries on a healthy source sits far below this.
const SOURCE_OUTAGE_FAILING_RATIO: f64 = 0.8;

/// How far out a source in outage has its series parked.
///
/// Without this, E4.2 leaves each series on the ordinary error backoff, which caps at
/// `ERROR_BACKOFF_MAX_HOURS` (24 h) — so a dead 209-series source costs 209 pointless
/// upstream fetches a day, forever. Parking the cohort a week out cuts that to ~30/day
/// while still re-probing continuously (the parks are jittered, so a trickle of series comes
/// due every day and any one of them succeeding ends the outage).
///
/// Deliberately much shorter than `PAUSED_PARK_HOURS`: this is a *broken* source, not a
/// *completed* series, and breakage is usually fixed by an extension update within days.
///
/// Its jitter is ONE-SIDED (`base + ABS(RANDOM()) % (base/5)`), so the real worst case is
/// +20%, not ±10% — `MAX_OUTAGE_PARKED_HOURS` carries that into the horizon invariant.
const SOURCE_OUTAGE_PARK_HOURS: i64 = 24 * 7;

/// Quiet window between repeat alerts for the same ongoing outage.
const SOURCE_OUTAGE_REALERT_HOURS: i64 = 24;

/// Is this source out of service? Both a floor and a share, because either alone is wrong:
/// a share alone declares an outage on a 1-series source with one dead entry, and a floor
/// alone declares one on a 900-series source with 5 deleted series. `confirmed_failing`
/// already excludes short blips and our own persist errors — see
/// `catalog::SourceScanHealth::confirmed_failing`.
fn is_source_out_of_service(series: i64, confirmed_failing: i64) -> bool {
    confirmed_failing >= SOURCE_OUTAGE_MIN_SERIES
        && (confirmed_failing as f64) >= SOURCE_OUTAGE_FAILING_RATIO * (series as f64)
}

/// Aggregate per-source scan health and act on whole-source outages.
///
/// Called after a tick that recorded failures — the only thing that can change the verdict —
/// so the steady state costs nothing. Everything it does is idempotent and derived from the
/// DB, so a restart mid-outage reaches the same conclusions.
async fn check_source_health(state: &AppState) {
    let health = match crate::catalog::source_scan_health(&state.pool).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "scan: per-source health aggregation failed");
            return;
        }
    };
    for h in health {
        if !is_source_out_of_service(h.series, h.confirmed_failing) {
            // Recovery: a probe got through, so drop the outage row and pull the cohort
            // back out of its park rather than making it wait out the week.
            match crate::catalog::clear_source_outage(&state.pool, &h.source_id).await {
                Ok(true) => {
                    let unparked = unpark_source_series(&state.pool, &h.source_id).await;
                    tracing::info!(
                        source_id = %h.source_id,
                        pkg_name = h.pkg_name.as_deref().unwrap_or("(uninstalled)"),
                        series = h.series,
                        unparked,
                        "scan: source recovered; outage cleared and its series unparked"
                    );
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(source_id = %h.source_id, error = %e, "scan: clearing source outage failed")
                }
            }
            continue;
        }
        // `cached_fallback` is the F11 signature (broken while reporting healthy);
        // `fetch_error` means it is at least failing out loud. Report whichever dominates.
        let kind = if h.cached_fallback >= h.fetch_error {
            FailureKind::CachedFallback
        } else {
            FailureKind::FetchError
        };
        let parked_until = park_source_series(&state.pool, &h.source_id).await;
        match crate::catalog::record_source_outage(
            &state.pool,
            &h.source_id,
            h.pkg_name.as_deref(),
            h.series,
            h.confirmed_failing,
            kind.as_str(),
            parked_until.as_deref(),
            SOURCE_OUTAGE_REALERT_HOURS,
        )
        .await
        {
            Ok(true) => tracing::error!(
                source_id = %h.source_id,
                pkg_name = h.pkg_name.as_deref().unwrap_or("(uninstalled)"),
                series = h.series,
                failing = h.confirmed_failing,
                cached_fallback = h.cached_fallback,
                fetch_error = h.fetch_error,
                zero_chapter_series = h.zero_chapter_series,
                kind = kind.as_str(),
                parked_until = parked_until.as_deref().unwrap_or("(not parked)"),
                "scan: WHOLE-SOURCE OUTAGE — every tracked series on this source is failing"
            ),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(source_id = %h.source_id, error = %e, "scan: recording source outage failed")
            }
        }
        // Stop the discovery walk too, where there is a subscription to stop. The walk and
        // the chapter fetch fail independently (Genz Toons' LATEST 404'd while its scans
        // reported success), so scan-side evidence trips the breaker directly rather than
        // waiting for the sync pass to accumulate its own strikes.
        if let Some(pkg) = h.pkg_name.as_deref() {
            let reason = format!(
                "scanner: whole-source outage — {}/{} series failing ({})",
                h.confirmed_failing,
                h.series,
                kind.as_str()
            );
            match crate::catalog::trip_subscription_breaker(&state.pool, pkg, &reason).await {
                Ok(true) => tracing::error!(
                    pkg_name = pkg,
                    "scan: disabled the extension's sync subscription (scan-side outage)"
                ),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(pkg_name = pkg, error = %e, "scan: tripping the subscription breaker failed")
                }
            }
        }
    }
}

/// Park every series of an out-of-service source `SOURCE_OUTAGE_PARK_HOURS` out (jittered
/// ±10%), and return the nominal park horizon for the outage record.
///
/// Only touches rows scheduled inside half the horizon, so re-running it on every failing
/// tick is a near-no-op instead of rewriting the whole cohort — and, importantly, does not
/// keep pushing the trickle of already-parked probes further away.
async fn park_source_series(pool: &SqlitePool, source_id: &str) -> Option<String> {
    let base_minutes = SOURCE_OUTAGE_PARK_HOURS * 60;
    let spread_minutes = (base_minutes / 5).max(1); // ±10%
    let horizon = Utc::now() + chrono::Duration::minutes(base_minutes);
    let half_horizon = (Utc::now() + chrono::Duration::minutes(base_minutes / 2)).to_rfc3339();
    let res = sqlx::query(
        "UPDATE series_scan_state \
            SET next_scan_at = strftime('%Y-%m-%dT%H:%M:%S', 'now', \
                  '+' || (? + ABS(RANDOM()) % ?) || ' minutes') || '+00:00', \
                updated_at   = strftime('%Y-%m-%dT%H:%M:%S', 'now') || '+00:00' \
          WHERE series_id IN (SELECT source_key FROM source_series \
                               WHERE source_type = 'suwayomi' AND source_id = ?) \
            AND next_scan_at < ?",
    )
    .bind(base_minutes - spread_minutes / 2)
    .bind(spread_minutes)
    .bind(source_id)
    .bind(&half_horizon)
    .execute(pool)
    .await;
    match res {
        Ok(r) => {
            if r.rows_affected() > 0 {
                tracing::warn!(
                    source_id,
                    parked = r.rows_affected(),
                    park_hours = SOURCE_OUTAGE_PARK_HOURS,
                    "scan: parked an out-of-service source's series"
                );
            }
            Some(horizon.to_rfc3339())
        }
        Err(e) => {
            tracing::warn!(source_id, error = %e, "scan: parking the source's series failed");
            None
        }
    }
}

/// Pull a recovered source's parked series back into the next hour, spread so the cohort
/// doesn't arrive as one herd (same reasoning as `reclaim_absurd_schedules`). Bounded to
/// rows actually parked out; a series legitimately scheduled soon is left alone.
///
/// This deliberately does not try to tell an OUTAGE park from a PAUSED one
/// (`park_paused`'s 14-day park sorts past the same bound), so a recovered source's paused
/// series get one extra scan before `scan_due` re-parks them. That costs at most one fetch
/// per paused series, once, on a recovery that only happens when a source comes back — and
/// the alternative, remembering which rows this function parked, is per-series state for a
/// self-correcting cost.
async fn unpark_source_series(pool: &SqlitePool, source_id: &str) -> u64 {
    let soon = (Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    sqlx::query(
        "UPDATE series_scan_state \
            SET next_scan_at = strftime('%Y-%m-%dT%H:%M:%S', 'now', \
                  '+' || (ABS(RANDOM()) % 60) || ' minutes') || '+00:00', \
                updated_at   = strftime('%Y-%m-%dT%H:%M:%S', 'now') || '+00:00' \
          WHERE series_id IN (SELECT source_key FROM source_series \
                               WHERE source_type = 'suwayomi' AND source_id = ?) \
            AND next_scan_at > ?",
    )
    .bind(source_id)
    .bind(&soon)
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0)
}

// ── Phase E3: per-source scanners with dynamic supervision ────────────────────────
//
// One scheduler loop PER source instead of one global loop, so a stalling source consumes
// only its own budget and cannot occupy every scan slot and starve the rest. The win is
// ISOLATION, not throughput (E3.5 measured every live source sub-2s and the scanner at ~36%
// utilisation), so per-source `SCAN_CONCURRENCY` stays 3 and a GLOBAL semaphore keeps the
// aggregate across all loops inside the band Suwayomi/FlareSolverr already handle.

/// Per-source tick cadence. A tick's due-set is now one source's, not the whole library, so
/// this can be tight: it drops worst-case polling lag from `SCAN_TICK_SECONDS` (300s) to 60s.
const SOURCE_TICK_SECONDS: u64 = 60;

/// How often the supervisor reconciles its live loops against the DB (on top of explicit
/// kicks). The desired set is ALWAYS derived from the DB, never from in-memory state, so a
/// process restart rebuilds it exactly — the same principle as `next_scan_at` being the timer
/// rather than an in-memory countdown.
const SUPERVISOR_RECONCILE_SECONDS: u64 = 60;

// AGGREGATE scan concurrency across all source loops lives in
// `crate::config::scan_global_concurrency` (`SCAN_GLOBAL_CONCURRENCY`, default 12), read once
// by `supervisor`. 15 loops × 3 = 45 concurrent outbound scanlator fetches would invite
// timeout→backoff cascades and ban risk (E3.4); the semaphore holds the real aggregate in the
// 12–16 band. It is an ENV VAR rather than a constant because a release build is ~13 minutes,
// and the lever that reduces outbound load has to be reachable by a restart. Setting it to 3
// reproduces the pre-E3 aggregate exactly: before E3 there was a single `run_loop` whose
// `for_each_concurrent(SCAN_CONCURRENCY)` was the only source of outbound scan concurrency in
// the process, so 3 permits shared across N loops is the same ceiling by another mechanism.

/// Concurrent scans in flight within ONE source loop.
///
/// **STAYS AT 3 — owner decision, handoff constraint 2.** This is the number the ban risk
/// attaches to, because it is per-scanlator-site: over-concurrency causes upstream timeouts
/// that cascade into `record_scan_failure` backoff and *de-schedule healthy series*.
/// Deliberately NOT env-configurable, unlike the aggregate ceiling — that is a capacity knob,
/// this is a safety constant. Hoisted from `scan_due`'s body so the
/// supervisor can log the effective pair and `per_source_concurrency_stays_at_three` can pin
/// it.
const SCAN_CONCURRENCY: usize = 3;

/// Backstop against a bad data state minting thousands of loops (E3.3). ~15 sources have
/// in-library series today.
const MAX_SOURCE_SCANNERS: usize = 64;

/// How many of those slots per-*source* loops may take. One is permanently reserved for the
/// sweeper, which is what makes "every row is scanned by someone" a total function rather
/// than a property that holds only below the cap — see [`sweeper_cutoff`].
const SOURCE_LOOP_SLOTS: usize = MAX_SOURCE_SCANNERS - 1;

/// The label the unassigned-sweeper loop is keyed by, and its `source_id IS NULL` marker.
const UNASSIGNED_KEY: &str = "__unassigned__";

/// Floor on how often the (library-wide) E4.3 health aggregation may run.
///
/// Its gate is "a loop reported a failure, or an outage is open". Under E3 the supervisor
/// reconciles every 60 s instead of the old 300 s scan tick, and §8e measured **77 of 146
/// ticks carrying ≥1 failure** — plus a standing outage keeps `outage_open` true
/// indefinitely, so the gate would be open essentially forever. Without this floor, the
/// whole-library `source_scan_health` GROUP BY (plus `park_source_series` /
/// `record_source_outage`) would go from ~288 runs/day to ~1,440 purely as a side effect of
/// the tick getting faster. 300 s preserves the pre-E3 cadence exactly.
const HEALTH_CHECK_MIN_SECONDS: u64 = 300;

/// Shared across every source loop: the global concurrency ceiling, plus a failure counter
/// the supervisor drains to decide whether to run the (library-wide) E4.3 health check.
struct ScanCoord {
    sem: Semaphore,
    failures_since_health: AtomicUsize,
}

/// The slice of `series_scan_state` one loop owns.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SourceSel {
    Id(String),
    /// Everything no per-source loop owns — swept on a loop of its own so the sharding can
    /// never orphan a row. Two populations land here:
    ///
    /// * `source_id IS NULL` — freshly enrolled (`ensure_pending` / `record_scan_failure`
    ///   insert without one) or genuinely unmappable, and
    /// * when `above` is set, every `source_id` sorting **above** it: the sources the
    ///   `MAX_SOURCE_SCANNERS` cap left without a loop. See [`sweeper_cutoff`].
    Unassigned {
        above: Option<String>,
    },
}

impl SourceSel {
    fn label(&self) -> &str {
        match self {
            SourceSel::Id(s) => s,
            SourceSel::Unassigned { .. } => UNASSIGNED_KEY,
        }
    }
}

/// The sweeper's residual boundary: `Some(last_assigned_id)` when the per-source loop cap
/// bit, `None` when every source got its own loop.
///
/// THIS IS THE ORPHAN PROOF. `assigned` is the first `slots` distinct `source_id`s **in
/// ascending order** (`distinct_scan_source_ids`), so the set of ids that got a loop is
/// exactly `{ id : id <= assigned.last() }`. Its complement is therefore the single range
/// `id > assigned.last()`, which the sweeper takes. Every `series_scan_state` row is thus in
/// exactly one of three disjoint, exhaustive partitions:
///
/// 1. `source_id IS NULL`                      → sweeper
/// 2. `source_id <= cutoff` (or no cutoff)     → that source's own loop
/// 3. `source_id > cutoff`                     → sweeper
///
/// The previous version simply stopped spawning at the cap and logged "the excess will not
/// be scanned", which was literally true: a clamped-out source's series had a non-NULL
/// `source_id`, so the `source_id IS NULL` sweeper did not match them and **nothing** did.
/// Latent today (~15 sources vs a cap of 64) but silent and unbounded if it ever fired.
///
/// Taking the cutoff whenever `assigned` is *full* (rather than proving there is a 64th
/// source with an extra query) is deliberately conservative: with exactly `slots` sources
/// the residual range matches nothing and the sweeper behaves as before.
fn sweeper_cutoff(assigned: &[String], slots: usize) -> Option<String> {
    if assigned.len() >= slots {
        assigned.last().cloned()
    } else {
        None
    }
}

/// `notify_one` here makes the supervisor reconcile immediately instead of waiting for its
/// interval — used when a source is subscribed or a series enrolled, so a brand-new source
/// starts scanning without a 60s delay (E3.3).
fn supervisor_kick() -> &'static Notify {
    static N: OnceLock<Notify> = OnceLock::new();
    N.get_or_init(Notify::new)
}

/// Kick the per-source-scanner supervisor to reconcile now (E3.3): a new source should start
/// scanning without waiting for the reconcile interval.
pub fn kick_supervisor() {
    supervisor_kick().notify_one();
}

/// Due series ids for one source's slice — the same bounded `next_scan_at <= ?` seek as the
/// library-wide query, pinned to `(source_id, next_scan_at)` (migration 0094). The sweeper's
/// ordinary form uses the `source_id IS NULL` range on the same index.
///
/// The sweeper's *residual* form (`above = Some`) adds `OR source_id > ?`, which is a
/// two-range predicate the composite index cannot serve as one seek — SQLite falls back to
/// the `next_scan_at` range on `idx_scan_state_next_scan` and filters, i.e. exactly the
/// pre-E3 library-wide cost. That only happens when there are more sources than loop slots
/// (64), which is 4× today's source count, and paying a full due-scan there is strictly
/// better than the alternative of not scanning those series at all.
async fn due_series_ids_for_source(
    pool: &SqlitePool,
    sel: &SourceSel,
    now_iso: &str,
    limit: i64,
) -> Vec<String> {
    let sql = match sel {
        SourceSel::Id(_) => {
            "SELECT series_id FROM series_scan_state \
             WHERE source_id = ? AND next_scan_at <= ? \
             ORDER BY next_scan_at ASC LIMIT ?"
        }
        SourceSel::Unassigned { above: None } => {
            "SELECT series_id FROM series_scan_state \
             WHERE source_id IS NULL AND next_scan_at <= ? \
             ORDER BY next_scan_at ASC LIMIT ?"
        }
        SourceSel::Unassigned { above: Some(_) } => {
            "SELECT series_id FROM series_scan_state \
             WHERE (source_id IS NULL OR source_id > ?) AND next_scan_at <= ? \
             ORDER BY next_scan_at ASC LIMIT ?"
        }
    };
    let mut q = sqlx::query_scalar::<_, String>(sql);
    match sel {
        SourceSel::Id(id) => q = q.bind(id),
        SourceSel::Unassigned {
            above: Some(cutoff),
        } => q = q.bind(cutoff),
        SourceSel::Unassigned { above: None } => {}
    }
    q.bind(now_iso)
        .bind(limit)
        .fetch_all(pool)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "scan: per-source due-query failed");
            Vec::new()
        })
}

/// One tick for a single source: pull its due-set and scan it 3-concurrently, each scan also
/// taking a GLOBAL permit so the aggregate across all loops stays bounded.
async fn tick_source(
    state: &AppState,
    sel: &SourceSel,
    coord: &Arc<ScanCoord>,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> TickOutcome {
    use futures::StreamExt as _;
    let due =
        due_series_ids_for_source(&state.pool, sel, &Utc::now().to_rfc3339(), DUE_BATCH_LIMIT)
            .await;
    let due_count = due.len();
    let ok = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    futures::stream::iter(due)
        .for_each_concurrent(SCAN_CONCURRENCY, |series_id| {
            let ok = &ok;
            let failed = &failed;
            let coord = coord.clone();
            async move {
                if *shutdown.borrow() {
                    return;
                }
                // Global aggregate ceiling: hold a permit across the upstream fetch.
                // `acquire` only errors if the semaphore is closed, which it never is.
                let _permit = coord.sem.acquire().await;
                // Timestamp the scan HERE, not from a batch-start `now` (P1-2): a backlog
                // batch can take longer than a series' interval, and a stale `now` schedules
                // the next scan into the past, so the drain never converges.
                match scan_due(state, &series_id, Utc::now()).await {
                    Ok(_) => {
                        ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        // A lock error is NOT backed off (our own write contention, not an
                        // upstream failure); the schedule is intact and the next tick retries.
                        if crate::db::is_locked_error(&e) {
                            tracing::warn!(series_id, error = %e, "scan: skipped this tick on write contention (not backed off)");
                        } else {
                            tracing::warn!(series_id, error = %e, "scan: series scan failed; backed off");
                        }
                    }
                }
            }
        })
        .await;
    let failed = failed.into_inner();
    coord
        .failures_since_health
        .fetch_add(failed, Ordering::Relaxed);
    TickOutcome {
        due: due_count,
        ok: ok.into_inner(),
        failed,
    }
}

/// One source's scan loop: tick every `SOURCE_TICK_SECONDS`, draining a backlog back-to-back
/// while a full batch still makes progress. `shutdown` here is the loop's OWN stop signal
/// (the supervisor sends it on reap and on global shutdown).
async fn run_source_loop(
    state: Arc<AppState>,
    sel: SourceSel,
    coord: Arc<ScanCoord>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // PER-LOOP STAGGER. `interval()` fires its first tick immediately, so every loop the
    // supervisor spawns in one reconcile pass would tick at the same instant — and, being
    // periodic from that start, keep doing so forever. After a restart that means ~15 loops
    // issuing their due-queries and opening their upstream fetches on the same beat every
    // 60 s, which is the same self-sustaining cohort pattern `jitter_interval_hours` exists
    // to prevent, one level up. The global semaphore bounds the concurrency but not the
    // burstiness. A uniform offset inside one tick period spreads them permanently, at the
    // cost of at most one tick period of extra latency on the first tick.
    let stagger_ms = rand_core::OsRng.next_u64() % (SOURCE_TICK_SECONDS * 1_000);
    let mut ticker = interval_at(
        tokio::time::Instant::now() + Duration::from_millis(stagger_ms),
        Duration::from_secs(SOURCE_TICK_SECONDS),
    );
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tracing::info!(source = sel.label(), stagger_ms, "source scan loop started");
    // §7 Phase E / §9: "alert when `due == DUE_BATCH_LIMIT` on consecutive ticks". A saturated
    // batch means the due-query hit its cap, so there is certainly more waiting and the
    // achieved interval is the target PLUS an unbounded queue delay — the failure mode a hard
    // target introduces, and the one that makes a tier's stated cadence a lie. One saturated
    // batch is normal (a cold-start backlog drains through several); consecutive ones across
    // separate TICKS mean the drain is not keeping up. Per-loop, so a single wedged source is
    // attributable rather than smeared across the library.
    let mut saturated_ticks: u32 = 0;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let mut tick_saturated = false;
                loop {
                    let out = tick_source(&state, &sel, &coord, &shutdown).await;
                    tick_saturated |= out.due >= DUE_BATCH_LIMIT as usize;
                    {
                        // Recover from a poisoned lock rather than propagating the panic — a
                        // stale health snapshot is harmless next to a dead loop. Totals
                        // (library_size / overdue) are set by the supervisor; the loops report
                        // the most recent activity.
                        let mut h = state.scan_health.lock().unwrap_or_else(|e| e.into_inner());
                        // Liveness is always refreshed — "is the scanner running at all?".
                        h.last_tick_at = Some(Utc::now().to_rfc3339());
                        // But the ACTIVITY fields are only written by a tick that actually had
                        // work. With N loops on a 60 s beat, the overwhelming majority of ticks
                        // find nothing due (~17 due rows/min library-wide against 16 loops), so
                        // writing them unconditionally meant the admin console read
                        // `scanned_ok = 0` almost always, and — worse — every empty tick reset
                        // `consecutive_stuck_ticks` to 0, which silently disabled E4's "looping
                        // without progress" signal. Sharding the loop must not shard away the
                        // health signal (item 14 promises the display stays answerable).
                        if out.due > 0 {
                            h.scanned_ok = out.ok;
                            h.scanned_failed = out.failed;
                            if out.ok > 0 {
                                h.last_success_at = Some(Utc::now().to_rfc3339());
                            }
                            if out.failed > 0 && out.ok == 0 {
                                h.consecutive_stuck_ticks =
                                    h.consecutive_stuck_ticks.saturating_add(1);
                            } else {
                                h.consecutive_stuck_ticks = 0;
                            }
                        }
                    }
                    if out.due > 0 || out.failed > 0 {
                        tracing::info!(
                            source = sel.label(),
                            overdue = out.due,
                            ok = out.ok,
                            failed = out.failed,
                            "source scan tick complete"
                        );
                    }
                    // Keep draining only while a full batch still advances (a cold-start
                    // backlog); a full batch that scanned nothing (outage) must not tight-loop.
                    let more_due = out.due >= DUE_BATCH_LIMIT as usize;
                    if !more_due || out.ok == 0 || *shutdown.borrow() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(DRAIN_BATCH_DELAY_MS)).await;
                    if *shutdown.borrow() {
                        break;
                    }
                }
                if tick_saturated {
                    saturated_ticks = saturated_ticks.saturating_add(1);
                    if saturated_ticks >= SATURATED_TICK_ALERT {
                        tracing::warn!(
                            source = sel.label(),
                            consecutive_saturated_ticks = saturated_ticks,
                            batch_limit = DUE_BATCH_LIMIT,
                            "scan: due batch saturated on consecutive ticks — demand exceeds \
                             drain capacity, so every tier's achieved interval is its target \
                             PLUS an unbounded queue delay"
                        );
                    }
                } else {
                    saturated_ticks = 0;
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!(source = sel.label(), "source scan loop stopping");
                    break;
                }
            }
        }
    }
}

/// A running source loop plus the handle to stop it.
struct Child {
    stop: tokio::sync::watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

/// Spawn one source loop wrapped in per-child panic supervision (E3.3): a panicking loop
/// restarts after a backoff and cannot take down its siblings or the supervisor.
fn spawn_source_child(state: Arc<AppState>, sel: SourceSel, coord: Arc<ScanCoord>) -> Child {
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let join = tokio::spawn(async move {
        loop {
            let h = tokio::spawn(run_source_loop(
                state.clone(),
                sel.clone(),
                coord.clone(),
                stop_rx.clone(),
            ));
            match h.await {
                Ok(()) => break,
                Err(e) if e.is_panic() => {
                    if *stop_rx.borrow() {
                        break;
                    }
                    tracing::error!(
                        source = sel.label(),
                        "source scan loop panicked; restarting in 10s"
                    );
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    if *stop_rx.borrow() {
                        break;
                    }
                }
                Err(_) => break, // cancelled
            }
        }
    });
    Child {
        stop: stop_tx,
        join,
    }
}

/// Assign a `source_id` to any scan-state row that can be mapped but hasn't been yet (freshly
/// enrolled), so it moves from the unassigned sweeper onto its own source's loop. Cheap: the
/// `EXISTS` guard stops it rewriting genuinely-unmappable rows every pass, so it is bounded by
/// the (normally zero) count of newly-enrolled NULL rows. Returns rows healed.
async fn heal_null_source_ids(pool: &SqlitePool) -> u64 {
    sqlx::query(
        "UPDATE series_scan_state \
            SET source_id = (SELECT ss.source_id FROM source_series ss \
                              WHERE ss.source_type = 'suwayomi' \
                                AND ss.source_key = series_scan_state.series_id LIMIT 1) \
          WHERE source_id IS NULL \
            AND EXISTS (SELECT 1 FROM source_series ss \
                         WHERE ss.source_type = 'suwayomi' \
                           AND ss.source_key = series_scan_state.series_id)",
    )
    .execute(pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "scan: source_id heal failed");
        0
    })
}

/// Distinct non-null `source_id`s that have at least one scan-state row — the sources that
/// need a loop — capped at `limit` and returned in **ascending order**.
///
/// The ordering is load-bearing, not cosmetic: it is what makes the assigned set a prefix of
/// the sorted id space, so its complement is the single range `source_id > last` that
/// [`sweeper_cutoff`] hands to the sweeper. An unordered `LIMIT` would leave an arbitrary,
/// reconcile-to-reconcile-varying hole that no range can describe.
async fn distinct_scan_source_ids(pool: &SqlitePool, limit: usize) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT source_id FROM series_scan_state WHERE source_id IS NOT NULL \
         ORDER BY source_id ASC LIMIT ?",
    )
    .bind(limit as i64)
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "scan: distinct-source query failed");
        Vec::new()
    })
}

/// Compute the reconcile delta: which loops to spawn, and which to reap. Pure, so the
/// set-difference logic is unit-tested without spawning tasks. `desired` always includes the
/// unassigned sweeper; a source is reaped only when it has no series left (coverage is kept
/// regardless of subscription state — a disabled source's series are parked by E4.3, not
/// orphaned here).
fn reconcile_delta(
    running: &HashSet<String>,
    desired: &HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    let to_spawn: Vec<String> = desired.difference(running).cloned().collect();
    let to_reap: Vec<String> = running.difference(desired).cloned().collect();
    (to_spawn, to_reap)
}

/// Spawn the per-source scan supervisor. Runs until `shutdown` resolves. (`_tick_seconds` is
/// the legacy library-wide tick; per-source loops use `SOURCE_TICK_SECONDS`. The parameter is
/// kept so the `main.rs` call site is unchanged.)
pub fn spawn(
    state: Arc<AppState>,
    _tick_seconds: u64,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(supervisor(state, shutdown));
}

async fn supervisor(state: Arc<AppState>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    // Read ONCE, here: the permit count is fixed for the life of the semaphore, so re-reading
    // it per loop could only produce loops that disagree about the ceiling.
    let permits = crate::config::scan_global_concurrency();
    let coord = Arc::new(ScanCoord {
        sem: Semaphore::new(permits),
        failures_since_health: AtomicUsize::new(0),
    });
    let mut children: HashMap<String, Child> = HashMap::new();
    // The sweeper always runs, so a row with no `source_id` yet (or an unmappable one, or one
    // on a source the loop cap left unassigned) is never orphaned by the sharding.
    let mut sweeper_above: Option<String> = None;
    children.insert(
        UNASSIGNED_KEY.to_string(),
        spawn_source_child(
            state.clone(),
            SourceSel::Unassigned { above: None },
            coord.clone(),
        ),
    );

    let mut reconcile = interval(Duration::from_secs(SUPERVISOR_RECONCILE_SECONDS));
    reconcile.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let kick = supervisor_kick();
    let mut last_health: Option<tokio::time::Instant> = None;
    let mut last_drift: Option<tokio::time::Instant> = None;
    // Logged so the effective ceiling is answerable from the logs alone — an env var that
    // silently did not take effect is the failure mode a knob like this invites.
    tracing::info!(
        global_permits = permits,
        per_source_concurrency = SCAN_CONCURRENCY,
        "scan supervisor started"
    );

    loop {
        let now = Utc::now();
        // Belt-and-braces against legacy far-future schedules (migration 0057) — bounded,
        // and a no-op once drained. Once per reconcile, not per source tick.
        reclaim_absurd_schedules(&state.pool, now).await;
        heal_null_source_ids(&state.pool).await;

        // The sources that get their own loop: the first `SOURCE_LOOP_SLOTS` distinct ids in
        // ascending order. Anything past that boundary is covered by the sweeper's residual
        // range rather than dropped — see `sweeper_cutoff`.
        let assigned = distinct_scan_source_ids(&state.pool, SOURCE_LOOP_SLOTS).await;
        let want_above = sweeper_cutoff(&assigned, SOURCE_LOOP_SLOTS);
        let mut desired: HashSet<String> = assigned.into_iter().collect();
        desired.insert(UNASSIGNED_KEY.to_string());

        // The cap bit: re-point the sweeper at the residual range. Respawning is the whole
        // change — the sweeper is stateless, and this only ever happens when the source count
        // crosses 63 (4× today's).
        if want_above != sweeper_above {
            match want_above.as_deref() {
                Some(cutoff) => tracing::error!(
                    cap = SOURCE_LOOP_SLOTS,
                    cutoff,
                    "scan supervisor: source count reached the per-source loop cap — sources \
                     sorting above the cutoff are now swept by the unassigned loop, not dropped"
                ),
                None => tracing::info!(
                    cap = SOURCE_LOOP_SLOTS,
                    "scan supervisor: source count back under the loop cap — every source has \
                     its own loop again"
                ),
            }
            sweeper_above = want_above;
            if let Some(child) = children.remove(UNASSIGNED_KEY) {
                tokio::spawn(async move {
                    let _ = child.stop.send(true);
                    let _ = child.join.await;
                });
            }
        }

        let running: HashSet<String> = children.keys().cloned().collect();
        let (to_spawn, to_reap) = reconcile_delta(&running, &desired);
        for key in to_spawn {
            let sel = if key == UNASSIGNED_KEY {
                SourceSel::Unassigned {
                    above: sweeper_above.clone(),
                }
            } else {
                SourceSel::Id(key.clone())
            };
            children.insert(
                key.clone(),
                spawn_source_child(state.clone(), sel, coord.clone()),
            );
            tracing::info!(source = %key, "scan supervisor: source loop spawned");
        }
        for key in to_reap {
            if let Some(child) = children.remove(&key) {
                // Signal stop and wait for a graceful exit in a detached task, so `stop`
                // stays alive until the loop observes it (dropping it early would close the
                // channel and the loop could miss the signal).
                tokio::spawn(async move {
                    let _ = child.stop.send(true);
                    let _ = child.join.await;
                });
                tracing::info!(source = %key, "scan supervisor: source has no series left — loop retired");
            }
        }

        // Admin health snapshot: the library-wide totals, once per reconcile.
        {
            let total = scan_state_count(&state.pool).await;
            let due: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM series_scan_state WHERE next_scan_at <= ?",
            )
            .bind(now.to_rfc3339())
            .fetch_one(&state.pool)
            .await
            .unwrap_or(0);
            let mut h = state.scan_health.lock().unwrap_or_else(|e| e.into_inner());
            h.library_size = total;
            h.overdue_count = due as usize;
        }

        // E4.3 library-wide health check, gated so a clean steady state costs nothing: run it
        // only when a loop reported failures since the last reconcile, or an outage is open
        // (recovery happens on a pass full of successes, so the outage would otherwise linger)
        // — and at most every `HEALTH_CHECK_MIN_SECONDS`, so the faster E3 reconcile beat does
        // not multiply the whole-library aggregate by 5×.
        let ready = last_health
            .map(|t| t.elapsed() >= Duration::from_secs(HEALTH_CHECK_MIN_SECONDS))
            .unwrap_or(true);
        if ready && !*shutdown.borrow() {
            // `load` + `fetch_sub`, not `swap(0)`: a failure a loop records between the two
            // must survive to the NEXT check rather than being silently discarded, and when
            // the throttle skips this pass the counter must not be cleared at all.
            let failed = coord.failures_since_health.load(Ordering::Relaxed);
            let outage_open = crate::catalog::any_source_outage(&state.pool)
                .await
                .unwrap_or(false);
            if failed > 0 || outage_open {
                coord
                    .failures_since_health
                    .fetch_sub(failed, Ordering::Relaxed);
                last_health = Some(tokio::time::Instant::now());
                check_source_health(&state).await;
            }
        }

        // Phase E's achieved-vs-target instrument (§10). Pure in-process atomics — no query,
        // no lock — but throttled anyway so each window carries enough samples to mean
        // something and the log stays readable at the 60 s reconcile beat.
        let drift_ready = last_drift
            .map(|t: tokio::time::Instant| {
                t.elapsed() >= Duration::from_secs(TIER_DRIFT_REPORT_MIN_SECONDS)
            })
            .unwrap_or(true);
        if drift_ready && !*shutdown.borrow() {
            last_drift = Some(tokio::time::Instant::now());
            report_tier_drift();
        }

        tokio::select! {
            _ = reconcile.tick() => {}
            _ = kick.notified() => {
                tracing::debug!("scan supervisor: kicked — reconciling now");
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("scan supervisor stopping");
                    for (_, child) in children.drain() {
                        let _ = child.stop.send(true);
                    }
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    thread_local! {
        /// Schedule jitter is OFF by default in tests — see `jitter_interval_hours`.
        static JITTER_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub(super) fn jitter_enabled() -> bool {
        JITTER_ENABLED.with(|c| c.get())
    }

    /// The jitter itself: bounded to ±`SCHEDULE_JITTER_FRACTION` and genuinely random,
    /// which is what breaks up the self-sustaining scan cohort in production.
    #[test]
    fn jitter_stays_within_band_and_varies() {
        JITTER_ENABLED.with(|c| c.set(true));
        let base = 100.0_f64;
        let lo = base * (1.0 - SCHEDULE_JITTER_FRACTION);
        let hi = base * (1.0 + SCHEDULE_JITTER_FRACTION);
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..500 {
            let v = jitter_interval_hours(base);
            assert!(v >= lo && v <= hi, "jittered {v} outside [{lo}, {hi}]");
            distinct.insert((v * 1_000_000.0) as i64);
        }
        JITTER_ENABLED.with(|c| c.set(false));
        assert!(
            distinct.len() > 400,
            "jitter must actually vary (got {} distinct of 500)",
            distinct.len()
        );
        assert_eq!(jitter_interval_hours(0.0), 0.0, "zero stays zero");
    }

    /// **THE PARK-BELOW-HORIZON INVARIANT** (§8a — the 2033-parking bug).
    ///
    /// `reclaim_absurd_schedules` unconditionally drags every row scheduled past
    /// `ABSURD_HORIZON_HOURS` back into the next hour. So if ANY writer here can schedule a
    /// row beyond that horizon — *including the top of its jitter window* — that writer is
    /// silently a no-op: the park is written, the reclaim undoes it, and the only visible
    /// symptom is a scanner that never stops re-scanning "parked" series. That is exactly
    /// the failure a 60-day `PAUSED_PARK_HOURS` against a 16-day horizon would have caused.
    ///
    /// The arithmetic, spelled out so a future change can be checked by hand:
    ///
    /// | writer | base | jitter | worst case |
    /// |---|---|---|---|
    /// | `park_paused` | 336 h | `± (336/5)/2` = ±33 h | **369 h** |
    /// | `park_source_series` | 168 h | `+ 168/5` (ONE-SIDED) = +33 h | **201 h** |
    /// | `record_scan{,_policy}` | ≤ `MAX_INTERVAL_HOURS` = 336 h | `× 1.10` | **369.6 h** |
    ///
    /// max = 370 h (369.6 rounded up) < `ABSURD_HORIZON_HOURS` = 384 h. Margin: **14 h**.
    ///
    /// `MAX_SCHEDULED_HOURS < ABSURD_HORIZON_HOURS` is additionally asserted in a `const _`
    /// next to the constants, so a violation fails the BUILD. This test is the runtime half:
    /// it checks the constants agree with what the live RNG actually produces, which a
    /// compile-time assertion over a hand-written formula cannot.
    #[test]
    fn every_park_writer_stays_below_the_absurd_horizon() {
        // The declared ceiling really is below the horizon (the const assertion, restated
        // so a failure names the numbers).
        assert!(
            MAX_SCHEDULED_HOURS < ABSURD_HORIZON_HOURS,
            "max scheduled {MAX_SCHEDULED_HOURS}h must stay strictly below the \
             {ABSURD_HORIZON_HOURS}h reclaim horizon, or every park is undone"
        );
        // Each writer's declared worst case is under the horizon on its own...
        assert!(MAX_PARKED_HOURS < ABSURD_HORIZON_HOURS);
        assert!(MAX_OUTAGE_PARKED_HOURS < ABSURD_HORIZON_HOURS);
        assert!(MAX_SCHEDULED_INTERVAL_HOURS < ABSURD_HORIZON_HOURS as f64);
        // ...and `MAX_SCHEDULED_HOURS` really is the max of the three.
        assert!(MAX_SCHEDULED_HOURS >= MAX_PARKED_HOURS);
        assert!(MAX_SCHEDULED_HOURS >= MAX_OUTAGE_PARKED_HOURS);
        assert!(MAX_SCHEDULED_HOURS as f64 >= MAX_SCHEDULED_INTERVAL_HOURS);

        // The LIVE park path: sample the real RNG-backed function, not a copy of its
        // arithmetic. Every draw must sit inside the declared band AND below the horizon.
        let lo = PAUSED_PARK_HOURS - PARK_JITTER_SPREAD_HOURS / 2;
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..2_000 {
            let v = park_offset_hours();
            assert!(
                v >= lo && v <= MAX_PARKED_HOURS,
                "park offset {v}h outside [{lo}, {MAX_PARKED_HOURS}]"
            );
            assert!(
                v < ABSURD_HORIZON_HOURS,
                "park offset {v}h would be reclaimed by the {ABSURD_HORIZON_HOURS}h horizon"
            );
            distinct.insert(v);
        }
        // Handoff constraint 3: the park's jitter is never degenerate. Without spread a
        // parked cohort returns as one herd (~745 series / 35 min, 43% duty cycle, 154 GB).
        assert!(
            distinct.len() > 1,
            "park jitter collapsed to a single value — a parked cohort would re-cluster"
        );

        // The steady/legacy path's ceiling is reachable only via an admin override, and it
        // is clamped before the jitter is applied — so the clamp is what bounds it.
        assert_eq!(resolve_interval(Some(f64::MAX), 0.0), MAX_INTERVAL_HOURS);
        assert_eq!(
            scan_interval_hours(
                ComicType::Manga,
                None,
                at("2026-01-01T00:00:00Z"),
                Some(1e12)
            ),
            MAX_INTERVAL_HOURS
        );
    }

    /// Handoff constraint 2. Per-source concurrency is the number the ban risk attaches to,
    /// and it is the one thing E3 must NOT have changed. Pinned so a future capacity tweak
    /// cannot raise it by accident while reaching for the aggregate knob next to it.
    #[test]
    fn per_source_concurrency_stays_at_three() {
        assert_eq!(SCAN_CONCURRENCY, 3);
        // The aggregate knob is a separate, larger, env-overridable ceiling — never a
        // substitute for the per-source one.
        assert!(crate::config::DEFAULT_SCAN_GLOBAL_CONCURRENCY >= SCAN_CONCURRENCY);
    }

    /// Unit A: the aggregate ceiling is a restart-level knob (a release build is ~13 min), and
    /// a 0 would deadlock every source loop forever behind a semaphore that never issues a
    /// permit — which looks exactly like a healthy idle library from outside.
    ///
    /// Env vars are process-global, so this test sets and clears them in one place rather than
    /// racing sibling tests over the same variable.
    #[test]
    fn the_global_concurrency_knob_parses_and_never_reaches_zero() {
        use crate::config::scan_global_concurrency;
        std::env::remove_var("SCAN_GLOBAL_CONCURRENCY");
        assert_eq!(
            scan_global_concurrency(),
            crate::config::DEFAULT_SCAN_GLOBAL_CONCURRENCY,
            "unset must fall back to the documented default"
        );
        // The emergency setting: 3 permits reproduces the pre-E3 aggregate (one loop's worth).
        std::env::set_var("SCAN_GLOBAL_CONCURRENCY", "3");
        assert_eq!(scan_global_concurrency(), SCAN_CONCURRENCY);
        std::env::set_var("SCAN_GLOBAL_CONCURRENCY", " 6 ");
        assert_eq!(scan_global_concurrency(), 6, "whitespace must be tolerated");
        // Zero and garbage must never disable the scanner.
        std::env::set_var("SCAN_GLOBAL_CONCURRENCY", "0");
        assert_eq!(
            scan_global_concurrency(),
            1,
            "0 permits would deadlock every loop"
        );
        std::env::set_var("SCAN_GLOBAL_CONCURRENCY", "not-a-number");
        assert_eq!(
            scan_global_concurrency(),
            crate::config::DEFAULT_SCAN_GLOBAL_CONCURRENCY
        );
        std::env::set_var("SCAN_GLOBAL_CONCURRENCY", "-4");
        assert_eq!(
            scan_global_concurrency(),
            12,
            "a negative parses as garbage → default"
        );
        std::env::remove_var("SCAN_GLOBAL_CONCURRENCY");
    }

    /// Unit A wiring: the env var name must match `deploy/docker-compose.yml` exactly, and so
    /// must the default. A mismatch means the knob silently does nothing in production — the
    /// §8i/§8j "inert but green" pattern, applied to a safety lever.
    #[test]
    fn the_concurrency_knob_matches_the_compose_wiring() {
        let compose = include_str!("../../../deploy/docker-compose.yml");
        assert!(
            compose.contains("SCAN_GLOBAL_CONCURRENCY: ${SCAN_GLOBAL_CONCURRENCY:-12}"),
            "deploy/docker-compose.yml must pass SCAN_GLOBAL_CONCURRENCY with default \
             {}, or the knob is unreachable in production",
            crate::config::DEFAULT_SCAN_GLOBAL_CONCURRENCY
        );
        assert_eq!(crate::config::DEFAULT_SCAN_GLOBAL_CONCURRENCY, 12);
    }

    /// Unit C: the drift instrument must attribute a sample to the right tier, ignore anything
    /// that has no tier target (admin overrides, the legacy path), and reject nonsense.
    #[test]
    fn tier_drift_indexes_only_real_tiers() {
        assert_eq!(tier_index(TIER_FAST_ACTIVE_HOURS), Some(0));
        assert_eq!(tier_index(TIER_MANGA_HOURS), Some(1));
        assert_eq!(tier_index(TIER_FAST_DORMANT_HOURS), Some(2));
        // An admin override or a legacy inferred cadence has no target to drift from.
        assert_eq!(tier_index(7.0), None);
        assert_eq!(tier_index(MAX_INTERVAL_HOURS), None);
        // The three tiers are distinct, or two of them would share a bucket and their means
        // would contaminate each other.
        assert_eq!(
            TIER_TARGETS
                .iter()
                .map(|t| t.to_bits())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            TIER_TARGETS.len()
        );
        // A non-positive achieved interval (clock skew) is dropped, not recorded as a sample.
        let before = TIER_DRIFT[1].samples.load(Ordering::Relaxed);
        record_tier_achieved(TIER_MANGA_HOURS, 0.0);
        record_tier_achieved(TIER_MANGA_HOURS, -3.0);
        record_tier_achieved(999.0, 12.0); // no such tier
        assert_eq!(TIER_DRIFT[1].samples.load(Ordering::Relaxed), before);
    }

    /// Unit C: the reporting threshold is §10's "< 10% per tier", and the saturation alert is
    /// §7 Phase E's "alert when `due == DUE_BATCH_LIMIT` on consecutive ticks" — CONSECUTIVE,
    /// so a single cold-start drain batch must not alert.
    #[test]
    fn the_drift_and_saturation_thresholds_match_the_spec() {
        assert_eq!(TIER_DRIFT_WARN_FRACTION, 0.10);
        assert!(
            SATURATED_TICK_ALERT >= 2,
            "a single saturated batch is an ordinary drain, not an alert"
        );
        assert!(
            TIER_DRIFT_MIN_SAMPLES > 0,
            "reporting a mean over zero samples would divide by zero"
        );
    }

    /// Handoff constraint 3, at the constant: the park jitter window can never be zero-width
    /// (a `% 0` would also panic), regardless of how small `PAUSED_PARK_HOURS` becomes.
    #[test]
    fn park_jitter_is_never_degenerate() {
        assert!(
            PARK_JITTER_SPREAD_HOURS >= 1,
            "park jitter spread must be >= 1h; zero spread re-creates the thundering herd"
        );
        // ...and at the current park it is a real ±10% window, not the degenerate floor.
        assert_eq!(PARK_JITTER_SPREAD_HOURS, PAUSED_PARK_HOURS / 5);
    }

    /// E2's trigger selects "parked" rows by `next_scan_at` distance. That threshold has to
    /// sit ABOVE the slowest ordinary tier (or it wakes merely-slow series on every LATEST
    /// walk) and BELOW the park (or it selects nothing). Pinned because both ends move: the
    /// tiers are Phase E's and the park is E2's.
    #[test]
    fn the_trigger_threshold_separates_slow_tiers_from_parked_rows() {
        assert!(
            TRIGGER_PARKED_AFTER_HOURS as f64 > TIER_FAST_DORMANT_HOURS,
            "the trigger would wake dormant-tier series, not parked ones"
        );
        assert!(
            TRIGGER_PARKED_AFTER_HOURS < PAUSED_PARK_HOURS - PARK_JITTER_SPREAD_HOURS / 2,
            "the trigger must select every parked row, including the shortest jitter draw"
        );
    }

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn chap(upload_date: Option<&str>) -> SuwayomiChapter {
        chap_n(1, 1.0, upload_date)
    }

    fn chap_n(id: i64, number: f64, upload_date: Option<&str>) -> SuwayomiChapter {
        SuwayomiChapter {
            id,
            manga_id: 1,
            name: "c".into(),
            chapter_number: number,
            scanlator: None,
            upload_date: upload_date.map(|s| s.to_string()),
            is_read: false,
            is_bookmarked: false,
            is_downloaded: false,
            last_page_read: 0,
            page_count: 0,
        }
    }

    /// N chapters numbered 1..=N, undated (count-only fixtures).
    fn chaps(n: i64) -> Vec<SuwayomiChapter> {
        (1..=n).map(|i| chap_n(i, i as f64, None)).collect()
    }

    /// N chapters on a regular `gap_hours` cadence, the newest uploaded at
    /// `latest`. Dated fixtures are what the lateness gate needs: the accelerated
    /// poll now requires a cadence averaged over at least `MIN_TRUSTED_GAPS` real
    /// upload gaps plus a newest-upload timestamp to measure lateness against.
    fn dated_chaps(n: i64, gap_hours: i64, latest: DateTime<Utc>) -> Vec<SuwayomiChapter> {
        (0..n)
            .map(|i| {
                let ts = latest - chrono::Duration::hours(gap_hours * i);
                chap_n(
                    n - i,
                    (n - i) as f64,
                    Some(&ts.timestamp_millis().to_string()),
                )
            })
            .collect()
    }

    async fn migrated_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[test]
    fn unscheduled_series_is_due() {
        // No next_scan_at (never scanned) -> always due.
        assert!(is_due(None, at("2026-01-01T00:00:00Z")));
    }

    #[test]
    fn future_next_scan_is_not_due() {
        assert!(!is_due(
            Some("2026-01-02T00:00:00Z"),
            at("2026-01-01T00:00:00Z")
        ));
    }

    #[test]
    fn past_next_scan_is_due() {
        assert!(is_due(
            Some("2025-12-31T00:00:00Z"),
            at("2026-01-01T00:00:00Z")
        ));
    }

    #[test]
    fn poll_cadence_is_clamped() {
        // Default poll (30m) passes through under a weekly steady interval.
        assert_eq!(resolve_poll_minutes(None, 168.0), DEFAULT_POLL_MINUTES);
        // A 1-minute poll floors to MIN_POLL_MINUTES (can't out-run the tick).
        assert_eq!(resolve_poll_minutes(Some(1), 168.0), MIN_POLL_MINUTES);
        // A poll slower than the steady interval collapses onto it (no negative
        // "acceleration"): steady 1h = 60m caps a 120m poll.
        assert_eq!(resolve_poll_minutes(Some(120), 1.0), 60.0);
    }

    #[test]
    fn avg_interval_needs_two_dated_chapters() {
        assert_eq!(avg_interval_hours(&[]), None);
        assert_eq!(avg_interval_hours(&[chap(Some("1000"))]), None);
        // two chapters 24h apart (epoch seconds)
        let day = 24 * 3600;
        let a = chap(Some("1000000"));
        let b = chap(Some(&(1_000_000 + day).to_string()));
        let avg = avg_interval_hours(&[a, b]).unwrap();
        assert!((avg - 24.0).abs() < 0.001, "expected ~24h, got {avg}");
    }

    #[test]
    fn max_interval_clamp_avoids_duration_overflow() {
        // The absurd-override guard clamps to MAX_INTERVAL_HOURS; the resulting
        // Duration math must not overflow or panic (this was the m2 finding).
        let ms = (MAX_INTERVAL_HOURS * 3_600_000.0) as i64;
        let now = at("2026-01-01T00:00:00Z");
        let future = now + chrono::Duration::milliseconds(ms);
        assert!(future > now);
    }

    #[test]
    fn inferred_interval_clamps_up_to_min() {
        // A same-day burst (avg 0.2h) must be floored to MIN, not left sub-hour.
        assert_eq!(resolve_interval(None, 0.2), MIN_INTERVAL_HOURS);
        // No inferable cadence -> default (which is itself within [MIN, ACTIVE_MAX]).
        assert_eq!(resolve_interval(None, 0.0), DEFAULT_INTERVAL_HOURS);
    }

    /// P0-1: the steady POLL cadence must not track the PUBLICATION cadence. A weekly
    /// or fortnightly series is still *checked* at the active ceiling — polling at the
    /// upload rate is what put the median discovery lag at 28.8h with a p90 of 11 days.
    #[test]
    fn steady_cadence_is_capped_below_the_publication_cadence() {
        for avg in [24.0, 168.0, 336.0, 58_309.0] {
            assert_eq!(
                resolve_interval(None, avg),
                ACTIVE_MAX_INTERVAL_HOURS,
                "an inferred {avg}h cadence must still be polled at the active ceiling"
            );
        }
        // Below the ceiling the inferred cadence is still honoured (down to MIN).
        assert_eq!(resolve_interval(None, 8.0), 8.0);
        // The long ceiling stays reachable for a DELIBERATE admin override only.
        assert_eq!(resolve_interval(Some(336.0), 6.0), 336.0);
        assert!(ACTIVE_MAX_INTERVAL_HOURS < MAX_INTERVAL_HOURS);
    }

    /// P1-1: "awaiting" must mean genuinely late, not merely "a scheduled scan found
    /// nothing" — the latter was true by construction for every scheduler-driven scan.
    #[test]
    fn lateness_band_excludes_on_time_and_dead_series() {
        let weekly = 168.0;
        assert!(!is_genuinely_late(weekly, 100.0), "not due yet");
        assert!(
            !is_genuinely_late(weekly, weekly * LATE_FACTOR),
            "exactly at the late threshold is not yet late"
        );
        assert!(is_genuinely_late(weekly, weekly * 2.0), "slipping: late");
        assert!(
            !is_genuinely_late(weekly, weekly * LATE_BAND_MAX + 1.0),
            "past the band the series has stopped publishing, not slipped"
        );
    }

    /// P0-2 root cause: a sparse history spanning years must not be trusted as a cadence.
    #[test]
    fn sparse_history_is_not_a_trustworthy_cadence() {
        let now = at("2026-01-01T00:00:00Z");
        // Two chapters six years apart: a 6.6-year "average", which is what parked a
        // live ONGOING series until 2033-03-14.
        let sparse = dated_chaps(2, 6 * 365 * 24, now);
        let c = upload_cadence(&sparse).unwrap();
        assert!(c.gaps < MIN_TRUSTED_GAPS, "one gap is not a cadence");
        // Five dated chapters on a real weekly rhythm are trusted.
        let real = dated_chaps(5, 168, now);
        assert!(upload_cadence(&real).unwrap().gaps >= MIN_TRUSTED_GAPS);
    }

    #[test]
    fn override_bypasses_min_but_not_hard_floor() {
        // A deliberate override may go below MIN...
        assert_eq!(resolve_interval(Some(2.0), 168.0), 2.0);
        // ...but never below the hard floor that protects upstreams.
        assert_eq!(resolve_interval(Some(0.01), 168.0), HARD_MIN_INTERVAL_HOURS);
        // Upper clamp still holds.
        assert_eq!(resolve_interval(Some(1e9), 0.0), MAX_INTERVAL_HOURS);
    }

    // ---- record_scan bookkeeping (DB-backed) ----

    async fn persisted(pool: &SqlitePool, series_id: &str) -> ScanState {
        scan_state(pool, series_id)
            .await
            .expect("scan state row should exist")
    }

    #[tokio::test]
    async fn first_observation_records_baseline_without_flagging_new() {
        // SC3: a fresh series with a full back catalogue must NOT be flagged as
        // "new" on first sight — record the baseline count, leave last_new NULL.
        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();
        let now = at("2026-01-01T00:00:00Z");

        let new_found = record_scan(&pool, "1", "S", &admin, &chaps(12), now)
            .await
            .unwrap();
        assert!(!new_found, "first observation must not report new chapters");

        let row = persisted(&pool, "1").await;
        assert_eq!(row.known_chapter_count, 12);
        assert!(
            row.last_new_chapter_at.is_none(),
            "first observation must not stamp last_new_chapter_at"
        );
    }

    #[tokio::test]
    async fn subsequent_scan_flags_new_chapter() {
        // Steady state after a baseline: an added chapter IS reported and stamps
        // last_new_chapter_at.
        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();

        record_scan(
            &pool,
            "1",
            "S",
            &admin,
            &chaps(12),
            at("2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();

        let then = at("2026-01-08T00:00:00Z");
        let new_found = record_scan(&pool, "1", "S", &admin, &chaps(13), then)
            .await
            .unwrap();
        assert!(new_found, "an added chapter must be reported");

        let row = persisted(&pool, "1").await;
        assert_eq!(row.known_chapter_count, 13);
        assert_eq!(
            row.last_new_chapter_at.as_deref(),
            Some(then.to_rfc3339()).as_deref()
        );
    }

    #[tokio::test]
    async fn count_stable_but_higher_number_is_new() {
        // SC4: upstream drops one chapter and adds a higher-numbered one within an
        // interval — count is unchanged (3 -> 3) but the max number advanced.
        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();
        let base = vec![
            chap_n(1, 1.0, None),
            chap_n(2, 2.0, None),
            chap_n(3, 3.0, None),
        ];
        let churned = vec![
            chap_n(1, 1.0, None),
            chap_n(2, 2.0, None),
            chap_n(4, 4.0, None),
        ];

        record_scan(&pool, "1", "S", &admin, &base, at("2026-01-01T00:00:00Z"))
            .await
            .unwrap();
        assert_eq!(persisted(&pool, "1").await.known_max_chapter, Some(3.0));

        let new_found = record_scan(
            &pool,
            "1",
            "S",
            &admin,
            &churned,
            at("2026-01-02T00:00:00Z"),
        )
        .await
        .unwrap();
        assert!(new_found, "a higher chapter number must count as new");
        assert_eq!(persisted(&pool, "1").await.known_max_chapter, Some(4.0));
    }

    #[tokio::test]
    async fn max_chapter_heals_on_shrink_without_flagging() {
        // Removing the top chapter is not a new chapter (set-difference: no id
        // present now was absent before). Unlike the old monotonic high-water mark,
        // known_max now HEALS down to the current list's real max (SC4), so a
        // transient garbage number can't pin it forever.
        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();
        let full = vec![
            chap_n(1, 1.0, None),
            chap_n(2, 2.0, None),
            chap_n(3, 3.0, None),
        ];
        let trimmed = vec![chap_n(1, 1.0, None), chap_n(2, 2.0, None)];

        record_scan(&pool, "1", "S", &admin, &full, at("2026-01-01T00:00:00Z"))
            .await
            .unwrap();
        let new_found = record_scan(
            &pool,
            "1",
            "S",
            &admin,
            &trimmed,
            at("2026-01-02T00:00:00Z"),
        )
        .await
        .unwrap();
        assert!(!new_found, "a shrink must not report a new chapter");
        let row = persisted(&pool, "1").await;
        assert_eq!(
            row.known_chapter_count, 2,
            "count follows upstream downward"
        );
        assert_eq!(
            row.known_max_chapter,
            Some(2.0),
            "max heals to the current list's real max"
        );
    }

    #[tokio::test]
    async fn sentinel_number_does_not_pin_known_max() {
        // A garbage/sentinel chapter number (Suwayomi's -1.0, or an absurd value)
        // must not become the stored known_max — otherwise number-derived state is
        // pinned forever. The robust max ignores it and uses the real top chapter.
        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();
        let with_sentinel = vec![
            chap_n(1, 1.0, None),
            chap_n(2, 2.0, None),
            chap_n(3, -1.0, None),
            chap_n(4, 999_999.0, None),
        ];
        record_scan(
            &pool,
            "1",
            "S",
            &admin,
            &with_sentinel,
            at("2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(
            persisted(&pool, "1").await.known_max_chapter,
            Some(2.0),
            "sentinels are excluded from known_max"
        );
    }

    #[tokio::test]
    async fn removal_offsetting_insertion_is_flagged() {
        // A removal that offsets an insertion at/below the current max (count flat,
        // max unchanged) is still a new chapter via set-difference of identities.
        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();
        // ids {10,11,12}, numbers 1,2,3.
        let base = vec![
            chap_n(10, 1.0, None),
            chap_n(11, 2.0, None),
            chap_n(12, 3.0, None),
        ];
        // Drop id 11 (number 2), add id 13 renumbered 2 — count flat (3), max flat.
        let churned = vec![
            chap_n(10, 1.0, None),
            chap_n(13, 2.0, None),
            chap_n(12, 3.0, None),
        ];
        record_scan(&pool, "1", "S", &admin, &base, at("2026-01-01T00:00:00Z"))
            .await
            .unwrap();
        let new_found = record_scan(
            &pool,
            "1",
            "S",
            &admin,
            &churned,
            at("2026-01-02T00:00:00Z"),
        )
        .await
        .unwrap();
        assert!(
            new_found,
            "a new chapter identity must be flagged even with flat count and max"
        );
    }

    /// Hours between `now` and a series' persisted `next_scan_at`.
    async fn hours_until_next_scan(pool: &SqlitePool, series_id: &str, now: DateTime<Utc>) -> f64 {
        let next = persisted(pool, series_id)
            .await
            .next_scan_at
            .expect("next_scan_at should be scheduled");
        let next = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        (next - now).num_milliseconds() as f64 / 3_600_000.0
    }

    #[tokio::test]
    async fn awaiting_series_repolls_at_poll_cadence_then_reverts() {
        // SC1 (re-stated for the P1-1 policy): a weekly series whose chapter is
        // GENUINELY late — newest upload older than 1.25 weeks — re-polls at
        // poll_every_minutes (30m). Once a chapter lands it reverts to the steady
        // cadence and the streak is cleared.
        let pool = migrated_pool().await;
        let admin = ScanAdmin {
            override_interval_hours: Some(168.0),
            poll_every_minutes: Some(30),
            ..Default::default()
        };

        // Baseline: 6 weekly chapters, newest uploaded right now -> on schedule.
        let t0 = at("2026-01-01T00:00:00Z");
        let base = dated_chaps(6, 168, t0);
        record_scan(&pool, "1", "S", &admin, &base, t0)
            .await
            .unwrap();
        assert!(
            (hours_until_next_scan(&pool, "1", t0).await - 168.0).abs() < 0.01,
            "first observation schedules the steady interval"
        );

        // Nine days on with no new upload: 216h since the newest chapter against a
        // 168h cadence is 1.29x — inside the lateness band -> accelerated 30m poll.
        let t1 = at("2026-01-10T00:00:00Z");
        let new_found = record_scan(&pool, "1", "S", &admin, &base, t1)
            .await
            .unwrap();
        assert!(!new_found);
        assert!(
            (hours_until_next_scan(&pool, "1", t1).await - 0.5).abs() < 0.01,
            "a genuinely late series re-polls at the 30-minute poll cadence"
        );
        assert!(persisted(&pool, "1").await.awaiting_since.is_some());

        // Chapter finally lands -> revert to the steady interval.
        let t2 = at("2026-01-10T00:30:00Z");
        let landed = dated_chaps(7, 168, t2);
        let new_found = record_scan(&pool, "1", "S", &admin, &landed, t2)
            .await
            .unwrap();
        assert!(new_found);
        assert!(
            (hours_until_next_scan(&pool, "1", t2).await - 168.0).abs() < 0.01,
            "a landed chapter reverts to the steady cadence"
        );
        assert!(
            persisted(&pool, "1").await.awaiting_since.is_none(),
            "awaiting_since is cleared once a chapter lands"
        );
    }

    // ── Phase E: status/format-aware cadence (F6) ────────────────────────────────

    #[test]
    fn scan_interval_hours_picks_the_format_tier() {
        let now = at("2026-01-15T00:00:00Z");
        let recent = Some(at("2026-01-10T00:00:00Z")); // 5 days ago -> active
        let old = Some(at("2025-12-01T00:00:00Z")); // > 14 days -> dormant
                                                    // Fast formats, active -> 3h.
        assert_eq!(
            scan_interval_hours(ComicType::Manhwa, recent, now, None),
            3.0
        );
        assert_eq!(
            scan_interval_hours(ComicType::Manhua, recent, now, None),
            3.0
        );
        assert_eq!(
            scan_interval_hours(ComicType::Webtoon, recent, now, None),
            3.0
        );
        // Fast formats, dormant (or never released) -> 24h.
        assert_eq!(scan_interval_hours(ComicType::Manhwa, old, now, None), 24.0);
        assert_eq!(
            scan_interval_hours(ComicType::Manhwa, None, now, None),
            24.0
        );
        // Manga/comic -> 12h regardless of recency.
        assert_eq!(
            scan_interval_hours(ComicType::Manga, recent, now, None),
            12.0
        );
        assert_eq!(scan_interval_hours(ComicType::Comic, old, now, None), 12.0);
    }

    #[test]
    fn admin_override_beats_the_tier_and_is_clamped() {
        let now = at("2026-01-15T00:00:00Z");
        let recent = Some(at("2026-01-14T00:00:00Z"));
        // An override wins over the 3h manhwa tier.
        assert_eq!(
            scan_interval_hours(ComicType::Manhwa, recent, now, Some(48.0)),
            48.0
        );
        // Clamped to the hard floor and the absurdity ceiling.
        assert_eq!(
            scan_interval_hours(ComicType::Manhwa, recent, now, Some(0.1)),
            HARD_MIN_INTERVAL_HOURS
        );
        assert_eq!(
            scan_interval_hours(ComicType::Manga, recent, now, Some(1_000_000.0)),
            MAX_INTERVAL_HOURS
        );
    }

    #[tokio::test]
    async fn the_tier_lookup_resolves_a_real_type() {
        // THE GUARD THAT WAS MISSING. Every Phase E test built a `ComicType` by hand and
        // called `record_scan_policy` directly, so none of them touched the lookup that turns
        // a series id into one — and that lookup selected a column (`work.comic_type`) which
        // has never existed, silently tiering the entire library as Manga.
        let pool = migrated_pool().await;
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('w1', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ('ss1', 'w1', 'suwayomi', 'srcA', '7', '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // No feed/browse row yet -> the safe slow default, not a panic.
        assert_eq!(scan_comic_type(&pool, "7").await, ComicType::Manga);
        // An unmapped series id likewise.
        assert_eq!(scan_comic_type(&pool, "404").await, ComicType::Manga);

        // The browse row types it; the feed row (this module's own output) overrides it.
        sqlx::query(
            "INSERT INTO browse_catalogue (work_id, reader_id, title, comic_type, created_at) \
             VALUES ('w1', 'w1', 'T', 'MANHUA', '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(scan_comic_type(&pool, "7").await, ComicType::Manhua);
        sqlx::query(
            "INSERT INTO feed_series_updates (work_id, reader_id, title, comic_type, released_at) \
             VALUES ('w1', 'w1', 'T', 'MANHWA', 1700000000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            scan_comic_type(&pool, "7").await,
            ComicType::Manhwa,
            "a real materialized type reaches the tier engine — this is what was broken"
        );
    }

    #[test]
    fn the_dormancy_boundary_is_exact() {
        // `Duration::num_days()` truncates, so the original `num_days() <= 14` really meant
        // "< 15 days" and kept a series on the 3h tier a full day past the documented cutoff.
        let now = at("2026-01-15T00:00:00Z");
        let exactly_14d = Some(at("2026-01-01T00:00:00Z"));
        let one_second_past = Some(at("2025-12-31T23:59:59Z"));
        assert_eq!(
            scan_interval_hours(ComicType::Manhwa, exactly_14d, now, None),
            TIER_FAST_ACTIVE_HOURS,
            "released exactly DORMANT_AFTER_DAYS ago is still active"
        );
        assert_eq!(
            scan_interval_hours(ComicType::Manhwa, one_second_past, now, None),
            TIER_FAST_DORMANT_HOURS,
            "one second past the cutoff is dormant — no free extra day"
        );
    }

    #[test]
    fn newest_upload_does_not_need_a_cadence() {
        let t0 = at("2026-01-01T00:00:00Z");
        // Two dated chapters -> a cadence exists, and both agree on the newest upload.
        let two = dated_chaps(2, 72, t0);
        assert_eq!(newest_upload(&two), Some(t0));
        assert_eq!(upload_cadence(&two).map(|c| c.latest_upload), Some(t0));
        // ONE dated chapter -> no cadence at all, but the last release is still known.
        let one = dated_chaps(1, 72, t0);
        assert!(
            upload_cadence(&one).is_none(),
            "one timestamp yields no rhythm"
        );
        assert_eq!(
            newest_upload(&one),
            Some(t0),
            "…but the dormancy test only needs the newest upload, and it is right there"
        );
        assert_eq!(newest_upload(&[]), None);
    }

    #[tokio::test]
    async fn a_brand_new_single_chapter_manhwa_is_not_filed_as_dormant() {
        // Taking `last_release` from `UploadCadence` inherited its "needs >= 2 timestamps"
        // precondition, so a manhwa on its FIRST chapter — exactly the cohort the 3h tier
        // exists for — reported "never released" and got the 24h dormant tier.
        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();
        let t0 = at("2026-01-01T00:00:00Z");
        let one = dated_chaps(1, 72, t0);
        record_scan_policy(&pool, "1", "S", &admin, &one, Some(ComicType::Manhwa), t0)
            .await
            .unwrap();
        assert!(
            (hours_until_next_scan(&pool, "1", t0).await - TIER_FAST_ACTIVE_HOURS).abs() < 0.01,
            "a chapter released today is active regardless of how many chapters exist"
        );
    }

    #[test]
    fn fast_tier_gets_a_tighter_jitter() {
        assert_eq!(tier_jitter_fraction(3.0), 0.05);
        assert_eq!(tier_jitter_fraction(12.0), SCHEDULE_JITTER_FRACTION);
        assert_eq!(tier_jitter_fraction(24.0), SCHEDULE_JITTER_FRACTION);
    }

    #[tokio::test]
    async fn policy_mode_uses_the_tier_and_never_accelerates() {
        // The legacy path would put this genuinely-late weekly series into the 30-minute
        // awaiting poll (see `awaiting_series_repolls_at_poll_cadence_then_reverts`). On the
        // policy path (a real `comic_type`), the interval is the flat 12h manga tier and
        // `awaiting` is off — E5's LATEST-diff push signal is the acceleration now.
        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();
        let t0 = at("2026-01-01T00:00:00Z");
        let base = dated_chaps(6, 168, t0);
        record_scan_policy(&pool, "1", "S", &admin, &base, Some(ComicType::Manga), t0)
            .await
            .unwrap();
        assert!(
            (hours_until_next_scan(&pool, "1", t0).await - 12.0).abs() < 0.01,
            "manga tier is 12h, not the inferred 168h cadence"
        );
        // Nine days on, no new upload — the lateness band the legacy path accelerates in.
        let t1 = at("2026-01-10T00:00:00Z");
        record_scan_policy(&pool, "1", "S", &admin, &base, Some(ComicType::Manga), t1)
            .await
            .unwrap();
        assert!(
            (hours_until_next_scan(&pool, "1", t1).await - 12.0).abs() < 0.01,
            "policy mode never accelerates into the 30-minute awaiting poll"
        );
        assert!(
            persisted(&pool, "1").await.awaiting_since.is_none(),
            "policy mode leaves awaiting_since unset"
        );
    }

    #[tokio::test]
    async fn active_manhwa_gets_the_three_hour_tier() {
        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();
        let t0 = at("2026-01-01T00:00:00Z");
        // Newest chapter at t0 -> released today -> active fast tier.
        let base = dated_chaps(6, 72, t0);
        record_scan_policy(&pool, "1", "S", &admin, &base, Some(ComicType::Manhwa), t0)
            .await
            .unwrap();
        assert!(
            (hours_until_next_scan(&pool, "1", t0).await - 3.0).abs() < 0.01,
            "an active manhwa scans on the 3h tier"
        );
    }

    #[tokio::test]
    async fn on_time_and_dead_series_never_enter_the_accelerated_poll() {
        // P1-1: `due_series_ids` only ever returns rows that are already due, so
        // "came due and found nothing" was true for EVERY scheduler scan and routed
        // ~73% of the fetch budget to a few hundred series. Neither an on-schedule
        // series nor a long-dead one may accelerate.
        let pool = migrated_pool().await;
        let admin = ScanAdmin {
            override_interval_hours: Some(168.0),
            poll_every_minutes: Some(30),
            ..Default::default()
        };
        let t0 = at("2026-01-01T00:00:00Z");

        // (a) On schedule: the scan comes due at 170h but the newest upload is only
        // 170h old against a 168h cadence (1.01x) — the chapter isn't late yet.
        let fresh = dated_chaps(6, 168, t0);
        record_scan(&pool, "on_time", "S", &admin, &fresh, t0)
            .await
            .unwrap();
        let t1 = t0 + chrono::Duration::hours(170); // due, still nothing new
        record_scan(&pool, "on_time", "S", &admin, &fresh, t1)
            .await
            .unwrap();
        assert!(
            (hours_until_next_scan(&pool, "on_time", t1).await - 168.0).abs() < 0.01,
            "an on-schedule series stays on the steady cadence"
        );
        assert!(persisted(&pool, "on_time").await.awaiting_since.is_none());

        // (b) Dead: newest upload 10 weeks ago against a weekly cadence (10x, well
        // past LATE_BAND_MAX). It has stopped publishing, not slipped.
        let stale = dated_chaps(6, 168, t0 - chrono::Duration::hours(168 * 10));
        record_scan(&pool, "dead", "S", &admin, &stale, t0)
            .await
            .unwrap();
        let t2 = t0 + chrono::Duration::hours(200);
        record_scan(&pool, "dead", "S", &admin, &stale, t2)
            .await
            .unwrap();
        assert!(
            (hours_until_next_scan(&pool, "dead", t2).await - 168.0).abs() < 0.01,
            "a series past the lateness band is not accelerated"
        );
        assert!(persisted(&pool, "dead").await.awaiting_since.is_none());
    }

    #[tokio::test]
    async fn absurd_avg_interval_is_clamped_at_write_time() {
        // P0-2: the multi-year "averages" that parked live rows in 2031–2033 came from
        // sparse histories. Clamp them where they are WRITTEN, not only where they are
        // turned into a schedule, so the stored value (and everything reading it) is
        // sane too.
        let pool = migrated_pool().await;
        let now = at("2026-01-01T00:00:00Z");
        // Two chapters 6 years apart -> a ~58,000h raw average.
        let sparse = dated_chaps(2, 6 * 365 * 24, now);
        record_scan(&pool, "1", "S", &ScanAdmin::default(), &sparse, now)
            .await
            .unwrap();
        let row = persisted(&pool, "1").await;
        assert!(
            row.avg_interval_hours <= MAX_INTERVAL_HOURS,
            "stored avg must be clamped, got {}",
            row.avg_interval_hours
        );
        // ...and the schedule it produced stays inside the active ceiling.
        let hours = hours_until_next_scan(&pool, "1", now).await;
        assert!(
            hours <= ACTIVE_MAX_INTERVAL_HOURS + 0.01,
            "sparse history must not park the series, got {hours}h"
        );
    }

    #[tokio::test]
    async fn early_manual_scan_does_not_enter_awaiting() {
        // SC1/item-2: an admin triggerScan that force-scans a series BEFORE its
        // cadence (not yet due) and finds nothing must NOT flip it into the
        // accelerated poll — that would be a false "overdue".
        let pool = migrated_pool().await;
        let admin = ScanAdmin {
            override_interval_hours: Some(168.0),
            poll_every_minutes: Some(30),
            ..Default::default()
        };

        let t0 = at("2026-01-01T00:00:00Z");
        record_scan(&pool, "1", "S", &admin, &chaps(5), t0)
            .await
            .unwrap();

        // Force-scan just 1h later — long before the 168h cadence.
        let early = at("2026-01-01T01:00:00Z");
        record_scan(&pool, "1", "S", &admin, &chaps(5), early)
            .await
            .unwrap();
        assert!(
            (hours_until_next_scan(&pool, "1", early).await - 168.0).abs() < 0.01,
            "a not-yet-due scan keeps the steady cadence"
        );
        assert!(
            persisted(&pool, "1").await.awaiting_since.is_none(),
            "a not-yet-due scan does not enter awaiting"
        );
    }

    #[tokio::test]
    async fn first_real_scan_of_preregistered_series_is_not_awaiting() {
        // Regression (verify pass): enrol paths (add_source_series/federated_ingest) and
        // the reconcile backfill now pre-seed a due-now `series_scan_state` row via
        // ensure_pending — which has NO chapter snapshot. The first REAL scan must still be
        // treated as a baseline (steady cadence, not awaiting), even though a row already
        // exists (so `first_observation` is false). Without gating awaiting on
        // `have_prior_snapshot`, every newly-enrolled series would flip into the 30-min
        // accelerated poll for up to 48h on its first scan.
        let pool = migrated_pool().await;
        let admin = ScanAdmin {
            override_interval_hours: Some(168.0),
            poll_every_minutes: Some(30),
            ..Default::default()
        };
        // Pre-register with a due-now sentinel row + no snapshot (what ensure_pending does).
        ensure_pending(&pool, "1").await.unwrap();
        let t0 = at("2026-01-01T00:00:00Z");
        let new_found = record_scan(&pool, "1", "S", &admin, &chaps(5), t0)
            .await
            .unwrap();
        assert!(
            !new_found,
            "first real scan is a baseline, not new chapters"
        );
        assert!(
            (hours_until_next_scan(&pool, "1", t0).await - 168.0).abs() < 0.01,
            "a pre-registered series' first scan schedules the STEADY interval, not the poll cadence"
        );
        assert!(
            persisted(&pool, "1").await.awaiting_since.is_none(),
            "a pre-registered series' first scan must not enter awaiting"
        );
    }

    #[tokio::test]
    async fn awaiting_backs_off_after_window_then_re_arms_after_the_cooldown() {
        // SC1 + P0-3. A late series accelerates for `min(publication, 48h)`, then falls
        // back to the steady cadence — but unlike before, the streak is not pinned
        // forever: once `AWAITING_REARM_HOURS` elapses a fresh window may open. Live,
        // 1,679 of 2,201 awaiting series had been awaiting for more than 48h and could
        // never re-accelerate again.
        let pool = migrated_pool().await;
        // No override: the steady cadence is the ACTIVE ceiling, which is the shape a
        // real series now has.
        let admin = ScanAdmin {
            poll_every_minutes: Some(30),
            ..Default::default()
        };

        // Baseline: 6 chapters on a 168h rhythm, newest at t0.
        let t0 = at("2026-01-01T00:00:00Z");
        let base = dated_chaps(6, 168, t0);
        record_scan(&pool, "1", "S", &admin, &base, t0)
            .await
            .unwrap();
        assert!(
            (hours_until_next_scan(&pool, "1", t0).await - ACTIVE_MAX_INTERVAL_HOURS).abs() < 0.01,
            "the steady cadence is the active ceiling, NOT the 168h publication gap"
        );

        // 216h in (1.29x cadence) the chapter is late -> awaiting opens, 30m poll.
        let t1 = t0 + chrono::Duration::hours(216);
        record_scan(&pool, "1", "S", &admin, &base, t1)
            .await
            .unwrap();
        assert_eq!(
            persisted(&pool, "1").await.awaiting_since.as_deref(),
            Some(t1.to_rfc3339()).as_deref(),
            "the awaiting streak starts at the first genuinely-late scan"
        );
        assert!((hours_until_next_scan(&pool, "1", t1).await - 0.5).abs() < 0.01);

        // 50h later (> the 48h window) -> back to the steady cadence, streak preserved
        // as the cool-down marker.
        let t2 = t1 + chrono::Duration::hours(50);
        record_scan(&pool, "1", "S", &admin, &base, t2)
            .await
            .unwrap();
        assert!(
            (hours_until_next_scan(&pool, "1", t2).await - ACTIVE_MAX_INTERVAL_HOURS).abs() < 0.01,
            "past the awaiting window the series reverts to the steady cadence"
        );
        assert_eq!(
            persisted(&pool, "1").await.awaiting_since.as_deref(),
            Some(t1.to_rfc3339()).as_deref(),
            "the streak start is held as the cool-down marker, not re-stamped every scan"
        );

        // Past the cool-down (and still inside the lateness band: 457h/168h = 2.7x) the
        // window re-arms with a FRESH start, so acceleration is not lost forever.
        let t3 = t1 + chrono::Duration::hours(AWAITING_REARM_HOURS as i64 + 1);
        record_scan(&pool, "1", "S", &admin, &base, t3)
            .await
            .unwrap();
        assert_eq!(
            persisted(&pool, "1").await.awaiting_since.as_deref(),
            Some(t3.to_rfc3339()).as_deref(),
            "the streak re-arms once the cool-down has elapsed"
        );
        assert!(
            (hours_until_next_scan(&pool, "1", t3).await - 0.5).abs() < 0.01,
            "a re-armed window polls at the accelerated cadence again"
        );
    }

    #[tokio::test]
    async fn reclaim_pulls_legacy_far_future_rows_back_into_the_due_set() {
        // P0-2 read-side net: rows parked beyond ABSURD_HORIZON_HOURS can never satisfy
        // `next_scan_at <= now` and are invisible to the scheduler forever. Production
        // had 3,578 such rows, the furthest scheduled for 2033-03-14.
        let pool = migrated_pool().await;
        let now = Utc::now();
        let far = (now + chrono::Duration::days(2500)).to_rfc3339();
        let ok = (now + chrono::Duration::hours(6)).to_rfc3339();
        put_state(&pool, "parked_in_2033", Some(&far)).await;
        put_state(&pool, "normal", Some(&ok)).await;

        let n = reclaim_absurd_schedules(&pool, now).await;
        assert_eq!(n, 1, "only the absurd row is reclaimed");
        let reclaimed = next_scan_of(&pool, "parked_in_2033").await.unwrap();
        assert!(
            reclaimed < (now + chrono::Duration::hours(2)).to_rfc3339(),
            "reclaimed row is scheduled within the hour, got {reclaimed}"
        );
        assert_eq!(
            next_scan_of(&pool, "normal").await.as_deref(),
            Some(ok.as_str()),
            "a normally-scheduled row is untouched"
        );
        // Idempotent: the tail is drained, so a second pass is a no-op.
        assert_eq!(reclaim_absurd_schedules(&pool, now).await, 0);
    }

    #[tokio::test]
    async fn repeated_identical_scan_does_not_double_count() {
        // SC6: the read-prior + upsert is a single transaction and `count` is set
        // from the fresh list (not incremented), so a triggerScan repeating a tick
        // over the same chapters can't double-count or re-flag.
        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();

        record_scan(
            &pool,
            "1",
            "S",
            &admin,
            &chaps(7),
            at("2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        let after_first = persisted(&pool, "1").await;

        let new_found = record_scan(
            &pool,
            "1",
            "S",
            &admin,
            &chaps(7),
            at("2026-01-01T00:05:00Z"),
        )
        .await
        .unwrap();
        assert!(!new_found, "an identical re-scan reports no new chapters");
        let after_second = persisted(&pool, "1").await;
        assert_eq!(
            after_second.known_chapter_count, 7,
            "count is not double-counted"
        );
        assert_eq!(
            after_first.known_chapter_count,
            after_second.known_chapter_count
        );
        assert!(
            after_second.last_new_chapter_at.is_none(),
            "an identical re-scan does not stamp last_new_chapter_at"
        );
    }

    /// End-to-end smoke test against a *live* Suwayomi. Ignored by default (needs a
    /// running Suwayomi with a seeded library); run with:
    ///   KOMIKA_LIVE_SUWAYOMI=http://localhost:4567 \
    ///     cargo test --bin komika-server -- --ignored live_suwayomi_end_to_end --nocapture
    /// It validates the real GraphQL queries/deserialization (which the synthetic
    /// unit tests can't) and drives `record_scan` off real chapter data.
    #[tokio::test]
    #[ignore = "requires a live Suwayomi with a seeded library (KOMIKA_LIVE_SUWAYOMI)"]
    async fn live_suwayomi_end_to_end() {
        let Ok(base) = std::env::var("KOMIKA_LIVE_SUWAYOMI") else {
            eprintln!("skipped: set KOMIKA_LIVE_SUWAYOMI=http://localhost:4567");
            return;
        };
        let client = crate::suwayomi::SuwayomiClient::new(base, None, None);

        // (1) Real Suwayomi schema: library()/series()/chapters() deserialize.
        let lib = client.library().await.expect("library()");
        assert!(
            !lib.is_empty(),
            "seed a series into the Suwayomi library first"
        );
        let m = lib.into_iter().next().unwrap();
        let detail = client.series(m.id).await.expect("series()");
        assert_eq!(detail.id, m.id, "series() resolves the same manga");
        let chapters = client.chapters(m.id).await.expect("chapters()");
        assert!(!chapters.is_empty(), "expected the series to have chapters");

        // (2) Real millisecond uploadDate strings yield a positive finite cadence.
        let avg = avg_interval_hours(&chapters);
        if let Some(a) = avg {
            assert!(a > 0.0 && a.is_finite(), "avg_interval_hours = {a}");
        }

        let count = chapters.len() as i64;
        let real_max = latest_number(&chapters).expect("a max chapter number");
        eprintln!(
            "live '{}' (#{}) — {} chapters, max #{}, avg_interval_hours={:?}",
            m.title, m.id, count, real_max, avg
        );

        let pool = migrated_pool().await;
        let admin = ScanAdmin::default();
        let sid = m.id.to_string();
        let now = at("2026-07-15T00:00:00Z");

        // (3) First observation over REAL data: baseline recorded, nothing flagged
        // as new (SC3), and known_max_chapter captured even though it exceeds the
        // count (SC4 — here 166.5 > 102 for Oshi no Ko).
        let new_found = record_scan(&pool, &sid, &m.title, &admin, &chapters, now)
            .await
            .unwrap();
        assert!(
            !new_found,
            "first observation must not flag the back catalogue"
        );
        let row = persisted(&pool, &sid).await;
        assert_eq!(row.known_chapter_count, count);
        assert_eq!(row.known_max_chapter, Some(real_max));
        assert!(row.last_new_chapter_at.is_none());

        // (4) Identical re-scan a minute later: not due, no double-count (SC6).
        let again = record_scan(
            &pool,
            &sid,
            &m.title,
            &admin,
            &chapters,
            now + chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
        assert!(!again, "an identical re-scan reports no new chapters");
        assert_eq!(persisted(&pool, &sid).await.known_chapter_count, count);

        // (5) Splice a higher-numbered chapter onto the real list and mark the
        // series due: SC4 detects the number advance; a landed chapter clears
        // awaiting and reverts to steady cadence (SC1).
        let mut plus = chapters.clone();
        let mut top = plus[0].clone();
        top.id = i64::MAX;
        top.chapter_number = real_max + 1.0;
        plus.push(top);
        sqlx::query("UPDATE series_scan_state SET next_scan_at = NULL WHERE series_id = ?")
            .bind(&sid)
            .execute(&pool)
            .await
            .unwrap();
        let landed = record_scan(
            &pool,
            &sid,
            &m.title,
            &admin,
            &plus,
            now + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        assert!(landed, "a higher chapter number must be detected as new");
        let row = persisted(&pool, &sid).await;
        assert_eq!(row.known_max_chapter, Some(real_max + 1.0));
        assert!(row.last_new_chapter_at.is_some());
        assert!(
            row.awaiting_since.is_none(),
            "a landed chapter is not awaiting"
        );
    }

    // ── DB-driven work-selection ─────────────────────────────────────────────────

    /// Insert a scan-state row. `None` models a "due now" series — stored as the
    /// `DUE_NOW_SENTINEL` (the real representation), NOT a raw NULL, so it matches the
    /// bounded `next_scan_at <= ?` due-query like a real freshly-enrolled row.
    async fn put_state(pool: &SqlitePool, id: &str, next_scan_at: Option<&str>) {
        sqlx::query(
            "INSERT INTO series_scan_state (series_id, next_scan_at, updated_at) VALUES (?, ?, ?)",
        )
        .bind(id)
        .bind(next_scan_at.unwrap_or(DUE_NOW_SENTINEL))
        .bind("2024-01-01T00:00:00Z")
        .execute(pool)
        .await
        .unwrap();
    }

    async fn next_scan_of(pool: &SqlitePool, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT next_scan_at FROM series_scan_state WHERE series_id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn due_query_takes_null_and_past_orders_nulls_first_and_limits() {
        let pool = migrated_pool().await;
        let now = at("2024-01-01T00:00:00Z");
        put_state(&pool, "sentinel", None).await; // never scanned (sentinel) -> due now
        put_state(&pool, "past", Some("2023-12-31T00:00:00Z")).await; // overdue
        put_state(&pool, "future", Some("2024-02-01T00:00:00Z")).await; // not due

        let due = due_series_ids(&pool, &now.to_rfc3339(), 10).await;
        assert_eq!(
            due,
            vec!["sentinel".to_string(), "past".to_string()],
            "sentinel (due-now) sorts first, then overdue; future is excluded"
        );

        let capped = due_series_ids(&pool, &now.to_rfc3339(), 1).await;
        assert_eq!(capped, vec!["sentinel".to_string()], "LIMIT is honored");
    }

    #[tokio::test]
    async fn ensure_pending_is_due_now_and_never_clobbers() {
        let pool = migrated_pool().await;
        let now = at("2024-01-01T00:00:00Z");

        // Fresh row is due now — stored as the sentinel (never NULL) and selected by the
        // bounded due-query.
        ensure_pending(&pool, "42").await.unwrap();
        assert_eq!(
            next_scan_of(&pool, "42").await.as_deref(),
            Some(DUE_NOW_SENTINEL)
        );
        assert!(
            due_series_ids(&pool, &now.to_rfc3339(), 10)
                .await
                .contains(&"42".to_string()),
            "a sentinel row is due now"
        );

        // Park it, then re-run ensure_pending: the parked schedule must survive.
        park_paused(&pool, "42", now).await.unwrap();
        let parked = next_scan_of(&pool, "42").await;
        assert!(parked.is_some());
        ensure_pending(&pool, "42").await.unwrap();
        assert_eq!(
            next_scan_of(&pool, "42").await,
            parked,
            "ensure_pending must not disturb an existing row"
        );
    }

    #[tokio::test]
    async fn park_pushes_next_scan_out_and_drops_from_due_set() {
        let pool = migrated_pool().await;
        let now = at("2024-01-01T00:00:00Z");
        put_state(&pool, "done", None).await; // starts due
        park_paused(&pool, "done", now).await.unwrap();

        let next = next_scan_of(&pool, "done").await.unwrap();
        let hours = (at(&next) - now).num_hours();
        // Parked ~PAUSED_PARK_HOURS out, but jittered by ±(PAUSED_PARK_HOURS/10) to avoid a
        // cold-start cohort re-clustering into a thundering herd 14 days later (audit #9).
        let spread = PAUSED_PARK_HOURS / 5;
        assert!(
            (hours - PAUSED_PARK_HOURS).abs() <= spread,
            "parked ~{PAUSED_PARK_HOURS}h out (±{spread}), got {hours}h"
        );

        let due = due_series_ids(&pool, &now.to_rfc3339(), 10).await;
        assert!(
            !due.contains(&"done".to_string()),
            "parked series isn't due"
        );
    }

    async fn failures_of(pool: &SqlitePool, id: &str) -> i64 {
        sqlx::query_scalar("SELECT consecutive_failures FROM series_scan_state WHERE series_id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn record_scan_failure_backs_off_exponentially_then_resets_on_success() {
        // audit #1/#4: a failing scan must push next_scan_at out (growing) and bump the
        // failure counter, and a later success must clear the counter.
        let pool = migrated_pool().await;
        let now = at("2024-01-01T00:00:00Z");
        put_state(&pool, "dead", None).await; // due now

        record_scan_failure(&pool, "dead", now, FailureKind::FetchError)
            .await
            .unwrap();
        let after1 = (at(&next_scan_of(&pool, "dead").await.unwrap()) - now).num_minutes();
        assert_eq!(failures_of(&pool, "dead").await, 1);
        assert_eq!(
            after1, ERROR_BACKOFF_BASE_MINUTES,
            "first failure backs off base"
        );

        record_scan_failure(&pool, "dead", now, FailureKind::FetchError)
            .await
            .unwrap();
        let after2 = (at(&next_scan_of(&pool, "dead").await.unwrap()) - now).num_minutes();
        assert_eq!(failures_of(&pool, "dead").await, 2);
        assert_eq!(
            after2,
            ERROR_BACKOFF_BASE_MINUTES * 2,
            "second failure doubles"
        );

        // A successful scan resets the counter to 0.
        record_scan(&pool, "dead", "S", &ScanAdmin::default(), &chaps(3), now)
            .await
            .unwrap();
        assert_eq!(
            failures_of(&pool, "dead").await,
            0,
            "a success clears the failure streak"
        );
    }

    #[tokio::test]
    async fn backed_off_failure_no_longer_starves_a_healthy_overdue_series() {
        // audit #4: after a failure backs a dead series off into the future, a genuinely
        // overdue healthy series sorts AHEAD of it in the due-ordering (no starvation).
        let pool = migrated_pool().await;
        let now = at("2024-02-01T00:00:00Z");
        put_state(&pool, "dead", None).await; // was pinned due-now
        record_scan_failure(&pool, "dead", now, FailureKind::FetchError)
            .await
            .unwrap(); // now backed off ~30m out
        put_state(&pool, "healthy", Some("2024-01-31T00:00:00Z")).await; // overdue (past)

        let due = due_series_ids(&pool, &now.to_rfc3339(), 10).await;
        assert_eq!(
            due.first().map(String::as_str),
            Some("healthy"),
            "an overdue healthy series is no longer stuck behind the backed-off dead id"
        );
        assert!(
            !due.contains(&"dead".to_string()),
            "the backed-off dead id has left the current due-set"
        );
    }

    /// E4.2 (F11). The failure KIND is what makes a silent breakage visible, so it must be
    /// persisted on failure and cleared by a real success — the same lifecycle as
    /// `consecutive_failures`, which it exists to disambiguate.
    #[tokio::test]
    async fn a_cached_fallback_is_recorded_as_its_own_failure_kind_and_cleared_on_success() {
        let pool = migrated_pool().await;
        let now = at("2024-01-01T00:00:00Z");
        put_state(&pool, "quietly_broken", None).await;

        record_scan_failure(&pool, "quietly_broken", now, FailureKind::CachedFallback)
            .await
            .unwrap();
        let (kind, at_iso): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT last_failure_kind, last_failure_at FROM series_scan_state \
             WHERE series_id = 'quietly_broken'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            kind.as_deref(),
            Some("cached_fallback"),
            "a scan served from Suwayomi's cache must be distinguishable from a clean error"
        );
        assert_eq!(at_iso.as_deref(), Some(now.to_rfc3339().as_str()));
        assert_eq!(failures_of(&pool, "quietly_broken").await, 1);

        record_scan(
            &pool,
            "quietly_broken",
            "S",
            &ScanAdmin::default(),
            &chaps(3),
            now,
        )
        .await
        .unwrap();
        let (kind, at_iso): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT last_failure_kind, last_failure_at FROM series_scan_state \
             WHERE series_id = 'quietly_broken'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            kind.is_none() && at_iso.is_none(),
            "a real scan clears the failure, so the columns always describe the CURRENT state"
        );
    }

    /// E4.3. The outage predicate needs BOTH guards: the share alone condemns a 1-series
    /// source with one dead entry, and the floor alone condemns a large healthy source with
    /// a handful of deleted series.
    #[test]
    fn source_outage_needs_both_a_floor_and_a_share() {
        // Genz Toons: 209 of 209. The case this whole phase exists for.
        assert!(is_source_out_of_service(209, 209));
        // A big source with a few dead entries is NOT an outage.
        assert!(!is_source_out_of_service(861, 12));
        // Small sources are exempt from the automatic action, however bad the ratio.
        assert!(!is_source_out_of_service(2, 2));
        assert!(!is_source_out_of_service(4, 4));
        // Right at the floor, fully failing.
        assert!(is_source_out_of_service(5, 5));
        // At the floor but only half the source: not yet.
        assert!(!is_source_out_of_service(10, 5));
    }

    /// E4.3. A dead source must stop costing a fetch per series per day — but the park has to
    /// be idempotent (it runs on every failing tick) and must not push the trickle of probes
    /// ever further out, or the source could never be found to have recovered.
    #[tokio::test]
    async fn parking_an_out_of_service_source_is_idempotent_and_reversible() {
        let pool = migrated_pool().await;
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('w', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for key in ["10", "11"] {
            sqlx::query(
                "INSERT INTO source_series \
                   (id, work_id, source_type, source_id, source_key, created_at) \
                 VALUES (?, 'w', 'suwayomi', 'dead_src', ?, '2024-01-01T00:00:00Z')",
            )
            .bind(format!("ss_{key}"))
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
            put_state(&pool, key, None).await; // due now
        }
        // A series on ANOTHER source, to prove the park is scoped.
        sqlx::query(
            "INSERT INTO source_series \
               (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ('ss_20', 'w', 'suwayomi', 'live_src', '20', '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        put_state(&pool, "20", None).await;

        let parked_until = park_source_series(&pool, "dead_src").await;
        assert!(parked_until.is_some(), "the park reports its horizon");
        let first = next_scan_of(&pool, "10").await.unwrap();
        let hours = (at(&first) - Utc::now()).num_hours();
        assert!(
            (hours - SOURCE_OUTAGE_PARK_HOURS).abs() <= SOURCE_OUTAGE_PARK_HOURS / 5,
            "parked ~{SOURCE_OUTAGE_PARK_HOURS}h out (±10%), got {hours}h"
        );
        assert_eq!(
            next_scan_of(&pool, "20").await.as_deref(),
            Some(DUE_NOW_SENTINEL),
            "a healthy source's series is untouched"
        );

        // Idempotent: a second pass leaves the existing parks alone rather than pushing them
        // out again, so the weekly probe actually happens.
        park_source_series(&pool, "dead_src").await;
        assert_eq!(
            next_scan_of(&pool, "10").await.as_deref(),
            Some(first.as_str()),
            "re-parking an already-parked series must not defer its probe"
        );

        // Recovery pulls the cohort back into the next hour instead of making it wait out
        // the rest of the week.
        let unparked = unpark_source_series(&pool, "dead_src").await;
        assert_eq!(unparked, 2, "both series of the source are unparked");
        let back = at(&next_scan_of(&pool, "10").await.unwrap());
        assert!(
            back <= Utc::now() + chrono::Duration::hours(1),
            "unparked series come due within the hour, got {back}"
        );
    }

    /// E2. A paused series appearing in its source's LATEST is a push signal: that source
    /// just published for it. Waking it is what replaces polling — and what makes reopen
    /// detection ≤1 day instead of ≤14, at zero additional upstream fetches.
    ///
    /// The three negative cases are the point. A series on its ordinary cadence is not
    /// parked and must not be dragged forward; a series scanned inside the cooldown must
    /// not be re-triggered just for sitting in a top-30 window it can occupy for days
    /// (measured 3.3–7.5); and neither may a series we do not track at all be invented.
    #[tokio::test]
    async fn latest_wakes_a_parked_series_but_not_an_active_or_recently_scanned_one() {
        let pool = migrated_pool().await;
        let now = Utc::now();
        let parked_after = (now + chrono::Duration::hours(TRIGGER_PARKED_AFTER_HOURS)).to_rfc3339();
        let scanned_before = (now - chrono::Duration::hours(TRIGGER_COOLDOWN_HOURS)).to_rfc3339();
        let put = |id: &'static str, next: String, last: Option<String>| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO series_scan_state (series_id, next_scan_at, last_scanned_at, updated_at) \
                     VALUES (?, ?, ?, '2026-01-01T00:00:00Z')",
                )
                .bind(id)
                .bind(next)
                .bind(last)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        let long_ago = (now - chrono::Duration::days(30)).to_rfc3339();
        // Parked (a paused series) and last scanned long ago → wake it.
        put(
            "parked",
            (now + chrono::Duration::days(13)).to_rfc3339(),
            Some(long_ago.clone()),
        )
        .await;
        // Parked but scanned yesterday → inside the cooldown, leave it alone.
        put(
            "recent",
            (now + chrono::Duration::days(13)).to_rfc3339(),
            Some((now - chrono::Duration::days(1)).to_rfc3339()),
        )
        .await;
        // Active on a normal 12h cadence → not parked, must not be pulled forward.
        put(
            "active",
            (now + chrono::Duration::hours(12)).to_rfc3339(),
            Some(long_ago.clone()),
        )
        .await;

        let keys: Vec<String> = ["parked", "recent", "active", "untracked"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let due = crate::catalog::paused_series_due_for_trigger(
            &pool,
            &keys,
            &parked_after,
            &scanned_before,
        )
        .await
        .unwrap();
        assert_eq!(
            due,
            vec!["parked".to_string()],
            "only the parked, cold series"
        );

        assert_eq!(trigger_due_now(&pool, &due).await, 1);
        assert_eq!(
            next_scan_of(&pool, "parked").await.as_deref(),
            Some(DUE_NOW_SENTINEL),
            "woken series are ENQUEUED at the due-now sentinel, never scanned inline"
        );
        // The scheduler picks it up on its very next tick, which is the whole mechanism.
        assert!(due_series_ids(&pool, &now.to_rfc3339(), 10)
            .await
            .contains(&"parked".to_string()));
        // …and nothing else moved.
        assert!(next_scan_of(&pool, "active").await.as_deref() != Some(DUE_NOW_SENTINEL));
        assert!(next_scan_of(&pool, "recent").await.as_deref() != Some(DUE_NOW_SENTINEL));
    }

    #[tokio::test]
    async fn park_clears_awaiting_and_failures() {
        // audit #5(low)/#9: parking a paused series must clear a stale awaiting streak and
        // reset the failure counter (the fetch that led to the park succeeded).
        let pool = migrated_pool().await;
        let now = at("2024-01-01T00:00:00Z");
        sqlx::query(
            "INSERT INTO series_scan_state \
               (series_id, next_scan_at, awaiting_since, consecutive_failures, updated_at) \
             VALUES ('p', NULL, '2023-12-01T00:00:00Z', 3, '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        park_paused(&pool, "p", now).await.unwrap();

        let row = scan_state(&pool, "p").await.unwrap();
        assert!(row.awaiting_since.is_none(), "park clears awaiting_since");
        assert_eq!(failures_of(&pool, "p").await, 0, "park resets failures");
    }

    #[tokio::test]
    async fn backfill_covers_only_untracked_suwayomi_series() {
        let pool = migrated_pool().await;
        // A work for the source_series FK (foreign keys are enforced).
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('w', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let ins_ss = |key: &str, ty: &str| {
            let pool = pool.clone();
            let key = key.to_string();
            let ty = ty.to_string();
            async move {
                sqlx::query(
                    "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
                     VALUES (?, 'w', ?, ?, '2024-01-01T00:00:00Z')",
                )
                .bind(format!("ss_{ty}_{key}"))
                .bind(&ty)
                .bind(&key)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        ins_ss("100", "suwayomi").await; // untracked suwayomi -> should be backfilled
        ins_ss("200", "suwayomi").await; // already tracked (below) -> left alone
        ins_ss("300", "mangadex").await; // not suwayomi -> ignored
        park_paused(&pool, "200", at("2024-01-01T00:00:00Z"))
            .await
            .unwrap();
        let parked_200 = next_scan_of(&pool, "200").await;

        let added = crate::catalog::backfill_pending_scan_states(&pool)
            .await
            .unwrap();
        assert_eq!(added, 1, "only the untracked suwayomi series is backfilled");
        assert_eq!(
            next_scan_of(&pool, "100").await.as_deref(),
            Some(DUE_NOW_SENTINEL),
            "backfilled row is due now (sentinel, never NULL)"
        );
        assert_eq!(
            next_scan_of(&pool, "200").await,
            parked_200,
            "already-tracked series is untouched"
        );
        assert!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM series_scan_state WHERE series_id = '300'"
            )
            .fetch_one(&pool)
            .await
            .unwrap()
                == 0,
            "non-suwayomi series is never tracked"
        );
    }

    #[tokio::test]
    async fn enrol_paths_never_leave_null_next_scan_at() {
        // COMPLETENESS INVARIANT (audit #6): the due-query is a bounded `next_scan_at <= ?`,
        // so a NULL row can never match and would silently never scan. Assert the enrol
        // paths write the sentinel, never NULL, and that such rows are due now.
        let pool = migrated_pool().await;
        let now = at("2024-01-01T00:00:00Z");
        ensure_pending(&pool, "1").await.unwrap();
        ensure_pending(&pool, "2").await.unwrap();
        let nulls: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM series_scan_state WHERE next_scan_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(nulls, 0, "no enrolled row may have a NULL next_scan_at");
        let due = due_series_ids(&pool, &now.to_rfc3339(), 10).await;
        assert!(
            due.contains(&"1".to_string()) && due.contains(&"2".to_string()),
            "sentinel rows are due now"
        );
    }

    // Locks the due-query plan: it must use the next_scan_at index, take its ordering from
    // the index (no temp-b-tree sort), and be a bounded SEARCH (range seek) — NOT a full
    // SCAN. The bounded SEARCH is what makes the query O(due): with "due now" stored as the
    // far-past sentinel (not NULL), the `next_scan_at <= ?` range early-terminates at the
    // first future row, so future-dated rows are never visited (audit #6).
    #[tokio::test]
    async fn due_query_is_index_backed_bounded_and_unsorted() {
        use sqlx::Row as _;
        let pool = migrated_pool().await;
        let plan: Vec<String> = sqlx::query(
            "EXPLAIN QUERY PLAN SELECT series_id FROM series_scan_state \
             WHERE next_scan_at <= ? ORDER BY next_scan_at ASC LIMIT ?",
        )
        .bind("2024-01-01T00:00:00Z")
        .bind(DUE_BATCH_LIMIT)
        .fetch_all(&pool)
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<String, _>("detail"))
        .collect();
        let joined = plan.join(" | ");
        assert!(
            joined.contains("idx_scan_state_next_scan"),
            "due-query must use the next_scan_at index, got: {joined}"
        );
        assert!(
            !joined.to_uppercase().contains("TEMP B-TREE"),
            "due-query ordering must come from the index, not a sort, got: {joined}"
        );
        // A bounded range seek reports as SEARCH … (next_scan_at<?); a full walk reports as
        // SCAN. The `<= ?` sentinel design must yield the former (early-terminating).
        let upper = joined.to_uppercase();
        assert!(
            upper.contains("SEARCH") && !upper.contains("SCAN SERIES_SCAN_STATE"),
            "due-query must be a bounded SEARCH (range seek), not a full SCAN, got: {joined}"
        );
    }

    /// A `SuwayomiManga` carrying nothing but an id + upstream status — all
    /// `effective_status` reads.
    fn manga_with_status(id: i64, status: &str) -> SuwayomiManga {
        SuwayomiManga {
            id,
            title: format!("S{id}"),
            url: None,
            thumbnail_url: None,
            author: None,
            artist: None,
            description: None,
            genre: Vec::new(),
            status: status.to_string(),
            in_library: true,
            in_library_at: None,
            last_fetched_at: None,
            latest_chapter_at: None,
            source_id: "1".into(),
            source: None,
            chapters: None,
        }
    }

    /// Migration 0057's statement 1 hand-writes the pause rule in SQL, duplicating
    /// `is_paused` + `effective_status`. A mismatch is silent and expensive in BOTH
    /// directions: classifying a paused series as active drags it onto the 12h sweep
    /// (7,397 live paused rows — roughly a doubling of the scan budget), and
    /// classifying an active one as paused strands it at whatever legacy multi-year
    /// `next_scan_at` it already carries.
    ///
    /// This runs the REAL migration file against a seeded matrix and compares, row by
    /// row, against the Rust predicate — so the two cannot drift without a red test.
    #[tokio::test]
    async fn migration_0057_pause_predicate_matches_rust() {
        let pool = migrated_pool().await;
        let now = Utc::now();
        // Far enough out that statement 1 matches it (> now + 14h), but inside the
        // absurd horizon so statement 2 leaves it alone — the row therefore moves if
        // and only if statement 1 considers it NON-paused.
        let parked = (now + chrono::Duration::hours(300)).to_rfc3339();

        // Every upstream status `status_from` distinguishes, plus a word it doesn't.
        let statuses = [
            "ONGOING",
            "COMPLETED",
            "PUBLISHING_FINISHED",
            "LICENSED",
            "CANCELLED",
            "ON_HIATUS",
            "UNKNOWN",
            "SOMETHING_NEW",
        ];
        // NULL = no override; 0 = an explicit "keep scanning" (what
        // `setSeriesPaused(false)` writes); 1 = forced pause.
        let pauses = [None, Some(0i64), Some(1i64)];
        // NULL = none; the five `komika_status` words; and one it rejects.
        let overrides = [
            None,
            Some("ONGOING"),
            Some("COMPLETED"),
            Some("HIATUS"),
            Some("CANCELLED"),
            Some("UNKNOWN"),
            Some("NOT_A_STATUS"),
        ];

        let mut expected: Vec<(String, bool)> = Vec::new(); // (id, rust says paused)
        let mut id = 1i64;
        for status in statuses {
            for paused_override in pauses {
                for status_override in overrides {
                    let sid = id.to_string();
                    sqlx::query(
                        "INSERT INTO suwayomi_series (id, title, status, source_id, updated_at) \
                         VALUES (?, ?, ?, '1', ?)",
                    )
                    .bind(id)
                    .bind(format!("S{id}"))
                    .bind(status)
                    .bind(now.to_rfc3339())
                    .execute(&pool)
                    .await
                    .unwrap();
                    if paused_override.is_some() || status_override.is_some() {
                        sqlx::query(
                            "INSERT INTO series_admin \
                               (series_id, paused_override, status_override, updated_at) \
                             VALUES (?, ?, ?, ?)",
                        )
                        .bind(&sid)
                        .bind(paused_override)
                        .bind(status_override)
                        .bind(now.to_rfc3339())
                        .execute(&pool)
                        .await
                        .unwrap();
                    }
                    put_state(&pool, &sid, Some(&parked)).await;

                    let admin = ScanAdmin {
                        paused_override,
                        status_override: status_override.map(|s| s.to_string()),
                        ..Default::default()
                    };
                    let m = manga_with_status(id, status);
                    expected.push((sid, is_paused(effective_status(&m, &admin), &admin)));
                    id += 1;
                }
            }
        }

        // Run the migration itself, not a copy of it.
        sqlx::raw_sql(include_str!(
            "../migrations/0057_clamp_legacy_next_scan_at.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let cutoff = (now + chrono::Duration::hours(14)).to_rfc3339();
        for (sid, rust_paused) in &expected {
            let stored = next_scan_of(&pool, sid).await.unwrap();
            let moved = stored < cutoff;
            assert_eq!(
                moved, !rust_paused,
                "series {sid}: migration moved={moved} but scanner::is_paused={rust_paused} \
                 (stored {stored})"
            );
        }
        // Sanity: the matrix actually exercises both outcomes.
        assert!(expected.iter().any(|(_, p)| *p) && expected.iter().any(|(_, p)| !*p));
    }

    /// Statement 1 must be idempotent — re-running it can neither move a row it already
    /// placed inside the 12h window nor pick up a paused row it deliberately skipped.
    #[tokio::test]
    async fn migration_0057_is_idempotent() {
        let pool = migrated_pool().await;
        let now = Utc::now();
        let sql = include_str!("../migrations/0057_clamp_legacy_next_scan_at.sql");
        // One active row parked in 2033, one paused row on a legal 300h park.
        for (id, status) in [(1i64, "ONGOING"), (2, "COMPLETED")] {
            sqlx::query(
                "INSERT INTO suwayomi_series (id, title, status, source_id, updated_at) \
                 VALUES (?, ?, ?, '1', ?)",
            )
            .bind(id)
            .bind(format!("S{id}"))
            .bind(status)
            .bind(now.to_rfc3339())
            .execute(&pool)
            .await
            .unwrap();
        }
        put_state(&pool, "1", Some("2033-03-14T03:12:41+00:00")).await;
        let paused_at = (now + chrono::Duration::hours(300)).to_rfc3339();
        put_state(&pool, "2", Some(&paused_at)).await;
        sqlx::query("UPDATE series_scan_state SET avg_interval_hours = 58309.0")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        let after_first = next_scan_of(&pool, "1").await.unwrap();
        assert!(
            after_first < (now + chrono::Duration::hours(13)).to_rfc3339(),
            "the active row lands inside the 12h window, got {after_first}"
        );
        assert_eq!(
            next_scan_of(&pool, "2").await.as_deref(),
            Some(paused_at.as_str()),
            "a paused row on a legal park is left alone"
        );
        let avg: f64 = sqlx::query_scalar("SELECT MAX(avg_interval_hours) FROM series_scan_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            (avg - MAX_INTERVAL_HOURS).abs() < 0.001,
            "absurd stored averages are retired to the clamp, got {avg}"
        );

        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        assert_eq!(
            next_scan_of(&pool, "1").await.as_deref(),
            Some(after_first.as_str()),
            "a second run must not re-roll a row it already placed"
        );
        assert_eq!(
            next_scan_of(&pool, "2").await.as_deref(),
            Some(paused_at.as_str()),
            "a second run must not pick up the paused row either"
        );
    }

    /// Nothing this module writes may land outside `(now, now + ABSURD_HORIZON_HOURS]`.
    ///
    /// The upper half is what makes `reclaim_absurd_schedules` safe to run every tick:
    /// if a legitimate writer could exceed the horizon, the reclaim would fight it and
    /// yank healthy series back into the due-set forever. The lower half is the
    /// non-convergence bug in another disguise — a `next_scan_at` at or before `now` is
    /// instantly re-due. Swept with jitter ON, across pathological cadences and admin
    /// overrides (including NaN/inf, which reach `clamp`).
    #[tokio::test]
    async fn scheduled_next_scan_never_escapes_the_reclaim_horizon() {
        let pool = migrated_pool().await;
        JITTER_ENABLED.with(|c| c.set(true));
        let now = at("2026-01-01T00:00:00Z");
        let floor = now.to_rfc3339();
        let horizon = (now + chrono::Duration::hours(ABSURD_HORIZON_HOURS)).to_rfc3339();

        let overrides = [
            None,
            Some(0.0),
            Some(-5.0),
            Some(0.0001),
            Some(1.0),
            Some(MAX_INTERVAL_HOURS),
            Some(58_309.0),
            Some(f64::INFINITY),
            Some(f64::NAN),
        ];
        let polls = [None, Some(-1i64), Some(1), Some(30), Some(100_000)];
        // Cadences: none, a same-day burst, weekly, the absurd sparse-history case, and
        // a list whose uploads all share one timestamp (zero gaps -> no cadence at all).
        let lists: Vec<Vec<SuwayomiChapter>> = vec![
            Vec::new(),
            chaps(3),
            dated_chaps(8, 1, now - chrono::Duration::hours(1)),
            dated_chaps(8, 168, now - chrono::Duration::hours(400)),
            dated_chaps(8, 20_000, now - chrono::Duration::hours(100_000)),
            vec![
                chap_n(1, 1.0, Some("1700000000000")),
                chap_n(2, 2.0, Some("1700000000000")),
            ],
        ];

        let mut id = 0;
        for over in overrides {
            for poll in polls {
                for list in &lists {
                    id += 1;
                    let sid = id.to_string();
                    let admin = ScanAdmin {
                        override_interval_hours: over,
                        poll_every_minutes: poll,
                        ..Default::default()
                    };
                    // Twice: the second call has a prior snapshot, so it can reach the
                    // awaiting/accelerated branch the first (baseline) one cannot.
                    for _ in 0..2 {
                        record_scan(&pool, &sid, "S", &admin, list, now)
                            .await
                            .unwrap();
                        let next = next_scan_of(&pool, &sid).await.unwrap();
                        assert!(
                            next > floor,
                            "over={over:?} poll={poll:?}: scheduled {next} at or before now"
                        );
                        assert!(
                            next <= horizon,
                            "over={over:?} poll={poll:?}: scheduled {next} past the \
                             {ABSURD_HORIZON_HOURS}h reclaim horizon"
                        );
                    }
                }
            }
        }
        JITTER_ENABLED.with(|c| c.set(false));
    }

    /// The awaiting state machine must TERMINATE and stay bounded.
    ///
    /// `awaiting_since` is both the accelerated-window start and the re-arm cool-down
    /// marker, and the accelerated cadence (30m) is 24x the steady one — so a series
    /// that can never leave the awaiting state is a 24x load multiplier that no
    /// constant in this file bounds. Drive a series that STOPS publishing through its
    /// own schedule for 120 simulated days (following whatever `next_scan_at` it
    /// writes, exactly as the scheduler would) and assert it converges: the streak
    /// clears, the final cadence is the steady one, and the total fetch count stays
    /// near the analytic bound rather than the once-every-30-minutes runaway.
    #[tokio::test]
    async fn a_series_that_stops_publishing_leaves_the_accelerated_poll() {
        let pool = migrated_pool().await;
        let admin = ScanAdmin {
            poll_every_minutes: Some(30),
            ..Default::default()
        };
        // 8 chapters on a weekly rhythm; the newest lands at t0 and none ever follow.
        let t0 = at("2026-01-01T00:00:00Z");
        let chapters = dated_chaps(8, 168, t0);
        record_scan(&pool, "1", "S", &admin, &chapters, t0)
            .await
            .unwrap();

        let deadline = t0 + chrono::Duration::days(120);
        let mut clock = t0;
        let mut scans = 0usize;
        let mut accelerated = 0usize;
        loop {
            let next = parse_iso(next_scan_of(&pool, "1").await.as_deref()).unwrap();
            assert!(next > clock, "schedule must advance; got {next} at {clock}");
            if next > deadline {
                break;
            }
            clock = next;
            scans += 1;
            assert!(scans < 20_000, "scheduler did not converge within 120 days");
            record_scan(&pool, "1", "S", &admin, &chapters, clock)
                .await
                .unwrap();
            let after = parse_iso(next_scan_of(&pool, "1").await.as_deref()).unwrap();
            if (after - clock).num_minutes() <= 60 {
                accelerated += 1;
            }
        }

        assert!(
            persisted(&pool, "1").await.awaiting_since.is_none(),
            "a series past the lateness band must not still be marked awaiting"
        );
        // 120d at the 12h ceiling = 240 scans. The lateness band for a 168h cadence is
        // (210h, 504h] past the newest upload = 294h wide, which is longer than the
        // 240h re-arm, so exactly two 48h accelerated windows are reachable: 2 x 96
        // extra fetches. Anything beyond that means a window failed to close.
        assert!(
            accelerated <= 2 * 96 + 4,
            "accelerated polling ran away: {accelerated} fast re-polls of {scans} scans"
        );
        assert!(
            scans < 500,
            "120 days of a dead series cost {scans} fetches; the steady ceiling plus two \
             accelerated windows is ~430"
        );
        assert!(
            accelerated > 0,
            "the fixture must actually exercise the accelerated poll"
        );
    }

    // ── the merged Updates feed's incremental (scanner-half) refresh ──────────────────

    /// Everything `upsert_feed_series_update` joins: a work, its Suwayomi
    /// `source_series` mapping, and the cached `suwayomi_series` row carrying the
    /// release clock. `latest_chapter_at` is 13-digit epoch-millis TEXT, exactly as
    /// `series_cache::derive_latest_chapter_at` stores it.
    async fn seed_feed_fixture(pool: &SqlitePool, latest_chapter_at: Option<&str>) {
        sqlx::query(
            "INSERT INTO work (id, primary_title, original_language, created_at, updated_at) \
             VALUES ('w1', 'Canonical Title', 'ko', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('ss1', 'w1', 'suwayomi', '7', '2026-01-01T00:00:00Z')",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO suwayomi_series \
                 (id, title, thumbnail_url, status, in_library, source_id, chapter_count, \
                  latest_chapter_at, updated_at) \
             VALUES (7, 'Suwayomi Title', '/thumb/7', 'ONGOING', 1, '1', 13, ?, \
                     '2026-01-01T00:00:00Z')",
        )
        .bind(latest_chapter_at)
        .execute(pool)
        .await
        .unwrap();
    }

    /// F4. The card must be labelled with the newest chapter's NUMBER. It used to be
    /// labelled `NULL`, and the reader fell back to `Ch. {chapterCount}` — so a series with
    /// 5 mirrored chapters whose newest is 10.5 announced "Ch. 5". Those are different
    /// quantities, and half-chapters/mid-series pickups make them disagree constantly.
    ///
    /// Two traps this pins, both measured in production:
    ///   * NEWEST-RELEASED, not `MAX(number)` — they disagree on 1,174 of 11,797 series,
    ///     and the update event is about the chapter that just landed;
    ///   * a sanity clamp, or the two series carrying `Ch.99999999` (a TEST upload) and
    ///     `Ch.20240120` (a date) label their cards `Ch. 100000000`.
    #[tokio::test]
    async fn the_feed_card_is_labelled_with_a_chapter_number_never_a_count() {
        let pool = migrated_pool().await;
        seed_feed_fixture(&pool, Some("1785071625000")).await;
        // Five chapters. The newest RELEASE is 10.5 (a half chapter), the highest NUMBER
        // is 12, and there is a garbage 99999999 upload sitting on top of both.
        for (id, number, name, upload) in [
            (1_i64, 1.0_f64, "Ch 1", "1785000000000_i64"),
            (2, 12.0, "Ch 12", "1785010000000"),
            (3, 10.5, "Ch 10.5", "1785071625000"),
            (4, 99_999_999.0, "Ch.99999999 - TEST", "1785071625001"),
            (5, 2.0, "Ch 2", "1785005000000"),
        ] {
            sqlx::query(
                "INSERT INTO suwayomi_chapter \
                   (id, manga_id, name, chapter_number, page_count, upload_date, updated_at) \
                 VALUES (?, 7, ?, ?, 0, ?, '2026-01-01T00:00:00Z')",
            )
            .bind(id)
            .bind(name)
            .bind(number)
            .bind(upload.trim_end_matches("_i64"))
            .execute(&pool)
            .await
            .unwrap();
        }

        // The first scan is a BASELINE (it stamps no `last_new_chapter_at`, and the feed's
        // scanner half requires one), so the detection has to be the second scan — same
        // shape as `incremental_write_converges_with_the_periodic_rebuild`.
        let admin = ScanAdmin::default();
        record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(12),
            at("2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        let found = record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(13),
            at("2026-01-08T00:00:00Z"),
        )
        .await
        .unwrap();
        assert!(found, "the second scan is a real detection");
        touch_feed_series_update(&pool, "7", found).await;

        let label: Option<String> =
            sqlx::query_scalar("SELECT latest_chapter FROM feed_series_updates WHERE work_id='w1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            label.as_deref(),
            Some("10.5"),
            "the newest RELEASED chapter's number — not the count (13), not MAX (12), \
             and not the 99999999 sentinel"
        );

        // And the wholesale rebuild agrees, which is what stops the label flipping
        // depending on which writer touched the row last.
        crate::catalog::refresh_feed_series_updates(&pool)
            .await
            .unwrap();
        let rebuilt: Option<String> =
            sqlx::query_scalar("SELECT latest_chapter FROM feed_series_updates WHERE work_id='w1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rebuilt.as_deref(), Some("10.5"), "rebuild agrees");
    }

    /// The feed row for the fixture's work, as (reader_id, released_at, typeof, title,
    /// comic_type, chapter_count, detected_at).
    #[allow(clippy::type_complexity)]
    async fn feed_row(
        pool: &SqlitePool,
    ) -> Option<(
        String,
        i64,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
    )> {
        sqlx::query_as(
            "SELECT reader_id, released_at, typeof(released_at), title, comic_type, \
                    chapter_count, detected_at \
             FROM feed_series_updates WHERE work_id = 'w1'",
        )
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    /// Every column of the fixture's feed row plus the sort key's STORAGE CLASS, as one
    /// comparable string. `~` stands in for NULL so a NULL never swallows the whole
    /// concatenation.
    ///
    /// EVERY column, deliberately — that is what makes the convergence proof below a proof
    /// rather than a spot check. Migration 0068's three additions (`status`,
    /// `content_rating`, `en_chapter_count`) are in here for the same reason the original
    /// twelve are: two of them are Browse FILTER keys and one is a SORT key, so a row whose
    /// value depends on which writer touched it last is a row that appears or disappears
    /// from Browse depending on scan timing.
    async fn feed_digest(pool: &SqlitePool) -> Option<String> {
        sqlx::query_scalar(
            "SELECT work_id || '|' || reader_id || '|' || title \
                 || '|' || COALESCE(cover_url, '~') || '|' || COALESCE(suwayomi_thumbnail, '~') \
                 || '|' || COALESCE(comic_type, '~') || '|' || COALESCE(latest_chapter, '~') \
                 || '|' || COALESCE(latest_chapter_title, '~') \
                 || '|' || COALESCE(chapter_count, '~') \
                 || '|' || released_at || '|' || typeof(released_at) \
                 || '|' || COALESCE(detected_at, '~') || '|' || is_nsfw \
                 || '|' || COALESCE(status, '~') || '|' || content_rating \
                 || '|' || en_chapter_count \
             FROM feed_series_updates WHERE work_id = 'w1'",
        )
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    /// The same digest over Browse's own table (migration 0069), which
    /// `mirror_feed_row_into_browse_catalogue` writes incrementally and
    /// `catalog::refresh_browse_catalogue` rebuilds. `comic_type` is included because a NULL
    /// there is invisible to Browse's format tabs, and `typeof(released_at)` because a TEXT
    /// sort key would sort the whole undated tail above every dated work.
    ///
    /// `latest_chapter` (migration 0095) is included for the same reason `en_chapter_count`
    /// is: Browse prints the two side by side ("12 ch · Ch. 151"), so a column the
    /// incremental mirror forgets to carry leaves the NUMBER stale while the COUNT advances
    /// — the two halves of one card disagreeing, which is F4 wearing a different hat. It was
    /// in fact missing from the mirror's `MUT` list when 0095 landed (§8h); this digest is
    /// what stops that recurring.
    async fn browse_digest(pool: &SqlitePool) -> Option<String> {
        sqlx::query_scalar(
            "SELECT work_id || '|' || reader_id || '|' || title \
                 || '|' || COALESCE(cover_url, '~') || '|' || COALESCE(suwayomi_thumbnail, '~') \
                 || '|' || COALESCE(comic_type, '~') || '|' || COALESCE(status, '~') \
                 || '|' || content_rating || '|' || is_nsfw || '|' || en_chapter_count \
                 || '|' || COALESCE(latest_chapter, '~') \
                 || '|' || COALESCE(released_at, -1) || '|' || typeof(released_at) \
                 || '|' || created_at \
             FROM browse_catalogue WHERE work_id = 'w1'",
        )
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    /// THE convergence proof for `upsert_feed_series_update`: drive the incremental path
    /// twice, then hand the table to the REAL periodic rebuild and demand a byte-identical
    /// row. A writer-dependent row — one whose contents depend on which of the two touched
    /// it last — is the whole risk of maintaining a materialized table from two places.
    ///
    /// The second detection deliberately moves the SOURCE's `chapter_count` (13 → 14)
    /// between writes, because that is the single interleaving the rebuild's literal
    /// `COALESCE(feed_series_updates.chapter_count, excluded.chapter_count)` gets wrong:
    /// copied verbatim it would pin the count at 13 forever, and a scanner-only card
    /// renders `Ch. {latest_chapter ?? chapter_count}` — announcing a new chapter while
    /// still printing the old number. Stating that one clause in the converged direction
    /// is what this asserts.
    ///
    /// This test spans a file another owner maintains: if `refresh_feed_series_updates`'
    /// field mapping is edited, this fails rather than letting the two writers drift.
    #[tokio::test]
    async fn incremental_write_converges_with_the_periodic_rebuild() {
        let pool = migrated_pool().await;
        seed_feed_fixture(&pool, Some("1785071625000")).await;
        let admin = ScanAdmin::default();

        record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(12),
            at("2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        let first = record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(13),
            at("2026-01-08T00:00:00Z"),
        )
        .await
        .unwrap();
        touch_feed_series_update(&pool, "7", first).await;

        // The source moves on — a newer release AND a higher count — exactly as
        // `series_cache::put_chapters` would have written them before the next scan.
        sqlx::query(
            "UPDATE suwayomi_series SET chapter_count = 14, latest_chapter_at = '1785671625000' \
             WHERE id = 7",
        )
        .execute(&pool)
        .await
        .unwrap();
        let second = record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(14),
            at("2026-01-15T00:00:00Z"),
        )
        .await
        .unwrap();
        assert!(second);
        touch_feed_series_update(&pool, "7", second).await;

        let incremental = feed_digest(&pool)
            .await
            .expect("two detections must publish");
        let incremental_browse = browse_digest(&pool)
            .await
            .expect("a detection must reach Browse's table too — it no longer reads the feed");
        let (_, released_at, typ, .., chapter_count, _) = feed_row(&pool).await.unwrap();
        assert_eq!(
            chapter_count,
            Some(14),
            "the count must follow the source, not the first incremental write"
        );
        // The ON CONFLICT branch's `MAX(existing, excluded)` must also land an INTEGER —
        // `detected_chapter_upserts_the_updates_feed_row` only covers the INSERT branch,
        // and a TEXT sort key sorts the whole mirror half above the whole scanner half.
        assert_eq!(
            typ, "integer",
            "the conflict branch must keep released_at INTEGER"
        );
        assert_eq!(released_at, 1_785_671_625_000, "and must move forward");

        // Hand the table to the periodic rebuild, which owns it from scratch.
        crate::catalog::refresh_feed_series_updates(&pool)
            .await
            .unwrap();
        let rebuilt = feed_digest(&pool)
            .await
            .expect("the rebuild must produce the row too");
        assert_eq!(
            incremental, rebuilt,
            "the incremental writer and the periodic rebuild must not disagree about any column"
        );

        // And the same for `browse_catalogue`. Two writers, one table, again — and here the
        // stakes are the DEFAULT Browse sort: a row the incremental path left behind would put
        // a series with a brand-new chapter in the undated tail of "Recently updated".
        let rebuilt_browse = browse_digest(&pool)
            .await
            .expect("the rebuild must produce the browse row too");
        assert_eq!(
            incremental_browse, rebuilt_browse,
            "the incremental browse-row copy and the periodic rebuild must not disagree"
        );

        // --- and again with the release ledger ACTIVE (Phase C2) -----------------------
        //
        // Everything above ran with an empty `release_event`, so `ledger::is_complete` was
        // false and the projection pass was inert in BOTH writers — which proves they agree
        // when it is switched off, and nothing about when it is on. That is the state this
        // deploy ships in, and it is exactly the state that stops being true the moment the
        // background seed finishes on production.
        //
        // So: seed the ledger from the spine the fixture's chapters now populate, drive the
        // incremental path once more, and demand the same byte-identical agreement with the
        // rebuild. Both paths run the identical `project_feed_from_ledger` statement, so the
        // convergence argument is structural — this is what stops it being merely an
        // argument.
        // `chaps()` carries no upload dates, and an undated chapter is deliberately not
        // seedable — the ledger refuses to invent a release time for one. So populate the
        // cache through the REAL `put_chapters` path with dated chapters, which is also
        // what writes them into the canonical spine (Phase B1).
        crate::series_cache::put_chapters(
            &pool,
            7,
            &dated_chaps(14, 24, at("2026-01-15T00:00:00Z")),
        )
        .await
        .unwrap();
        crate::catalog::spine::drain_suwayomi_series(&pool)
            .await
            .unwrap();
        while crate::catalog::spine::drain_chapter_keys(&pool)
            .await
            .unwrap()
            > 0
        {}
        while crate::catalog::ledger::seed_batch(&pool).await.unwrap() > 0 {}
        assert!(
            crate::catalog::ledger::is_complete(&pool).await.unwrap(),
            "the fixture must reach a complete ledger, or this asserts nothing"
        );
        // A release time in the ledger may never be in the future — the deployment guard.
        crate::catalog::ledger::assert_no_future_events(&pool)
            .await
            .unwrap();

        touch_feed_series_update(&pool, "7", true).await;
        let incremental_led = feed_digest(&pool)
            .await
            .expect("the incremental path must still publish with the ledger active");
        crate::catalog::refresh_feed_series_updates(&pool)
            .await
            .unwrap();
        let rebuilt_led = feed_digest(&pool)
            .await
            .expect("the rebuild must still produce the row with the ledger active");
        assert_eq!(
            incremental_led, rebuilt_led,
            "with the ledger projecting the release clock, the two writers must STILL agree"
        );
    }

    /// A detection publishes the series into the merged Updates feed immediately,
    /// without waiting for `catalog::refresh_feed_series_updates`.
    #[tokio::test]
    async fn detected_chapter_upserts_the_updates_feed_row() {
        let pool = migrated_pool().await;
        seed_feed_fixture(&pool, Some("1785071625000")).await;
        let admin = ScanAdmin::default();

        // Baseline first: a first observation is not a detection (SC3).
        let baseline = record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(12),
            at("2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        touch_feed_series_update(&pool, "7", baseline).await;
        assert!(feed_row(&pool).await.is_none(), "baseline is not an update");

        let then = at("2026-01-08T00:00:00Z");
        let new_found = record_scan(&pool, "7", "S", &admin, &chaps(13), then)
            .await
            .unwrap();
        assert!(new_found);
        touch_feed_series_update(&pool, "7", new_found).await;

        let (reader_id, released_at, typ, title, comic_type, chapter_count, detected_at) =
            feed_row(&pool).await.expect("detection must publish a row");
        // THE load-bearing assertion: the sort key is stored as INTEGER epoch-millis, not
        // as text. A TEXT key sorts every ISO '2…' mirror row above every millis '1…'
        // scanner row under BINARY collation — see migration 0064.
        assert_eq!(typ, "integer", "released_at must be an INTEGER, not TEXT");
        assert_eq!(released_at, 1_785_071_625_000);
        // A Suwayomi-only work has no MangaDex anchor, so the card must navigate by the
        // numeric Suwayomi id, not by `w_…`.
        assert_eq!(reader_id, "7");
        assert_eq!(title, "Canonical Title");
        assert_eq!(chapter_count, Some(13));
        assert_eq!(detected_at.as_deref(), Some(then.to_rfc3339()).as_deref());
        // A brand-new row must not land type-less: `comic_type IS NULL` is invisible to
        // the reader's format tabs. 'ko' -> MANHWA, via the real `resolve_comic_type`.
        assert_eq!(comic_type.as_deref(), Some("MANHWA"));
    }

    /// The incremental write max-merges: it may move a row forward in time, never
    /// backwards, and never clobbers the mirror half's identity/display fields.
    #[tokio::test]
    async fn feed_upsert_never_moves_a_row_backwards() {
        let pool = migrated_pool().await;
        // The Suwayomi source is OLDER than what the mirror half already published.
        seed_feed_fixture(&pool, Some("1700000000000")).await;
        sqlx::query(
            "INSERT INTO feed_series_updates \
                 (work_id, reader_id, title, comic_type, latest_chapter, released_at, is_nsfw) \
             VALUES ('w1', 'w1', 'Mirror Title', 'MANGA', '99', 1785071625000, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let admin = ScanAdmin::default();

        record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(12),
            at("2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        let new_found = record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(13),
            at("2026-01-08T00:00:00Z"),
        )
        .await
        .unwrap();
        assert!(new_found);
        touch_feed_series_update(&pool, "7", new_found).await;

        let (reader_id, released_at, _, title, comic_type, ..) = feed_row(&pool).await.unwrap();
        assert_eq!(
            released_at, 1_785_071_625_000,
            "an older scanner clock must not pull the row backwards"
        );
        // `reader_id` precedence: an existing canonical `w_…` id survives, so a
        // mangadex-anchored work keeps navigating to its canonical page.
        assert_eq!(reader_id, "w1");
        assert_eq!(
            title, "Mirror Title",
            "display fields stay on the mirror half"
        );
        assert_eq!(
            comic_type.as_deref(),
            Some("MANGA"),
            "type is never rewritten"
        );
    }

    /// A mangadex-anchored work whose MIRROR half never fires — a licensing takedown leaves
    /// the spine with no dated chapter, so only the Suwayomi scanner half publishes its feed
    /// row — must still navigate to its canonical `w_…` page, not the numeric Suwayomi one.
    /// This is the regression the 293-row `reader_id` heal (migration 0070) targets, pinned
    /// across all three writers: the incremental upsert, the periodic feed rebuild, and Browse.
    #[tokio::test]
    async fn feed_reader_id_is_canonical_for_a_takedown_anchored_work() {
        let pool = migrated_pool().await;
        seed_feed_fixture(&pool, Some("1785071625000")).await;
        // Give the work a MangaDex ANCHOR but NO mirror feed row (the takedown case): the
        // spine has no dated chapter, so the rebuild's mirror half skips it and only the
        // Suwayomi scanner half below publishes a row.
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('ssmd', 'w1', 'mangadex', 'uuid-abc', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let admin = ScanAdmin::default();

        record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(12),
            at("2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        let found = record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(13),
            at("2026-01-08T00:00:00Z"),
        )
        .await
        .unwrap();
        assert!(found);
        touch_feed_series_update(&pool, "7", found).await;

        // (1) Incremental writer (`upsert_feed_series_update`): the anchor test wins on INSERT.
        let (reader_id, ..) = feed_row(&pool).await.expect("detection must publish a row");
        assert_eq!(
            reader_id, "w1",
            "an anchored work must navigate to its canonical page even when only the Suwayomi \
             half publishes its feed row"
        );

        // (2) Periodic rebuild agrees (both derivations carry the same anchor test).
        crate::catalog::refresh_feed_series_updates(&pool)
            .await
            .unwrap();
        let (rebuilt, ..) = feed_row(&pool)
            .await
            .expect("the rebuild must publish a row");
        assert_eq!(
            rebuilt, "w1",
            "the periodic rebuild must derive the canonical id too"
        );

        // (3) Browse's table (rebuilt by the call above) carries the canonical id — the card's
        // link — so a Browse card and the same work's Updates card agree.
        let browse_reader_id: String =
            sqlx::query_scalar("SELECT reader_id FROM browse_catalogue WHERE work_id = 'w1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            browse_reader_id, "w1",
            "Browse must navigate to the canonical page too"
        );
    }

    /// Write amplification: `persist_scan` runs on every scan (~475+/hr), so a scan that
    /// detected nothing must not touch the feed at all. Proven with a fixture that WOULD
    /// produce a row — the series has already had a detection, so the periodic rebuild's
    /// guards all pass — leaving the `new_found` gate as the only thing holding it back.
    #[tokio::test]
    async fn unchanged_scan_does_not_write_the_feed() {
        let pool = migrated_pool().await;
        seed_feed_fixture(&pool, Some("1785071625000")).await;
        let admin = ScanAdmin::default();

        record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(12),
            at("2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        let new_found = record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(13),
            at("2026-01-08T00:00:00Z"),
        )
        .await
        .unwrap();
        touch_feed_series_update(&pool, "7", new_found).await;
        assert!(feed_row(&pool).await.is_some());
        sqlx::query("DELETE FROM feed_series_updates")
            .execute(&pool)
            .await
            .unwrap();

        // Same chapter list again: no detection, so no write.
        let new_found = record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(13),
            at("2026-01-09T00:00:00Z"),
        )
        .await
        .unwrap();
        assert!(!new_found, "an identical list is not a detection");
        touch_feed_series_update(&pool, "7", new_found).await;
        assert!(
            feed_row(&pool).await.is_none(),
            "an unchanged scan must not write the feed"
        );
    }

    /// `released_at` is NOT NULL and a work with no dated chapter is not an "update":
    /// such a series is EXCLUDED, exactly as the periodic rebuild excludes it — never
    /// inserted with a NULL sort key (which would fail the write) nor with a fabricated
    /// one from our own clock.
    #[tokio::test]
    async fn series_without_a_release_time_produces_no_feed_row() {
        let pool = migrated_pool().await;
        seed_feed_fixture(&pool, None).await;
        let admin = ScanAdmin::default();

        record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(12),
            at("2026-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        let new_found = record_scan(
            &pool,
            "7",
            "S",
            &admin,
            &chaps(13),
            at("2026-01-08T00:00:00Z"),
        )
        .await
        .unwrap();
        assert!(new_found);
        touch_feed_series_update(&pool, "7", new_found).await;

        assert!(feed_row(&pool).await.is_none());
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feed_series_updates")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "an undated series is not an update");
    }

    // ── Phase E3: per-source scanners ────────────────────────────────────────────

    #[test]
    fn reconcile_delta_spawns_new_and_reaps_gone() {
        let running: HashSet<String> = ["a", UNASSIGNED_KEY]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let desired: HashSet<String> = ["b", UNASSIGNED_KEY]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (spawn, reap) = reconcile_delta(&running, &desired);
        assert_eq!(spawn, vec!["b".to_string()], "a new source is spawned");
        assert_eq!(reap, vec!["a".to_string()], "a gone source is reaped");
    }

    #[test]
    fn reconcile_delta_is_empty_when_stable() {
        let s: HashSet<String> = ["a", "b", UNASSIGNED_KEY]
            .iter()
            .map(|x| x.to_string())
            .collect();
        let (spawn, reap) = reconcile_delta(&s, &s);
        assert!(
            spawn.is_empty() && reap.is_empty(),
            "a stable set is a no-op"
        );
    }

    #[tokio::test]
    async fn per_source_due_query_isolates_by_source() {
        let pool = migrated_pool().await;
        for (sid, src) in [
            ("100", Some("s1")),
            ("101", Some("s1")),
            ("200", Some("s2")),
            ("300", None::<&str>),
        ] {
            sqlx::query(
                "INSERT INTO series_scan_state (series_id, source_id, next_scan_at, updated_at) \
                 VALUES (?, ?, ?, '2026-01-01T00:00:00Z')",
            )
            .bind(sid)
            .bind(src)
            .bind(DUE_NOW_SENTINEL)
            .execute(&pool)
            .await
            .unwrap();
        }
        let now = "2030-01-01T00:00:00Z";
        let mut s1 = due_series_ids_for_source(&pool, &SourceSel::Id("s1".into()), now, 10).await;
        s1.sort();
        assert_eq!(
            s1,
            vec!["100".to_string(), "101".to_string()],
            "only s1's series"
        );
        let s2 = due_series_ids_for_source(&pool, &SourceSel::Id("s2".into()), now, 10).await;
        assert_eq!(s2, vec!["200".to_string()], "only s2's series");
        let un =
            due_series_ids_for_source(&pool, &SourceSel::Unassigned { above: None }, now, 10).await;
        assert_eq!(
            un,
            vec!["300".to_string()],
            "the sweeper owns the unassigned row"
        );
    }

    #[tokio::test]
    async fn heal_assigns_source_id_and_leaves_unmappable_null() {
        let pool = migrated_pool().await;
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('w', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ('ss_500', 'w', 'suwayomi', 'srcX', '500', '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for sid in ["500", "999"] {
            sqlx::query(
                "INSERT INTO series_scan_state (series_id, next_scan_at, updated_at) \
                 VALUES (?, ?, '2026-01-01T00:00:00Z')",
            )
            .bind(sid)
            .bind(DUE_NOW_SENTINEL)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(
            heal_null_source_ids(&pool).await,
            1,
            "only the mappable row is healed"
        );
        let mapped: Option<String> =
            sqlx::query_scalar("SELECT source_id FROM series_scan_state WHERE series_id = '500'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(mapped.as_deref(), Some("srcX"));
        let unmapped: Option<String> =
            sqlx::query_scalar("SELECT source_id FROM series_scan_state WHERE series_id = '999'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            unmapped, None,
            "the unmappable row stays NULL for the sweeper"
        );
        assert_eq!(
            distinct_scan_source_ids(&pool, SOURCE_LOOP_SLOTS).await,
            vec!["srcX".to_string()],
            "the healed source now has a loop"
        );
    }

    #[tokio::test]
    async fn heal_is_idempotent_and_terminates() {
        // §8b's infinite-loop class: a work-list query whose writer does not share its
        // predicate re-offers the same rows forever. Here the SELECT subquery and the EXISTS
        // guard are the same predicate, and `source_series.source_id` is NOT NULL, so a healed
        // row can never come back NULL. Second pass must affect ZERO rows.
        let pool = migrated_pool().await;
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('w', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Including the '' ("unknown, e.g. backfill") source id the schema allows — it must
        // heal to a real (empty-string) shard rather than staying NULL forever.
        for (ss_id, src, key) in [("ss_1", "srcA", "1"), ("ss_2", "", "2")] {
            sqlx::query(
                "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
                 VALUES (?, 'w', 'suwayomi', ?, ?, '2024-01-01T00:00:00Z')",
            )
            .bind(ss_id)
            .bind(src)
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        }
        for sid in ["1", "2", "3"] {
            sqlx::query(
                "INSERT INTO series_scan_state (series_id, next_scan_at, updated_at) \
                 VALUES (?, ?, '2026-01-01T00:00:00Z')",
            )
            .bind(sid)
            .bind(DUE_NOW_SENTINEL)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert_eq!(heal_null_source_ids(&pool).await, 2, "both mappable rows");
        assert_eq!(
            heal_null_source_ids(&pool).await,
            0,
            "a second pass rewrites nothing — the heal converges instead of spinning"
        );
    }

    /// `EXPLAIN QUERY PLAN` lines for a statement, joined — the substrate for the
    /// sargability assertions below.
    async fn query_plan(pool: &SqlitePool, sql: &str) -> String {
        sqlx::query_as::<_, (i64, i64, i64, String)>(&format!("EXPLAIN QUERY PLAN {sql}"))
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, _, _, detail)| detail)
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[tokio::test]
    async fn the_e3_queries_are_index_seeks_not_table_scans() {
        // §8b's standing lesson: the difference between a seek and a scan here was 0.05 s vs
        // 12 minutes at 99.8% CPU. These run every 60 s per loop, so pin the plans.
        let pool = migrated_pool().await;

        // Per-source due-set — must ride migration 0094's (source_id, next_scan_at).
        let by_source = query_plan(
            &pool,
            "SELECT series_id FROM series_scan_state WHERE source_id = 'x' \
               AND next_scan_at <= '2026-01-01' ORDER BY next_scan_at ASC LIMIT 10",
        )
        .await;
        assert!(
            by_source.contains("idx_scan_state_source_next"),
            "per-source due-query lost its index: {by_source}"
        );
        assert!(
            !by_source.contains("TEMP B-TREE"),
            "the index must supply the ORDER BY too: {by_source}"
        );

        // Sweeper — `source_id IS NULL` is a valid range on the same index.
        let sweeper = query_plan(
            &pool,
            "SELECT series_id FROM series_scan_state WHERE source_id IS NULL \
               AND next_scan_at <= '2026-01-01' ORDER BY next_scan_at ASC LIMIT 10",
        )
        .await;
        assert!(
            sweeper.contains("idx_scan_state_source_next"),
            "sweeper due-query lost its index: {sweeper}"
        );

        // The heal's correlated lookup must probe `source_series` by (source_key,
        // source_type), NOT scan it once per scan-state row — and there must be no CAST on a
        // column anywhere in it (§8b).
        let heal = query_plan(
            &pool,
            "UPDATE series_scan_state \
                SET source_id = (SELECT ss.source_id FROM source_series ss \
                                  WHERE ss.source_type = 'suwayomi' \
                                    AND ss.source_key = series_scan_state.series_id LIMIT 1) \
              WHERE source_id IS NULL \
                AND EXISTS (SELECT 1 FROM source_series ss \
                             WHERE ss.source_type = 'suwayomi' \
                               AND ss.source_key = series_scan_state.series_id)",
        )
        .await;
        assert!(
            heal.contains("SEARCH ss"),
            "the heal must SEARCH source_series, not SCAN it per row: {heal}"
        );
    }

    #[test]
    fn sweeper_takes_the_residual_range_only_when_the_cap_bites() {
        let three: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            sweeper_cutoff(&three, 5),
            None,
            "under the cap every source has its own loop; the sweeper stays NULL-only"
        );
        assert_eq!(
            sweeper_cutoff(&three, 3),
            Some("c".to_string()),
            "at the cap the sweeper takes everything sorting above the last assigned id"
        );
        assert_eq!(sweeper_cutoff(&[], 3), None, "no sources, no residual");
    }

    #[tokio::test]
    async fn the_cap_never_orphans_a_row() {
        // THE ORPHAN PROOF, end to end. With only 2 loop slots and 4 sources, sources s3/s4
        // get no loop of their own. Every row must still be selected by exactly one selector.
        let pool = migrated_pool().await;
        for (sid, src) in [
            ("10", Some("s1")),
            ("20", Some("s2")),
            ("30", Some("s3")),
            ("40", Some("s4")),
            ("50", None::<&str>),
        ] {
            sqlx::query(
                "INSERT INTO series_scan_state (series_id, source_id, next_scan_at, updated_at) \
                 VALUES (?, ?, ?, '2026-01-01T00:00:00Z')",
            )
            .bind(sid)
            .bind(src)
            .bind(DUE_NOW_SENTINEL)
            .execute(&pool)
            .await
            .unwrap();
        }
        let slots = 2;
        let assigned = distinct_scan_source_ids(&pool, slots).await;
        assert_eq!(
            assigned,
            vec!["s1".to_string(), "s2".to_string()],
            "the assigned set is the ascending PREFIX, which is what makes the rest a range"
        );
        let cutoff = sweeper_cutoff(&assigned, slots);
        assert_eq!(cutoff.as_deref(), Some("s2"));

        let now = "2030-01-01T00:00:00Z";
        let mut covered: Vec<String> = Vec::new();
        for id in &assigned {
            covered.extend(
                due_series_ids_for_source(&pool, &SourceSel::Id(id.clone()), now, 10).await,
            );
        }
        covered.extend(
            due_series_ids_for_source(&pool, &SourceSel::Unassigned { above: cutoff }, now, 10)
                .await,
        );
        covered.sort();
        assert_eq!(
            covered,
            vec![
                "10".to_string(),
                "20".to_string(),
                "30".to_string(),
                "40".to_string(),
                "50".to_string()
            ],
            "every row is scanned by exactly one selector — no duplicates, no orphans"
        );
    }
}
