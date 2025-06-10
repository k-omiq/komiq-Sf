//! Extension-level source sync (source-sync §5).
//!
//! A one-time bulk ingest ("add all from extension") walks a source's listing ONCE,
//! so series added to that extension *later* are never discovered. And a single-added
//! series that dropped out of the Suwayomi library stops being scanned entirely. This
//! background job closes both gaps for every *subscribed* extension
//! (`extension_subscription`, driven by the admin `setExtensionSubscription` toggle):
//!
//!   1. **Reconcile** — backfill a `series_scan_state` row for every enrolled series that
//!      lacks one (that row is what the DB-driven scanner scans from), and re-assert
//!      `inLibrary=true` upstream for enrolled series missing from the library so
//!      Suwayomi's own state stays consistent for drifted/single-added series.
//!   2. **Discover** — re-walk each subscribed extension's sources (LATEST) and
//!      auto-enrol any series we don't have yet, via the same `ingest_source_series`
//!      path the bulk ingest uses (in-library → dedup → scan-on-enrol).
//!
//! Chapter *updates* are NOT produced here: Suwayomi's source listing carries no
//! chapter list, only per-manga metadata, so new-chapter detection stays with the
//! per-series scanner (`crate::scanner`). This job's job is discovery + enrolment;
//! keeping series in the library is what lets the scanner keep them fresh.
//!
//! Mirrors the `scanner`/`ingest` background-task conventions: supervised + restarted
//! on panic, every per-item error logged and skipped, never panics the process.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use crate::graphql::AppState;
use crate::suwayomi::FetchType;

/// Extensions with a sync pass currently in flight — a single-flight guard so a rapid
/// re-toggle (or a subscribe kick overlapping the daily pass) can't run two concurrent
/// LATEST walks of the same source (audit LOW). Enrolment is idempotent, so the harm is
/// only redundant upstream fetches, but skipping the overlap is cheap.
fn running_pkgs() -> &'static std::sync::Mutex<HashSet<String>> {
    static S: std::sync::OnceLock<std::sync::Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(HashSet::new()))
}

