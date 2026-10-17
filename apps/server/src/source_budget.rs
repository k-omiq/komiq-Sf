//! One per-process rate budget for every background consumer of an upstream source.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use crate::config::SourceRateLimit;
use crate::rate_limit::RateLimiter;
use crate::scanner::EffectiveRateLimit;

#[derive(Clone, Copy, Debug)]
pub(crate) enum SourceUse {
    Scan,
    /// One discovery or ingest listing page.
    Discovery,
    Sync,
    /// An ingest item's series detail and chapter list, like a scan.
    Ingest,
    MediaPage,
    Cover,
    /// One request a regional engine passes on to the source for a read (search,
    /// chapter list, page list).
    Regional,
}

impl SourceUse {
    fn cost(self, pace: Pace) -> u32 {
        match self {
            Self::Scan | Self::Ingest => pace.requests_per_scan,
            Self::Discovery | Self::Sync | Self::MediaPage | Self::Cover | Self::Regional => 1,
        }
    }
}

pub(crate) struct SourceBudgets {
    throttles: std::sync::RwLock<HashMap<String, Arc<SourceThrottle>>>,
    media_pages: RateLimiter,
}

impl Default for SourceBudgets {
    fn default() -> Self {
        Self {
            throttles: Default::default(),
            media_pages: RateLimiter::new(600, 3600),
        }
    }
}

impl SourceBudgets {
    /// The throttle for one source right now, if it has a limit.
    pub(crate) fn throttle(&self, source_id: &str) -> Option<Arc<SourceThrottle>> {
        self.throttles
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(source_id)
            .cloned()
    }

    /// Make the live throttles match `limits`: add the new ones, re-pace the changed ones
    /// IN PLACE (their recent starts survive, so an edit never hands a source a fresh
    /// window on top of the one it just spent), and drop the ones no longer listed.
    pub(crate) fn apply_limits(&self, limits: &[EffectiveRateLimit]) {
        let mut map = self.throttles.write().unwrap_or_else(|e| e.into_inner());
        map.retain(|source_id, _| {
            let keep = limits.iter().any(|l| &l.limit.source_id == source_id);
            if !keep {
                tracing::info!(source_id = %source_id, "scan supervisor: per-source rate limit removed");
            }
            keep
        });
        for l in limits {
            let l = &l.limit;
            let changed = match map.get(&l.source_id) {
                Some(t) => t.set_limit(l),
                None => {
                    map.insert(l.source_id.clone(), Arc::new(SourceThrottle::new(l)));
                    true
                }
            };
            if changed {
                // Logged per source so the effective cap is answerable from the logs alone
                // (the same reason `global_permits` is logged): a throttle that silently
                // did not apply is the failure mode that gets a source banned twice.
                tracing::info!(
                    source_id = %l.source_id,
                    requests = l.requests,
                    window_secs = l.window_secs,
                    requests_per_scan = l.requests_per_scan,
                    scans_per_window = l.scans_per_window(),
                    gap_secs = l.window_secs / l.scans_per_window() as u64,
                    "scan supervisor: per-source rate limit active"
                );
            }
        }
    }

    /// Wait for this source's request budget; sources without a limit continue at once.
    pub(crate) async fn spend(
        &self,
        source_id: &str,
        usage: SourceUse,
        shutdown: &watch::Receiver<bool>,
    ) -> bool {
        if *shutdown.borrow() {
            return false;
        }
        match self.throttle(source_id) {
            Some(throttle) => throttle.acquire(usage, shutdown).await,
            None => true,
        }
    }

    /// Wait up to `max_wait` for this source's budget, for a request someone is waiting
    /// on; sources without a limit continue at once. A refusal records nothing.
    pub(crate) async fn spend_within(
        &self,
        source_id: &str,
        usage: SourceUse,
        max_wait: Duration,
    ) -> bool {
        let Some(throttle) = self.throttle(source_id) else {
            return true;
        };
        let (_running, shutdown) = watch::channel(false);
        tokio::time::timeout(max_wait, throttle.acquire(usage, &shutdown))
            .await
            .unwrap_or(false)
    }

