//! Sharded sliding-window rate limiter.
//!
//! Replaces the old single `Mutex<HashMap>` limiter that lived in `graphql/mod.rs`:
//! that one took the process-global lock twice per `check` (once in `is_limited`,
//! once in `record`) and, past 4096 keys, ran an O(all keys) sweep while holding
//! the lock inside async context. Fine at 78 rps; a convoy at 2,000.
//!
//! Design here:
//! - N = [`SHARD_COUNT`] shards, each its own `Mutex<HashMap<String, Vec<Instant>>>`,
//!   picked by hashing the client key — concurrent checks on different keys contend
//!   only 1/N of the time.
//! - `check` acquires exactly ONE shard lock for the whole check-and-record.
//! - The occasional anti-leak sweep walks only the touched shard (threshold is the
//!   old global 4096 divided across shards, so total resident keys stay bounded the
//!   same way), never the whole table.
//!
//! Public behavior — limits, windows, retry-after values, "reads never insert",
//! "empty-after-prune keys are evicted" — is identical to the old implementation.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Shard count. Power of two, comfortably above the core count of the target
/// deployment; 16 maps of a few hundred keys each stay cheap to sweep.
const SHARD_COUNT: usize = 16;

/// Per-shard opportunistic-sweep threshold. The old limiter swept its single map
/// at 4096 keys; splitting that budget across shards keeps the same global bound
/// on resident keys while making each sweep 1/16th the work.
const SHARD_SWEEP_THRESHOLD: usize = 4096 / SHARD_COUNT;

/// A dependency-light sliding-window rate limiter, keyed by an arbitrary string
/// (e.g. `login:<username>`). Prunes stale timestamps on each check.
pub struct RateLimiter {
    shards: [Mutex<HashMap<String, Vec<Instant>>>; SHARD_COUNT],
    max: u32,
    window: Duration,
    /// Per-instance random SipHash keys for shard selection. `DefaultHasher::new()`
    /// would hash with FIXED zero seeds, letting a client precompute keys (IPs,
    /// usernames) that all land in one shard and concentrate contention there;
    /// `RandomState` seeds per instance, so the mapping is unpredictable outside
    /// the process.
    hasher: std::collections::hash_map::RandomState,
}