/// RAII single-flight token for one extension's sync pass; releases on drop (incl. panic).
struct PkgGuard(String);
impl PkgGuard {
    fn try_acquire(pkg: &str) -> Option<PkgGuard> {
        let mut set = running_pkgs().lock().unwrap_or_else(|e| e.into_inner());
        set.insert(pkg.to_string())
            .then(|| PkgGuard(pkg.to_string()))
    }
}
impl Drop for PkgGuard {
    fn drop(&mut self) {
        running_pkgs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Polite throttle between browse pages — each page is one upstream fetch.
const PAGE_DELAY_MS: u64 = 750;
/// Polite throttle between reconcile `set_in_library` heal calls, so a first-run mass
/// drift can't fire thousands of sequential upstream writes back-to-back.
const HEAL_DELAY_MS: u64 = 50;
/// Stop a source's LATEST walk after this many CONSECUTIVE fully-already-known pages.
/// LATEST is newest/recently-updated first, so a run of all-known pages means we've
/// caught up; the `max_pages` cap still bounds the walk when this never trips.
const STOP_AFTER_KNOWN_PAGES: u32 = 2;

/// Floor on how long a restart-skipped tick defers the next attempt. Without a floor a
/// pass stamped a fraction of a second ago would re-fire immediately.
const RESTART_DEFER_MIN_SECS: u64 = 60;

/// Has a graceful shutdown been requested? Checked inside the long per-item loops so a
/// pass in flight yields promptly instead of running to completion (P1-4).
fn stopping(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

/// Seconds since the last COMPLETED full pass, or `None` if none has ever run.
///
/// `catalog::source_sync_due` answers the yes/no question; the scheduler additionally
/// needs the elapsed time, so it can defer a restart-skipped tick by the REMAINDER of
/// the interval rather than by a whole fresh one (P1-3).
async fn seconds_since_last_pass(pool: &sqlx::SqlitePool) -> Option<u64> {
    let last: Option<String> =
        sqlx::query_scalar("SELECT last_full_pass_at FROM sync_state WHERE id = 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let t = chrono::DateTime::parse_from_rfc3339(&last?).ok()?;
    Some(
        (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
            .num_seconds()
            .max(0) as u64,
    )
}

/// Spawn the source-sync scheduler. Runs until `shutdown` resolves. Supervised: a
/// panic in the tick loop is logged and the loop restarts after a short backoff.
pub fn spawn(
    state: Arc<AppState>,
    interval_seconds: u64,
    max_pages: i64,
    shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_loop(
                state.clone(),
                interval_seconds,
                max_pages,
                shutdown.clone(),
            ));
            match handle.await {
                Ok(()) => break,
                Err(e) if e.is_panic() => {
                    if *shutdown.borrow() {
                        break;
                    }
                    tracing::error!("source-sync loop panicked; restarting in 30s");
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if *shutdown.borrow() {
                        break;
                    }
                }
                Err(_) => break, // cancelled
            }
        }
    });
}

async fn run_loop(
    state: Arc<AppState>,
    interval_seconds: u64,
    max_pages: i64,
    mut shutdown: watch::Receiver<bool>,
) {
    // Start 30s behind the scanner so the boot burst is spread (see scanner::run_loop).
    // This also gives Suwayomi a moment to finish accepting connections: the reconcile
    // below failed at boot because it ran 57s after process start while Suwayomi was
    // still coming up.
    //
    // Deadline-driven rather than a fixed `interval`, because the retry cadence is not
    // constant: an incomplete pass must come back in minutes, a complete one in
    // `interval_seconds`. See RECONCILE_RETRY_SECS.
    let mut next_at = Instant::now() + Duration::from_secs(30);
    tracing::info!(interval_seconds, max_pages, "source-sync scheduler started");
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next_at) => {
                // The first run also happens on every restart. Skip the pass when a full
                // one completed less than the interval ago, so frequent redeploys don't
                // re-run the upstream-heavy reconcile + LATEST walks on every boot
                // (audit #3). Scheduled runs are `interval` apart, so they're always due;
                // only the redundant restart run is skipped.
                if !crate::catalog::source_sync_due(&state.pool, interval_seconds).await {
                    // Defer by what REMAINS of the interval since the last completed pass,
                    // not by a whole fresh one (P1-3). The old code restarted the clock at
                    // the restart, so every redeploy pushed the next reconcile out another
                    // full interval — with the 1-day default, redeploying more often than
                    // ~21.6h meant the pass never ran at all. Production: boot at
                    // 07-24T12:13:23, skip logged at 12:13:53, next actual pass a full 24h
                    // after the RESTART, a real gap of up to ~45h.
                    let elapsed = seconds_since_last_pass(&state.pool).await.unwrap_or(0);
                    let defer = interval_seconds
                        .saturating_sub(elapsed)
                        .max(RESTART_DEFER_MIN_SECS);
                    tracing::info!(
                        elapsed_secs = elapsed,
                        defer_secs = defer,
                        "source-sync: a recent pass exists — skipping restart tick"
                    );
                    next_at = Instant::now() + Duration::from_secs(defer);
                    continue;
                }
                // Race the pass against shutdown (P1-4). `sync_all(...).await` used to be
                // awaited INSIDE this arm, so the `shutdown.changed()` arm below could not
                // fire until the pass returned — a 5m15s pass (of which >=140s was pure
                // per-item sleeps across a 2,811-item loop) swallowed any SIGTERM in that
                // window, was then killed mid-flight, and because `mark_source_sync_pass`
                // never ran the entire pass was redone from scratch on the next boot.
                // `inner` is a clone so the select below can still borrow `shutdown`
                // mutably; it also lets the per-item loops bail early on their own.
                let inner = shutdown.clone();
                let mut watcher = shutdown.clone();
                let outcome = tokio::select! {
                    o = sync_all(&state, max_pages, &inner) => o,
                    _ = watcher.changed() => {
                        if *watcher.borrow() {
                            tracing::info!("source-sync: shutdown during pass — abandoning");
                            break;
                        }
                        continue;
                    }
                };
                if *inner.borrow() {
                    tracing::info!("source-sync scheduler stopping");
                    break;
                }
                tracing::info!(
                    subscriptions = outcome.subscriptions,
                    series_added = outcome.series_added,
                    reconciled = outcome.reconciled,
                    "source-sync pass complete"
                );
                // An INCOMPLETE pass must retry soon. Previously the code declined to
                // stamp the pass and commented that "a restart then retries instead of
                // waiting a full interval" — but nothing re-fired early, so a single
                // transient failure meant a full `interval_seconds` blackout. In
                // production the reconcile failed 57s after boot (Suwayomi not yet
                // accepting connections), `sync_state` stayed empty for the entire
                // uptime, and the library drift heal never ran once.
                next_at = Instant::now()
                    + if outcome.reconciled {
                        Duration::from_secs(interval_seconds)
                    } else {
                        Duration::from_secs(RECONCILE_RETRY_SECS.min(interval_seconds))
                    };
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("source-sync scheduler stopping");
                    break;
                }
            }
        }
    }
}