    /// Wait up to `max_wait` for source pacing, then check the hourly media budget.
    /// Hold the throttle's window through both checks so a refusal consumes neither budget.
    pub(crate) async fn spend_media_page(&self, source_id: &str, max_wait: Duration) -> bool {
        let Some(throttle) = self.throttle(source_id) else {
            return self.media_pages.check(source_id).is_ok();
        };
        let deadline = Instant::now() + max_wait;
        tokio::time::timeout_at(deadline, async {
            let mut starts = throttle.starts.lock().await;
            loop {
                let pace = throttle.pace();
                let cost = SourceUse::MediaPage.cost(pace);
                let now = Instant::now();
                if now > deadline {
                    return false;
                }
                let mut recent = starts.clone();
                let wait = SourceThrottle::wait(&mut recent, pace, cost, now);
                if wait.is_zero() {
                    if self.media_pages.check(source_id).is_err() {
                        return false;
                    }
                    recent.push_back((now, cost));
                    *starts = recent;
                    return true;
                }
                tokio::time::sleep(wait).await;
            }
        })
        .await
        .unwrap_or(false)
    }
}

/// A rolling request window plus an even gap after each start, proportional to its cost.
/// Scan-only traffic keeps exactly the old `window / scans_per_window` pacing.
/// The async mutex stays held across waits so consumers queue in order. Limit edits
/// re-pace the same window; a restart begins with an empty, immediately usable window.
pub(crate) struct SourceThrottle {
    pace: std::sync::Mutex<Pace>,
    starts: tokio::sync::Mutex<VecDeque<(Instant, u32)>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pace {
    scans_per_window: u32,
    requests_per_scan: u32,
    capacity: u32,
    window: Duration,
}

impl Pace {
    fn of(limit: &SourceRateLimit) -> Self {
        let scans_per_window = limit.scans_per_window();
        Self {
            scans_per_window,
            requests_per_scan: limit.requests_per_scan,
            capacity: scans_per_window * limit.requests_per_scan,
            window: Duration::from_secs(limit.window_secs),
        }
    }

    fn gap(self, cost: u32) -> Duration {
        let nanos = (self.window.as_nanos() * u128::from(cost) / u128::from(self.capacity))
            .min(Duration::MAX.as_nanos());
        Duration::new(
            (nanos / 1_000_000_000) as u64,
            (nanos % 1_000_000_000) as u32,
        )
    }
}

impl SourceThrottle {
    pub(crate) fn new(limit: &SourceRateLimit) -> Self {
        let pace = Pace::of(limit);
        Self {
            pace: std::sync::Mutex::new(pace),
            starts: tokio::sync::Mutex::new(VecDeque::new()),
        }
    }

    fn pace(&self) -> Pace {
        *self.pace.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Re-pace to `limit`, keeping the recent starts. Returns whether anything changed.
    fn set_limit(&self, limit: &SourceRateLimit) -> bool {
        let next = Pace::of(limit);
        let mut pace = self.pace.lock().unwrap_or_else(|e| e.into_inner());
        let changed = *pace != next;
        *pace = next;
        changed
    }

    /// Series scans admitted per window: the scanner's per-tick batch size.
    pub(crate) fn scans_per_window(&self) -> u32 {
        self.pace().scans_per_window
    }

    fn wait(
        starts: &mut VecDeque<(Instant, u32)>,
        pace: Pace,
        cost: u32,
        now: Instant,
    ) -> Duration {
        while starts
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= pace.window)
        {
            starts.pop_front();
        }
        let mut wait = Duration::ZERO;
        let mut total: u64 = starts.iter().map(|(_, cost)| u64::from(*cost)).sum();
        if total + u64::from(cost) > u64::from(pace.capacity) {
            for (at, old_cost) in starts.iter() {
                total -= u64::from(*old_cost);
                // An empty window always admits even a spend larger than its capacity.
                if total == 0 || total + u64::from(cost) <= u64::from(pace.capacity) {
                    wait = pace.window.saturating_sub(now.duration_since(*at));
                    break;
                }
            }
        }
        if let Some((last, last_cost)) = starts.back() {
            wait = wait.max(
                pace.gap(*last_cost)
                    .saturating_sub(now.duration_since(*last)),
            );
        }
        wait
    }

    /// A stop request interrupts both queueing for the mutex and the pacing sleep.
    async fn acquire(&self, usage: SourceUse, shutdown: &watch::Receiver<bool>) -> bool {
        let mut shutdown = shutdown.clone();
        let mut starts = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => return false,
            starts = self.starts.lock() => starts,
        };
        loop {
            if *shutdown.borrow() {
                return false;
            }
            // Read each pass: edits re-pace even a caller already waiting.
            let pace = self.pace();
            let cost = usage.cost(pace);
            let now = Instant::now();
            let wait = Self::wait(&mut starts, pace, cost, now);
            if wait.is_zero() {
                starts.push_back((now, cost));
                return true;
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = wait_for_shutdown(&mut shutdown) => return false,
            }
        }
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if shutdown.wait_for(|stopping| *stopping).await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::merge_source_rate_limits;