impl RateLimiter {
    pub fn new(max: u32, window_secs: u64) -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(HashMap::new())),
            max,
            window: Duration::from_secs(window_secs),
            hasher: std::collections::hash_map::RandomState::new(),
        }
    }

    /// The shard holding `key`'s hits — keyed SipHash (see `hasher`), so keys that
    /// embed client-controlled strings can't be aimed at one shard.
    fn shard(&self, key: &str) -> &Mutex<HashMap<String, Vec<Instant>>> {
        let mut h = std::hash::BuildHasher::build_hasher(&self.hasher);
        key.hash(&mut h);
        &self.shards[(h.finish() as usize) % SHARD_COUNT]
    }

    /// Lock one shard, recovering from a poisoned lock rather than propagating the
    /// panic — a single panic-while-held must not brick login/register for the
    /// process lifetime.
    fn lock_shard<'a>(
        shard: &'a Mutex<HashMap<String, Vec<Instant>>>,
    ) -> std::sync::MutexGuard<'a, HashMap<String, Vec<Instant>>> {
        shard.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Opportunistic sweep of ONE shard: when it is large, drop keys whose window
    /// has fully elapsed so a churn of distinct client IPs can't grow the table
    /// without bound (there is no background reaper).
    fn sweep_if_large(&self, map: &mut HashMap<String, Vec<Instant>>, now: Instant) {
        if map.len() >= SHARD_SWEEP_THRESHOLD {
            map.retain(|_, v| {
                v.retain(|t| now.duration_since(*t) < self.window);
                !v.is_empty()
            });
        }
    }

    /// Retry-after seconds for a pruned, non-empty entry at/over `max`; `None`
    /// if still under it.
    fn limited_after_prune(&self, entry: &[Instant], now: Instant, max: u32) -> Option<u64> {
        if entry.len() as u32 >= max {
            let oldest = entry.first().copied().unwrap_or(now);
            let retry = self.window.saturating_sub(now.duration_since(oldest));
            Some(retry.as_secs().max(1))
        } else {
            None
        }
    }

    /// Read-only: is `key` already at the limit within the window? Returns the
    /// retry-after seconds if so. Prunes stale timestamps but does NOT record a
    /// new hit — callers that want to count an attempt call `record` explicitly.
    pub fn is_limited(&self, key: &str) -> Option<u64> {
        let now = Instant::now();
        let mut map = Self::lock_shard(self.shard(key));
        // Only inspect an existing entry — never insert on a read, or every
        // distinct client IP would leave a permanent key (unbounded RSS growth).
        let entry = map.get_mut(key)?;
        entry.retain(|t| now.duration_since(*t) < self.window);
        if entry.is_empty() {
            map.remove(key);
            return None;
        }
        self.limited_after_prune(entry, now, self.max)
    }

    /// Record one attempt against `key` (prunes stale timestamps first).
    pub fn record(&self, key: &str) {
        let now = Instant::now();
        let mut map = Self::lock_shard(self.shard(key));
        self.sweep_if_large(&mut map, now);
        let entry = map.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        entry.push(now);
    }

    /// Check-and-record: `Err(retry)` if already at the limit, else records a hit.
    /// One shard-lock acquisition for the whole operation.
    pub fn check(&self, key: &str) -> std::result::Result<(), u64> {
        self.check_scaled(key, 1)
    }

    /// [`check`](Self::check) with `factor` times the limit, for a key that several
    /// clients share (one account, signed in to from many addresses).
    pub fn check_scaled(&self, key: &str, factor: u32) -> std::result::Result<(), u64> {
        let max = self.max.saturating_mul(factor);
        let now = Instant::now();
        let mut map = Self::lock_shard(self.shard(key));
        self.sweep_if_large(&mut map, now);
        let entry = map.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        // An empty (fresh or fully-stale) entry is never limited — mirroring the
        // old `is_limited` semantics, which evicted such keys and reported "not
        // limited" before `record` re-inserted them.
        if !entry.is_empty() {
            if let Some(retry) = self.limited_after_prune(entry, now, max) {
                return Err(retry);
            }
        }
        entry.push(now);
        Ok(())
    }

    /// Total resident keys across every shard. Test-only visibility into the
    /// anti-leak invariants (reads never insert; stale keys are evicted).
    #[cfg(test)]
    fn total_keys(&self) -> usize {
        self.shards.iter().map(|s| Self::lock_shard(s).len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The opportunistic sweep must actually fire: fill shards past
    /// [`SHARD_SWEEP_THRESHOLD`] with instantly-stale keys (0s window) and check
    /// the resident-key count collapses instead of growing without bound.
    #[test]
    fn sweep_evicts_stale_keys_once_a_shard_passes_the_threshold() {
        let rl = RateLimiter::new(5, 0);
        // 4x the global threshold so every shard crosses [`SHARD_SWEEP_THRESHOLD`]
        // several times regardless of hash distribution.
        let total = SHARD_SWEEP_THRESHOLD * SHARD_COUNT * 4;
        for i in 0..total {
            rl.record(&format!("sweep-key-{i}"));
        }
        // With a 0s window every recorded hit is stale on the next touch of its
        // shard, so a shard reaching the threshold gets wiped before the insert —
        // no shard can hold more than the threshold. Without the sweep this would
        // be `total` resident keys.
        let cap = SHARD_SWEEP_THRESHOLD * SHARD_COUNT;
        assert!(
            rl.total_keys() <= cap,
            "sweep never fired: {} keys resident of {total} (cap {cap})",
            rl.total_keys()
        );
    }

    #[test]
    fn limiter_blocks_only_after_max_records() {
        let rl = RateLimiter::new(2, 60);
        assert!(rl.is_limited("k").is_none(), "fresh key is not limited");
        rl.record("k");
        assert!(rl.is_limited("k").is_none(), "1 < max still allowed");
        rl.record("k");
        assert!(rl.is_limited("k").is_some(), "2 >= max is limited");
        // a different key has its own budget
        assert!(rl.is_limited("other").is_none());
    }

    #[test]
    fn limiter_is_read_only_until_record() {
        let rl = RateLimiter::new(1, 60);
        // repeated reads never trip the limit on their own
        for _ in 0..5 {
            assert!(rl.is_limited("k").is_none());
        }
        rl.record("k");
        assert!(rl.is_limited("k").is_some());
    }

    #[test]
    fn limiter_does_not_leak_keys() {
        // A read on an unknown key must not insert a map entry (A4) — otherwise
        // every distinct client IP would grow the map without bound.
        let rl = RateLimiter::new(3, 60);
        for i in 0..100 {
            assert!(rl.is_limited(&format!("k{i}")).is_none());
        }
        assert_eq!(rl.total_keys(), 0, "reads must not insert keys");

        // A key whose window has fully elapsed is evicted on the next read.
        let rl0 = RateLimiter::new(3, 0); // zero-length window → immediately stale
        rl0.record("k");
        assert_eq!(rl0.total_keys(), 1);
        assert!(rl0.is_limited("k").is_none());
        assert_eq!(rl0.total_keys(), 0, "a fully-stale key is evicted on read");
    }

    #[test]
    fn check_records_until_limit_then_reports_retry() {
        let rl = RateLimiter::new(3, 60);
        for i in 0..3 {
            assert!(rl.check("k").is_ok(), "hit {i} under the limit succeeds");
        }
        let retry = rl.check("k").expect_err("4th hit within window is limited");
        assert!(
            (1..=60).contains(&retry),
            "retry-after is clamped to [1, window] secs, got {retry}"
        );
        // A limited check does NOT consume budget for a sibling key.
        assert!(rl.check("k2").is_ok());
    }

    #[test]
    fn window_expiry_frees_the_budget() {
        // Zero-length window: every recorded hit is immediately stale, so the
        // limiter can never refuse — expiry really does free the budget.
        let rl = RateLimiter::new(1, 0);
        for _ in 0..5 {
            assert!(rl.check("k").is_ok(), "stale hits must not count");
        }

        // And with a real (1s) window the same key IS refused inside the window,
        // then admitted again once it elapses.
        let rl1 = RateLimiter::new(1, 1);
        assert!(rl1.check("k").is_ok());
        assert!(rl1.check("k").is_err(), "second hit inside the window");
        std::thread::sleep(Duration::from_millis(1100));
        assert!(rl1.check("k").is_ok(), "window elapsed → budget restored");
    }

    #[test]
    fn shard_isolation_keys_have_independent_budgets() {
        // Many keys (spread across every shard by the hash) each get their own
        // budget, and exhausting one never bleeds into another.
        let rl = RateLimiter::new(2, 60);
        for i in 0..200 {
            let key = format!("ip:{i}");
            assert!(rl.check(&key).is_ok());
            assert!(rl.check(&key).is_ok());
            assert!(
                rl.check(&key).is_err(),
                "key {key} exhausted its own budget"
            );
        }
        // All 200 keys are resident (window still open), none were dropped or merged.
        assert_eq!(rl.total_keys(), 200);
        // A fresh key is still admitted after all that.
        assert!(rl.check("fresh").is_ok());
    }

    #[test]
    fn concurrent_checks_admit_exactly_max() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // 8 threads hammer ONE key: exactly `max` checks may succeed. This is the
        // property the old two-lock check() only held by luck of scheduling; the
        // single-lock check makes it atomic.
        let rl = Arc::new(RateLimiter::new(100, 60));
        let ok = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let rl = Arc::clone(&rl);
                let ok = Arc::clone(&ok);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        if rl.check("hot").is_ok() {
                            ok.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            ok.load(Ordering::Relaxed),
            100,
            "400 concurrent checks on one key admit exactly max=100"
        );

        // And threads on DISTINCT keys don't contend for correctness: every
        // per-key budget is honored independently under parallel load.
        let rl = Arc::new(RateLimiter::new(3, 60));
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let rl = Arc::clone(&rl);
                std::thread::spawn(move || {
                    for i in 0..25 {
                        let key = format!("t{t}:k{i}");
                        for _ in 0..3 {
                            assert!(rl.check(&key).is_ok());
                        }
                        assert!(rl.check(&key).is_err());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