/// How soon to retry after a pass whose reconcile did not complete. Short enough that
/// a transient upstream blip costs minutes rather than a full sync interval, long
/// enough not to hammer a genuinely-down Suwayomi. Clamped to the configured interval
/// so a deployment with a very short interval never retries *slower* than normal.
const RECONCILE_RETRY_SECS: u64 = 900; // 15 minutes

/// Outcome of one source-sync pass.
pub struct SyncOutcome {
    pub subscriptions: usize,
    pub series_added: i64,
    /// Whether the library reconcile completed. When false the pass is NOT stamped and
    /// the scheduler retries early.
    pub reconciled: bool,
}

/// One full pass: refresh extension coordinates, reconcile library membership + scan
/// tracking, then discover new series for every subscribed extension. Returns
/// `(subscriptions_processed, series_added)`.
pub async fn sync_all(
    state: &AppState,
    max_pages: i64,
    shutdown: &watch::Receiver<bool>,
) -> SyncOutcome {
    // Extension coordinates change rarely; refresh them here (daily) rather than on
    // every scan tick. Non-fatal.
    crate::scanner::record_source_extensions(state).await;
    let reconciled = reconcile_library(state, shutdown).await;

    // Give any breaker-disabled subscription a probe pass once its disablement is stale
    // (P1-5) — before enumerating subscriptions, so a re-armed one is walked this pass.
    rearm_stale_breakers(state).await;

    let subs = match crate::catalog::subscribed_extensions(&state.pool).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "source-sync: failed to list subscriptions");
            // Couldn't even enumerate subscriptions — treat the pass as incomplete so
            // the scheduler retries early rather than waiting a full interval.
            return SyncOutcome {
                subscriptions: 0,
                series_added: 0,
                reconciled: false,
            };
        }
    };
    let mut total_added = 0i64;
    for pkg in &subs {
        if stopping(shutdown) {
            break;
        }
        total_added += sync_extension_inner(state, pkg, max_pages, shutdown).await;
    }
    // Stamp completion only when the reconcile actually SUCCEEDED, so a pass aborted by a
    // transient upstream/DB failure (library fetch down, couldn't enumerate enrolled) does
    // NOT throttle the next attempt as if it had finished — a restart then retries instead
    // of waiting a full interval (audit #3 follow-up). Stamped LAST, after the walks.
    if reconciled {
        if let Err(e) = crate::catalog::mark_source_sync_pass(&state.pool).await {
            tracing::warn!(error = %e, "source-sync: failed to record pass completion");
        }
    } else {
        tracing::warn!(
            retry_in_secs = RECONCILE_RETRY_SECS,
            "source-sync: reconcile incomplete — not stamping pass; retrying early"
        );
    }
    SyncOutcome {
        subscriptions: subs.len(),
        series_added: total_added,
        reconciled,
    }
}