    const MEDIA_WAIT: Duration = Duration::from_secs(60);

    /// The throttle paces evenly AND caps the window. 10 requests / 300 s at 2 per scan:
    /// five scans admitted one every 60 s, and the sixth waits for the first to age out of
    /// the window — never a burst, never more than the budget in any 300 s.
    #[tokio::test(start_paused = true)]
    async fn the_source_throttle_spaces_scans_and_caps_the_window() {
        let limit = crate::config::parse_scan_source_rate_limits("s=10/300")
            .pop()
            .unwrap();
        let t = SourceThrottle::new(&limit);
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let t0 = tokio::time::Instant::now();
        let mut at = Vec::new();
        for _ in 0..7 {
            assert!(t.acquire(SourceUse::Scan, &rx).await);
            at.push(tokio::time::Instant::now().duration_since(t0).as_secs());
        }
        assert_eq!(at, vec![0, 60, 120, 180, 240, 300, 360]);
        // Any 300 s window holds at most 5 starts: [0,300) has 5, the 6th lands at 300.
        for w in 0..=360 {
            let n = at.iter().filter(|s| **s >= w && **s < w + 300).count();
            assert!(n <= 5, "window starting at {w}s holds {n} scans");
        }
    }

    /// A stop request must not wait out a pacing sleep, and a refused acquire records nothing.
    #[tokio::test(start_paused = true)]
    async fn the_source_throttle_yields_to_shutdown() {
        let limit = crate::config::parse_scan_source_rate_limits("s=2/600@1")
            .pop()
            .unwrap();
        let t = SourceThrottle::new(&limit);
        let (tx, rx) = tokio::sync::watch::channel(false);
        assert!(
            t.acquire(SourceUse::Scan, &rx).await,
            "first scan is immediate"
        );
        let t0 = tokio::time::Instant::now();
        // The next scan owes a 300 s gap. Signal shutdown: the acquire must return false at
        // once instead of sleeping that gap out.
        tx.send(true).unwrap();
        assert!(!t.acquire(SourceUse::Scan, &rx).await);
        assert!(
            tokio::time::Instant::now().duration_since(t0) < Duration::from_secs(1),
            "shutdown must short-circuit the pacing sleep"
        );
    }

    /// `SCAN_SOURCE_RATE_LIMITS` must be wired through `deploy/docker-compose.yml`, with the
    /// Eris Scans cap as the shipped default — the WAF unban of 2026-09-05 came with an
    /// explicit 10 requests / 5 minutes condition, so the default in the file IS the promise.
    #[test]
    fn the_source_rate_limit_knob_matches_the_compose_wiring() {
        let compose = include_str!("../../../deploy/docker-compose.yml");
        assert!(
            compose.contains(
                "SCAN_SOURCE_RATE_LIMITS: ${SCAN_SOURCE_RATE_LIMITS:-8358420357808595754=10/300}"
            ),
            "deploy/docker-compose.yml must pass SCAN_SOURCE_RATE_LIMITS with the Eris Scans \
             default, or the cap is unreachable in production"
        );
        let v = crate::config::parse_scan_source_rate_limits("8358420357808595754=10/300");
        assert_eq!(v[0].scans_per_window(), 5);
        assert_eq!(
            SourceThrottle::new(&v[0])
                .pace()
                .gap(v[0].requests_per_scan),
            Duration::from_secs(60)
        );
    }

    /// The live map follows the list: a new limit appears, a changed one re-paces the SAME
    /// throttle (its recent starts survive, so an edit is not a fresh window), a dropped
    /// one goes.
    #[tokio::test(start_paused = true)]
    async fn the_live_throttles_follow_the_limit_list() {
        let coord = SourceBudgets::default();
        let list = |raw: &str| {
            merge_source_rate_limits(crate::config::parse_scan_source_rate_limits(raw), vec![])
        };
        assert!(coord.throttle("s").is_none());

        coord.apply_limits(&list("s=10/300"));
        let t = coord.throttle("s").expect("added");
        let (_tx, rx) = tokio::sync::watch::channel(false);
        assert!(
            t.acquire(SourceUse::Scan, &rx).await,
            "first scan is immediate"
        );

        // Halve the rate: same throttle, and the scan just made still counts.
        coord.apply_limits(&list("s=10/600"));
        let same = coord.throttle("s").unwrap();
        assert!(Arc::ptr_eq(&t, &same), "an edit must not reset the window");
        let t0 = tokio::time::Instant::now();
        assert!(same.acquire(SourceUse::Scan, &rx).await);
        assert_eq!(
            tokio::time::Instant::now().duration_since(t0),
            Duration::from_secs(120),
            "the next scan waits the NEW gap from the scan before the edit"
        );

        coord.apply_limits(&[]);
        assert!(coord.throttle("s").is_none());
    }