/// Re-assert `inLibrary=true` upstream for every enrolled Suwayomi series that is NOT
/// currently in the library, and backfill scan-state rows. Runs once per interval
/// regardless of subscriptions (it heals single-adds), and is throttled against restarts
/// by `source_sync_due`. Cheap in steady state — after the first heal the "missing" set is
/// empty, so no upstream writes happen. The membership fetch is the paginated, id-only
/// `library_ids()` (bounded pages, no 100k-record materialisation). Scan eligibility now
/// flows from the backfilled `series_scan_state` row, NOT
/// from `library()` membership (the DB-driven scanner no longer iterates the library);
/// the in-library re-assert keeps Suwayomi's own state consistent.
/// Returns whether the reconcile completed (so `sync_all` only stamps a genuinely finished
/// pass). `false` = a transient failure (couldn't enumerate enrolled series, or the library
/// fetch failed) aborted it partway and it should be retried, not throttled.
async fn reconcile_library(state: &AppState, shutdown: &watch::Receiver<bool>) -> bool {
    // Purge leaked non-English series FIRST, before anything re-asserts them. Komika
    // serves English only, but the multi-language `all.mangadex` extension had enrolled
    // ~59 languages of series (native-language titles + chapters) that leaked into
    // Browse before the English-only enrolment filter landed. Removing them here, ahead
    // of the scan-state backfill + in-library heal below, ensures neither step re-adds
    // them. Best-effort + idempotent: once drained the target set is empty and this is a
    // no-op. Purged series are also dropped from Suwayomi's own library (throttled).
    match crate::catalog::purge_foreign_language_suwayomi(&state.pool).await {
        Ok(ids) if ids.is_empty() => {}
        Ok(ids) => {
            tracing::info!(
                purged = ids.len(),
                "source-sync: purged leaked non-English Suwayomi series"
            );
            for id in ids {
                // Shutdown is checked per item, not just per pass (P1-4): this loop
                // sleeps HEAL_DELAY_MS between every element, so a few thousand items is
                // minutes of wall clock during which a SIGTERM would otherwise go unseen.
                if stopping(shutdown) {
                    return false;
                }
                if let Err(e) = state.suwayomi.set_in_library(id, false).await {
                    tracing::warn!(series_id = id, error = %e, "source-sync: un-library of purged non-English series failed");
                }
                tokio::time::sleep(Duration::from_millis(HEAL_DELAY_MS)).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, "source-sync: non-English purge failed"),
    }

    // Backfill a "due now" scan-state row for any enrolled series that lacks one (e.g.
    // federated-search enrolments, which don't scan-on-enrol) so the DB-driven scanner
    // tracks them. One set-based query; cheap and idempotent.
    match crate::catalog::backfill_pending_scan_states(&state.pool).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(added = n, "source-sync: backfilled scan-state rows"),
        Err(e) => tracing::warn!(error = %e, "source-sync: scan-state backfill failed"),
    }

    let enrolled = match crate::catalog::suwayomi_source_keys(&state.pool).await {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(error = %e, "source-sync: failed to list enrolled series");
            return false;
        }
    };
    if enrolled.is_empty() {
        return true;
    }
    // Paginated, id-only fetch — reconcile only needs the membership set, so don't
    // materialise 100k+ full manga records (audit #5).
    let in_library: HashSet<String> = match state.suwayomi.library_ids().await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "source-sync: failed to list library for reconcile");
            return false;
        }
    };
    let mut healed = 0usize;
    for key in enrolled {
        if in_library.contains(&key) {
            continue;
        }
        // Same per-item shutdown check as the purge loop above. An aborted reconcile
        // reports `false`, so the pass is NOT stamped and the next boot retries early
        // rather than treating a half-finished heal as a completed pass.
        if stopping(shutdown) {
            return false;
        }
        let Ok(id) = key.parse::<i64>() else { continue };
        match state.suwayomi.set_in_library(id, true).await {
            Ok(()) => healed += 1,
            Err(e) => {
                tracing::warn!(series_id = id, error = %e, "source-sync: reconcile set_in_library failed")
            }
        }
        // Throttle the heal like the LATEST walk: a first-run mass drift could otherwise
        // fire thousands of sequential `set_in_library` writes back-to-back at the engine
        // (audit LOW). Steady state the missing set is empty, so this never runs.
        tokio::time::sleep(Duration::from_millis(HEAL_DELAY_MS)).await;
    }
    if healed > 0 {
        tracing::info!(
            healed,
            "source-sync: re-enrolled drifted series into the library"
        );
    }
    true
}

/// How long a breaker-disabled subscription stays disabled before it is given one probe
/// pass. The breaker exists to stop re-walking a genuinely dead source every day, not to
/// be a permanent ban: a source that 502'd for a week may be fine now, and until this
/// existed the ONLY way back was an admin re-subscribe. Seven days is several sync
/// intervals at the 1-day default, so a still-dead source re-trips almost immediately and
/// costs one wasted walk a week.
const BREAKER_REARM_HOURS: i64 = 24 * 7;

/// Clear `disabled_at` on any subscription whose disablement is older than
/// `BREAKER_REARM_HOURS`, so it gets a probe pass (P1-5). Best-effort: a failure here
/// just means the re-arm happens on a later pass.
async fn rearm_stale_breakers(state: &AppState) {
    let horizon = (chrono::Utc::now() - chrono::Duration::hours(BREAKER_REARM_HOURS)).to_rfc3339();
    let stale: Vec<String> = match sqlx::query_scalar(
        "SELECT pkg_name FROM extension_subscription \
         WHERE disabled_at IS NOT NULL AND disabled_at < ?",
    )
    .bind(&horizon)
    .fetch_all(&state.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "source-sync: breaker re-arm query failed");
            return;
        }
    };
    for pkg in stale {
        // Probe re-arm, NOT a full reset: this is an automatic timer re-arm, so a
        // still-dead source must re-trip on its very next failing walk. A full clear
        // (`reset_subscription_breaker`) would hand it back the whole
        // SUBSCRIPTION_FAILURE_LIMIT budget, costing 5 failing walks per cycle instead
        // of 1. The full clear stays for the admin re-subscribe, where "I've fixed it"
        // is a human assertion worth trusting.
        match crate::catalog::rearm_subscription_breaker_probe(&state.pool, &pkg).await {
            Ok(()) => tracing::info!(
                pkg = %pkg,
                rearm_hours = BREAKER_REARM_HOURS,
                "source-sync: re-arming breaker-disabled subscription for a probe pass"
            ),
            Err(e) => tracing::warn!(pkg = %pkg, error = %e, "source-sync: breaker re-arm failed"),
        }
    }
}

/// Is this failure OURS rather than the source's?
///
/// The circuit breaker's job is to stop re-walking a dead source. It must not count
/// failures reaching our own infrastructure: production had `drakescans` carrying two
/// strikes for `flaresolverr: Temporary failure in name resolution` — a DNS lookup of our
/// own container, not a symptom of the source at all. At the 1-day cadence that
/// auto-disables in three days, and a five-day flaresolverr outage would have silently
/// disabled all twelve subscriptions with an admin re-subscribe as the only way back.
/// Deliberately generous: failing to trip the breaker costs one wasted walk per interval,
/// whereas tripping it wrongly costs the entire extension's discovery until someone
/// notices.
///
/// `"suwayomi error 5xx"` is `SuwayomiClient::gql`'s wording for a NON-2xx HTTP status
/// from OUR OWN engine (`anyhow!("Suwayomi error {}", res.status())`) — the source is
/// never reached at all, so a 5xx there is as much ours as a DNS failure. It was the
/// hole in this list: `504 Gateway Timeout` happened to be caught by the `"timeout"`
/// needle while `502 Bad Gateway` / `503 Service Unavailable` were not, so the SAME
/// class of failure was classified two different ways. That matters because the sync
/// scheduler's first run is 30s after boot and a redeploy restarts the Suwayomi
/// container: an engine that is listening but not yet ready answers 502/503, which
/// would have put a strike on EVERY subscription in the same pass — five such deploys
/// and all twelve auto-disable, with an admin re-subscribe the only way back. That is
/// precisely the outcome this function exists to prevent.
fn is_infrastructure_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "flaresolverr",
        "name resolution",
        "dns error",
        "connection refused",
        "connection reset",
        "connection closed",
        "broken pipe",
        "timed out",
        "timeout",
        "os error 111",
        "os error 113",
        "error sending request",
        "tcp connect error",
        "channel closed",
        "no route to host",
        "network is unreachable",
        // Our own Suwayomi engine, not the source. `res.status()` renders as
        // "502 Bad Gateway" etc., so the prefix + leading 5 covers the whole class.
        "suwayomi error 5",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

/// Discover + auto-enrol new series for ONE extension, across all its sources. Records
/// the pass outcome on the subscription row. Returns the number of series enrolled.
///
/// Shutdown-oblivious entry point, for the interactive on-subscribe kick which has no
/// shutdown handle; the scheduler uses `sync_extension_inner`.
pub async fn sync_extension(state: &AppState, pkg_name: &str, max_pages: i64) -> i64 {
    let (_tx, rx) = watch::channel(false);
    sync_extension_inner(state, pkg_name, max_pages, &rx).await
}