    /// Lowering a limit below what the window already holds must wait for enough old scans
    /// to age out, not just the oldest one.
    #[tokio::test(start_paused = true)]
    async fn a_lowered_limit_counts_the_scans_already_made() {
        let limit = |raw: &str| {
            crate::config::parse_scan_source_rate_limits(raw)
                .pop()
                .unwrap()
        };
        let t = SourceThrottle::new(&limit("s=8/400")); // 4 scans / 400 s, one per 100 s
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let t0 = tokio::time::Instant::now();
        for _ in 0..4 {
            assert!(t.acquire(SourceUse::Scan, &rx).await); // at 0, 100, 200, 300
        }
        assert!(t.set_limit(&limit("s=4/400"))); // now 2 scans / 400 s
        assert!(!t.set_limit(&limit("s=4/400")), "unchanged");
        assert!(t.acquire(SourceUse::Scan, &rx).await);
        // Two fit per 400 s, so the scans at 0, 100 AND 200 must all be out of the window:
        // 200 + 400. Waiting only for the oldest (400) would make it 4 scans in 400 s.
        assert_eq!(
            tokio::time::Instant::now().duration_since(t0),
            Duration::from_secs(600)
        );
    }

    fn budgets(raw: &str) -> SourceBudgets {
        let budgets = SourceBudgets::default();
        budgets.apply_limits(&merge_source_rate_limits(
            crate::config::parse_scan_source_rate_limits(raw),
            vec![],
        ));
        budgets
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_and_scans_share_one_request_window() {
        let budgets = budgets("s=10/300");
        let (_tx, rx) = watch::channel(false);
        let t0 = Instant::now();
        assert!(budgets.spend("s", SourceUse::Discovery, &rx).await);
        let mut at = Vec::new();
        for _ in 0..5 {
            assert!(budgets.spend("s", SourceUse::Scan, &rx).await);
            at.push(Instant::now().duration_since(t0).as_secs());
        }
        assert_eq!(at, vec![30, 90, 150, 210, 300]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unthrottled_source_never_waits() {
        let budgets = budgets("s=2/600");
        let (tx, rx) = watch::channel(false);
        assert!(budgets.spend("s", SourceUse::Scan, &rx).await);
        let t0 = Instant::now();
        for _ in 0..10 {
            for usage in [
                SourceUse::Scan,
                SourceUse::Discovery,
                SourceUse::Sync,
                SourceUse::Ingest,
                SourceUse::Cover,
            ] {
                assert!(budgets.spend("other", usage, &rx).await);
            }
        }
        assert_eq!(Instant::now(), t0);
        tx.send(true).unwrap();
        assert!(!budgets.spend("other", SourceUse::Scan, &rx).await);
    }

    #[tokio::test(start_paused = true)]
    async fn ingest_budget_is_per_source() {
        let budgets = SourceBudgets::default();
        for _ in 0..600 {
            assert!(budgets.spend_media_page("source", MEDIA_WAIT).await);
        }
        assert!(!budgets.spend_media_page("source", MEDIA_WAIT).await);
        assert!(budgets.spend_media_page("other-source", MEDIA_WAIT).await);
    }

    #[tokio::test(start_paused = true)]
    async fn media_pages_refuse_a_full_throttle_after_the_bound() {
        let budgets = budgets("s=8/400");
        let (_tx, rx) = watch::channel(false);
        for _ in 0..4 {
            assert!(budgets.spend("s", SourceUse::Scan, &rx).await);
        }
        budgets.apply_limits(&merge_source_rate_limits(
            crate::config::parse_scan_source_rate_limits("s=4/400"),
            vec![],
        ));
        // At 500 s the new 200 s gap has elapsed, but two scans still fill the window.
        tokio::time::advance(Duration::from_secs(200)).await;
        let t0 = Instant::now();
        let throttle = budgets.throttle("s").unwrap();
        let starts = throttle.starts.lock().await.clone();
        assert!(!budgets.spend_media_page("s", MEDIA_WAIT).await);
        assert_eq!(Instant::now().duration_since(t0), MEDIA_WAIT);
        assert_eq!(*throttle.starts.lock().await, starts);
        assert!(budgets.media_pages.is_limited("s").is_none());
        assert!(budgets.spend_media_page("s", MEDIA_WAIT).await);
        assert!(
            !budgets.spend_media_page("s", MEDIA_WAIT).await,
            "the gap also applies"
        );
        budgets.apply_limits(&[]);
        for _ in 0..599 {
            assert!(budgets.spend_media_page("s", MEDIA_WAIT).await);
        }
        assert!(!budgets.spend_media_page("s", MEDIA_WAIT).await);
    }

    #[tokio::test(start_paused = true)]
    async fn media_pages_wait_for_pacing_and_hourly_refusal_preserves_the_window() {
        let budgets = budgets("s=10/30@1");
        let t0 = Instant::now();
        assert!(budgets.spend_media_page("s", MEDIA_WAIT).await);
        assert_eq!(Instant::now(), t0);
        assert!(budgets.spend_media_page("s", MEDIA_WAIT).await);
        assert_eq!(Instant::now().duration_since(t0), Duration::from_secs(3));
        for _ in 0..598 {
            assert!(budgets.media_pages.check("s").is_ok());
        }
        let throttle = budgets.throttle("s").unwrap();
        let starts = throttle.starts.lock().await.clone();
        assert!(!budgets.spend_media_page("s", MEDIA_WAIT).await);
        assert_eq!(Instant::now().duration_since(t0), Duration::from_secs(6));
        assert_eq!(*throttle.starts.lock().await, starts);
    }

    #[tokio::test(start_paused = true)]
    async fn separate_app_state_budgets_do_not_share_state() {
        let first = Arc::new(budgets("s=2/600"));
        let second = Arc::new(budgets("s=2/600"));
        let (_tx, rx) = watch::channel(false);
        assert!(first.spend("s", SourceUse::Scan, &rx).await);
        let t0 = Instant::now();
        assert!(second.spend("s", SourceUse::Discovery, &rx).await);
        assert_eq!(Instant::now(), t0);
        for _ in 0..600 {
            assert!(first.spend_media_page("media", MEDIA_WAIT).await);
        }
        assert!(!first.spend_media_page("media", MEDIA_WAIT).await);
        assert!(second.spend_media_page("media", MEDIA_WAIT).await);
    }

    #[tokio::test(start_paused = true)]
    async fn consumers_queue_in_order_and_media_bounds_the_mutex_wait() {
        let budgets = budgets("s=10/300");
        let (_tx, rx) = watch::channel(false);
        assert!(budgets.spend("s", SourceUse::Discovery, &rx).await);
        let first = budgets.spend("s", SourceUse::Scan, &rx);
        let second = budgets.spend("s", SourceUse::Discovery, &rx);
        tokio::pin!(first, second);
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        let t0 = Instant::now();
        assert!(!budgets.spend_media_page("s", MEDIA_WAIT).await);
        assert_eq!(Instant::now().duration_since(t0), MEDIA_WAIT);
        assert!(first.await);
        assert!(futures::poll!(&mut second).is_pending());
        tokio::time::advance(MEDIA_WAIT).await;
        assert!(second.await);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_interrupts_a_pacing_sleep_and_a_queued_caller() {
        let budgets = budgets("s=2/600");
        let (tx, rx) = watch::channel(false);
        assert!(budgets.spend("s", SourceUse::Scan, &rx).await);
        let first = budgets.spend("s", SourceUse::Scan, &rx);
        let second = budgets.spend("s", SourceUse::Discovery, &rx);
        tokio::pin!(first, second);
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        let t0 = Instant::now();
        tx.send(true).unwrap();
        assert!(!second.await);
        assert!(!first.await);
        assert_eq!(Instant::now(), t0);
        assert_eq!(
            budgets
                .throttle("s")
                .unwrap()
                .starts
                .try_lock()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ingest_items_use_the_scan_cost_and_sub_scan_limits_still_admit() {
        let budgets = budgets("s=1/300@3");
        let (_tx, rx) = watch::channel(false);
        let t0 = Instant::now();
        assert!(budgets.spend("s", SourceUse::Ingest, &rx).await);
        assert_eq!(Instant::now(), t0);
        assert!(budgets.spend("s", SourceUse::Scan, &rx).await);
        assert_eq!(Instant::now().duration_since(t0), Duration::from_secs(300));
    }
}