/// `sync_extension`, plus a shutdown handle threaded into the per-page walk (P1-4).
async fn sync_extension_inner(
    state: &AppState,
    pkg_name: &str,
    max_pages: i64,
    shutdown: &watch::Receiver<bool>,
) -> i64 {
    // Single-flight per extension (audit LOW): a concurrent pass for the same pkg (rapid
    // re-toggle, or a subscribe kick landing during the daily pass) just skips.
    let Some(_guard) = PkgGuard::try_acquire(pkg_name) else {
        tracing::info!(
            pkg = pkg_name,
            "source-sync: extension already syncing — skipping"
        );
        return 0;
    };
    let sources = match state.suwayomi.list_sources().await {
        Ok(list) => list
            .into_iter()
            .filter(|s| s.pkg_name.as_deref() == Some(pkg_name))
            // English-only: a multi-language extension (notably `all.mangadex`)
            // exposes ~70 per-language sources under one pkg. Without this filter,
            // discovery walked EVERY language and enrolled non-English series (with
            // native-language titles + chapters) that leaked into Browse. Komika
            // serves English only, so restrict enrolment to the `en` source(s) —
            // matching `SuwayomiClient::resolve_source`'s English preference.
            .filter(|s| s.lang == "en")
            .map(|s| s.id)
            .collect::<Vec<_>>(),
        Err(e) => {
            let msg = format!("list_sources failed: {e}");
            tracing::warn!(pkg = pkg_name, error = %e, "source-sync: list_sources failed");
            // Reaching the engine at all is infrastructure; never a strike (P1-5).
            if is_infrastructure_error(&msg) {
                tracing::info!(
                    pkg = pkg_name,
                    "source-sync: infrastructure failure — not counted against the breaker"
                );
                return 0;
            }
            if let Ok(true) =
                crate::catalog::mark_subscription_synced(&state.pool, pkg_name, 0, Some(&msg)).await
            {
                tracing::error!(
                    pkg = pkg_name,
                    limit = crate::catalog::SUBSCRIPTION_FAILURE_LIMIT,
                    "source-sync: subscription auto-disabled after consecutive failures"
                );
            }
            return 0;
        }
    };

    let mut added = 0i64;
    let mut error: Option<String> = None;
    // Failures that are ours, not the source's, are logged but withheld from the breaker.
    let mut infra_error: Option<String> = None;
    for source_id in &sources {
        if stopping(shutdown) {
            break;
        }
        match sync_source_latest(state, source_id, max_pages, shutdown).await {
            Ok(n) => added += n,
            Err(e) => {
                tracing::warn!(pkg = pkg_name, source_id, error = %e, "source-sync: source walk failed");
                let msg = format!("source {source_id}: {e}");
                if is_infrastructure_error(&msg) {
                    infra_error = Some(msg);
                } else {
                    error = Some(msg);
                }
            }
        }
    }
    // A pass that only hit infrastructure failures records NOTHING: stamping it clean
    // would reset a genuine failure streak and clear `last_error`, hiding the outage;
    // stamping it failed would count our own DNS/FlareSolverr trouble as the source's.
    if error.is_none() {
        if let Some(msg) = &infra_error {
            tracing::warn!(
                pkg = pkg_name,
                last_error = %msg,
                "source-sync: infrastructure failure — subscription state left untouched"
            );
            return added;
        }
    }
    match crate::catalog::mark_subscription_synced(&state.pool, pkg_name, added, error.as_deref())
        .await
    {
        Ok(true) => tracing::error!(
            pkg = pkg_name,
            limit = crate::catalog::SUBSCRIPTION_FAILURE_LIMIT,
            last_error = error.as_deref().unwrap_or(""),
            "source-sync: subscription auto-disabled after consecutive failures; \
             re-enable from the admin console once the source is healthy"
        ),
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(pkg = pkg_name, error = %e, "source-sync: failed to record pass outcome")
        }
    }
    if added > 0 {
        tracing::info!(pkg = pkg_name, added, "source-sync: enrolled new series");
    }
    added
}

/// Walk one source's LATEST listing, auto-enrolling every series we don't yet have.
/// Stops after `STOP_AFTER_KNOWN_PAGES` consecutive fully-known pages (caught up), when
/// the source reports no next page, or at the `max_pages` cap (logged — a capped sweep
/// is not "fully caught up").
async fn sync_source_latest(
    state: &AppState,
    source_id: &str,
    max_pages: i64,
    shutdown: &watch::Receiver<bool>,
) -> anyhow::Result<i64> {
    let mut added = 0i64;
    let mut page: i32 = 1;
    let mut consecutive_known = 0u32;
    loop {
        // Per-page shutdown check (P1-4): a walk is `max_pages` upstream fetches with a
        // PAGE_DELAY_MS throttle between them, so it can run for minutes.
        if stopping(shutdown) {
            break;
        }
        let (has_next, mangas) = state
            .suwayomi
            .browse_source(source_id, FetchType::Latest, page, None)
            .await?;
        if mangas.is_empty() {
            break;
        }

        // Which of this page's series are not yet enrolled? (Suwayomi manga ids are the
        // `source_key` and globally unique, so a key-only lookup is unambiguous.) One
        // set-based query per page instead of a lookup per manga (audit #10).
        let keys: Vec<String> = mangas.iter().map(|m| m.id.to_string()).collect();
        let known = crate::catalog::existing_source_keys(&state.pool, "suwayomi", &keys).await?;
        let new_ids: Vec<i64> = mangas
            .iter()
            .filter(|m| !known.contains(&m.id.to_string()))
            .map(|m| m.id)
            .collect();

        if new_ids.is_empty() {
            consecutive_known += 1;
            if consecutive_known >= STOP_AFTER_KNOWN_PAGES {
                break;
            }
        } else {
            consecutive_known = 0;
            // Enrol sequentially — discovery volume per pass is small (only genuinely
            // new series), and this keeps the upstream engine gently loaded. Each
            // `ingest_source_series` sets in-library, dedups, and scan-on-enrols.
            for id in new_ids {
                if stopping(shutdown) {
                    break;
                }
                match crate::graphql::ingest_source_series(state, &id.to_string()).await {
                    Ok(_) => added += 1,
                    Err(e) => {
                        tracing::warn!(source_id, manga_id = id, error = %e, "source-sync: enrol failed")
                    }
                }
            }
        }

        if !has_next {
            break;
        }
        if page as i64 >= max_pages {
            tracing::info!(
                source_id,
                max_pages,
                "source-sync: hit page cap — sweep truncated, not necessarily fully caught up"
            );
            break;
        }
        page += 1;
        tokio::time::sleep(Duration::from_millis(PAGE_DELAY_MS)).await;
    }
    Ok(added)
}

/// Fire-and-forget a single-extension sync (the `setExtensionSubscription` enable path),
/// so the admin sees discovery results without waiting for the next scheduler tick.
/// `SOURCE_SYNC_MAX_PAGES` isn't threaded here; a modest cap keeps this interactive kick
/// bounded (a fuller sweep runs on the next scheduled pass).
pub fn spawn_extension_sync(state: Arc<AppState>, pkg_name: String) {
    tokio::spawn(async move {
        let added = sync_extension(&state, &pkg_name, DEFAULT_KICK_MAX_PAGES).await;
        tracing::info!(pkg = %pkg_name, added, "source-sync: on-subscribe kick complete");
    });
}

/// Page cap for the on-subscribe interactive kick (see `spawn_extension_sync`).
const DEFAULT_KICK_MAX_PAGES: i64 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    /// P1-5: the breaker must not count our own infrastructure against a source. The
    /// live case was `drakescans` carrying strikes for a DNS failure resolving our own
    /// flaresolverr container — three more daily passes and it would have auto-disabled
    /// a perfectly healthy source, with an admin re-subscribe the only way back.
    #[test]
    fn infrastructure_failures_are_not_the_sources_fault() {
        for ours in [
            "source drakescans: flaresolverr: Temporary failure in name resolution",
            "list_sources failed: error sending request for url",
            "source x: operation timed out",
            "source x: tcp connect error: Connection refused (os error 111)",
            "source x: Network is unreachable",
        ] {
            assert!(is_infrastructure_error(ours), "should be ours: {ours}");
        }
        for theirs in [
            "source x: unexpected status 404",
            "source x: failed to parse response body",
            "list_sources failed: GraphQL error: unknown field",
            "source x: source returned no results",
        ] {
            assert!(
                !is_infrastructure_error(theirs),
                "should count against the breaker: {theirs}"
            );
        }
    }

    /// Our own Suwayomi engine returning a 5xx must not be charged to the source.
    ///
    /// `SuwayomiClient::gql` renders every non-2xx from the engine as
    /// `"Suwayomi error {status}"`, and `status` Displays as e.g. `502 Bad Gateway`.
    /// Before this was covered, `504 Gateway Timeout` was (accidentally, via the
    /// `"timeout"` needle) treated as ours while `502`/`503` were charged to the source —
    /// so a Suwayomi restart during a deploy struck EVERY subscription in one pass.
    /// A 4xx from the engine stays a strike: that is a request we got wrong, not a blip.
    #[test]
    fn our_own_engine_5xx_is_not_the_sources_fault() {
        for ours in [
            "list_sources failed: Suwayomi error 502 Bad Gateway",
            "source 1: Suwayomi error 503 Service Unavailable",
            "source 1: Suwayomi error 504 Gateway Timeout",
            "source 1: Suwayomi error 500 Internal Server Error",
        ] {
            assert!(is_infrastructure_error(ours), "should be ours: {ours}");
        }
        // The engine rejecting OUR request is not a transient infrastructure blip.
        assert!(!is_infrastructure_error(
            "list_sources failed: Suwayomi error 400 Bad Request"
        ));
    }

    /// The two failures actually sitting in production's `extension_subscription`
    /// table must still be charged to their sources — they are the reason the breaker
    /// exists, and a needle that swallowed them would disable it outright. Both arrive
    /// as multi-kilobyte Java stack traces from the Suwayomi engine, so this pins the
    /// classifier against the real text rather than a tidy one-liner.
    #[test]
    fn real_production_source_failures_still_count_against_the_breaker() {
        let cloudflare = "source 8284219650954312474: Exception while fetching data \
             (/fetchSourceManga) : Cloudflare bypass currently disabled\r\n\r\n\
             java.io.IOException: Cloudflare bypass currently disabled\n\tat \
             eu.kanade.tachiyomi.network.interceptor.CloudflareInterceptor.intercept\
             (CloudflareInterceptor.kt:52)\n\tat \
             okhttp3.internal.connection.RealCall.execute(RealCall.kt:187)\n";
        let http_404 = "source 1061713767402958340: Exception while fetching data \
             (/fetchSourceManga) : HTTP error 404\r\n\r\n\
             eu.kanade.tachiyomi.network.HttpException: HTTP error 404\n\tat \
             okhttp3.internal.connection.RealCall.execute(RealCall.kt:187)\n";
        for theirs in [cloudflare, http_404] {
            assert!(
                !is_infrastructure_error(theirs),
                "a real source failure must still take a strike"
            );
        }
    }

    /// P1-3: a restart-skipped tick must defer by what REMAINS of the interval, not by a
    /// whole fresh one — otherwise redeploying more often than ~0.9x the interval means
    /// the reconcile pass never runs at all.
    #[tokio::test]
    async fn restart_skip_defers_by_the_remainder_not_a_full_interval() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        assert_eq!(
            seconds_since_last_pass(&pool).await,
            None,
            "no pass recorded yet"
        );

        let six_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(6)).to_rfc3339();
        sqlx::query("INSERT INTO sync_state (id, last_full_pass_at) VALUES (1, ?)")
            .bind(&six_hours_ago)
            .execute(&pool)
            .await
            .unwrap();

        let interval = 24 * 3600u64;
        let elapsed = seconds_since_last_pass(&pool).await.unwrap();
        assert!(
            (21_000..=22_000).contains(&elapsed),
            "expected ~6h elapsed, got {elapsed}s"
        );
        let defer = interval.saturating_sub(elapsed).max(RESTART_DEFER_MIN_SECS);
        assert!(
            defer < interval,
            "a restart must not push the pass out a whole extra interval ({defer}s vs {interval}s)"
        );
        assert!(
            (63_000..=66_000).contains(&defer),
            "expected ~18h remaining, got {defer}s"
        );
    }
}
