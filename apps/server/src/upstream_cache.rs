//! Bounded success-only upstream lookups, with single-flight initialization.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::sync::{Mutex, MutexGuard, OnceCell};

use crate::telemetry::CacheStats;

/// A TTL memo with single-flight initialisation: concurrent callers for the same key
/// share ONE in-flight fetch instead of each spending a request. A slot expires `ttl`
/// after its fetch was *started*. Active callers retain the same slot past expiry;
/// once they finish, the next lookup refreshes it. Eviction never disowns a fill or
/// its waiters. A failed init leaves the slot uninitialised and retryable.
///
/// `tokio::time::Instant` rather than `std`: identical in production, but it obeys
/// `tokio::time::pause`/`advance`, which is what lets the expiry test be exact.
pub(crate) struct TtlMemo<V> {
    ttl: Duration,
    cap: usize,
    slots: std::sync::Mutex<HashMap<String, MemoSlot<V>>>,
    stats: &'static CacheStats,
}

/// When the slot's fetch was started, and the cell its value lands in (shared by
/// every caller that joined while the fetch was in flight).
struct MemoSlot<V> {
    started: tokio::time::Instant,
    touched: tokio::time::Instant,
    cell: Arc<MemoCell<V>>,
}

struct MemoCell<V> {
    value: OnceCell<V>,
    initializing: Mutex<()>,
}

/// Counts a same-key waiter dropped (deadline, disconnect) before it got the lock.
struct WaitingCaller(Option<&'static CacheStats>);

impl Drop for WaitingCaller {
    fn drop(&mut self) {
        if let Some(stats) = self.0 {
            stats.waiter_cancellations(1);
        }
    }
}

impl<V> MemoCell<V> {
    /// The initializer guard, and whether this caller waited behind another one.
    async fn initializer(
        &self,
        stats: &'static CacheStats,
    ) -> Result<(Option<MutexGuard<'_, ()>>, bool)> {
        match self.initializing.try_lock() {
            Ok(guard) => Ok((Some(guard), false)),
            Err(_) => {
                // Initialization may have completed between the caller's fast
                // path and try_lock; that answer needs no waiter admission.
                if self.value.get().is_some() {
                    return Ok((None, false));
                }
                // A cold same-key waiter is scarce too: do not let it fill local
                // HTTP admission while one source request hangs. Its client share
                // is released before it can become the next network initializer.
                let wait = crate::request_budget::cache_wait_permit()
                    .inspect_err(|_| stats.waiter_refusals(1))?;
                let mut waiting = WaitingCaller(Some(stats));
                let guard = self.initializing.lock().await;
                waiting.0 = None;
                if let Some(wait) = wait {
                    wait.obtained();
                }
                Ok((Some(guard), true))
            }
        }
    }
}

impl<V: Clone> TtlMemo<V> {
    pub(crate) fn new(ttl: Duration, cap: usize, stats: &'static CacheStats) -> Self {
        Self {
            ttl,
            cap: cap.max(1),
            slots: std::sync::Mutex::new(HashMap::new()),
            stats,
        }
    }

    /// Inspect only an initialized, unexpired answer; do not allocate a slot for
    /// a miss or let a probe extend its lifetime.
    pub(crate) fn has_value(&self, key: &str) -> bool {
        let now = tokio::time::Instant::now();
        self.slots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .is_some_and(|slot| {
                now.duration_since(slot.started) < self.ttl && slot.cell.value.get().is_some()
            })
    }

    /// The live slot for `key`, replacing an expired one. The lock is never held
    /// across an await, so a slow upstream fetch blocks only its own key's waiters.
    fn slot(&self, key: &str) -> Result<Arc<MemoCell<V>>> {
        let now = tokio::time::Instant::now();
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = slots.get_mut(key) {
            if now.duration_since(slot.started) < self.ttl || Arc::strong_count(&slot.cell) > 1 {
                slot.touched = now;
                return Ok(slot.cell.clone());
            }
        }
        if slots.remove(key).is_some() {
            self.stats.expired(1);
        }
        if slots.len() >= self.cap {
            let ttl = self.ttl;
            let before = slots.len();
            slots.retain(|_, slot| {
                now.duration_since(slot.started) < ttl || Arc::strong_count(&slot.cell) > 1
            });
            self.stats.expired((before - slots.len()) as u64);
            if slots.len() >= self.cap {
                let victim = slots
                    .iter()
                    .filter(|(_, slot)| Arc::strong_count(&slot.cell) == 1)
                    .min_by_key(|(_, slot)| slot.touched)
                    .map(|(key, _)| key.clone());
                match victim {
                    Some(key) => {
                        slots.remove(&key);
                        self.stats.evictions(1);
                    }
                    // All cells have callers: shed this new key rather than exceed
                    // the bound or split one key into two concurrent initializers.
                    None => {
                        self.stats.registry_full(1);
                        self.stats.set_entries(slots.len());
                        return Err(crate::request_budget::UpstreamBusy.into());
                    }
                }
            }
        }
        let cell = Arc::new(MemoCell {
            value: OnceCell::new(),
            initializing: Mutex::new(()),
        });
        slots.insert(
            key.to_string(),
            MemoSlot {
                started: now,
                touched: now,
                cell: cell.clone(),
            },
        );
        self.stats.set_entries(slots.len());
        Ok(cell)
    }

    /// Count how a lookup that reached the initializer was answered.
    fn count_init<T, E>(&self, ran: bool, waited: bool, result: &std::result::Result<T, E>) {
        if ran {
            self.stats.misses(1);
            if result.is_ok() {
                self.stats.fills(1);
            } else {
                self.stats.fill_errors(1);
            }
        } else if waited {
            self.stats.coalesced(1);
        } else {
            self.stats.hits(1);
        }
    }

    pub(crate) async fn get_or_try_init<F, Fut>(&self, key: &str, init: F) -> Result<V>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V>>,
    {
        let cell = self.slot(key)?;
        if let Some(value) = cell.value.get() {
            self.stats.hits(1);
            return Ok(value.clone());
        }
        let (_guard, waited) = cell.initializer(self.stats).await?;
        let mut ran = false;
        let result = cell
            .value
            .get_or_try_init(|| {
                ran = true;
                init()
            })
            .await;
        self.count_init(ran, waited, &result);
        Ok(result?.clone())
    }
    /// Reuse positive lookups without remembering absence. An unknown chapter can
    /// appear in the engine later; neither that absence nor a failed lookup is a
    /// cached answer. Concurrent successful lookups still share one initializer.
    pub(crate) async fn get_or_try_init_optional<F, Fut>(
        &self,
        key: &str,
        init: F,
    ) -> Result<Option<V>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Option<V>>>,
    {
        let cell = self.slot(key)?;
        if let Some(value) = cell.value.get() {
            self.stats.hits(1);
            return Ok(Some(value.clone()));
        }
        let (_guard, waited) = cell.initializer(self.stats).await?;
        let mut ran = false;
        let result = cell
            .value
            .get_or_try_init(|| {
                ran = true;
                async { init().await.map_err(Some)?.ok_or(None) }
            })
            .await;
        // An absent answer is a miss without a fill: absence is never cached.
        match &result {
            Err(None) if ran => self.stats.misses(1),
            _ => self.count_init(ran, waited, &result),
        }
        match result {
            Ok(value) => Ok(Some(value.clone())),
            Err(None) => Ok(None),
            Err(Some(error)) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::sync::atomic::Ordering;

    #[tokio::test(start_paused = true)]
    async fn memo_shares_one_fetch_and_expires() {
        use std::sync::atomic::AtomicUsize;
        // Paused time: `advance` moves the clock exactly, so the TTL edge is a fact
        // of the test, not of how loaded the runner is.
        let ttl = Duration::from_secs(600);
        let stats = CacheStats::leak();
        let memo = TtlMemo::<Vec<String>>::new(ttl, 2, stats);
        assert!(!memo.has_value("missing"));
        assert!(memo.slots.lock().unwrap().is_empty());
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = |calls: Arc<AtomicUsize>| async move {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(vec!["u".to_string()])
        };

        // Concurrent openers of one chapter share a single upstream request.
        let (a, b) = tokio::join!(
            memo.get_or_try_init("ch", || fetch(calls.clone())),
            memo.get_or_try_init("ch", || fetch(calls.clone()))
        );
        assert_eq!(a.unwrap(), ["u"]);
        assert_eq!(b.unwrap(), ["u"]);
        assert!(memo.has_value("ch"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second caller must join the first"
        );
        let seen = stats.snapshot();
        assert_eq!((seen.misses, seen.fills, seen.coalesced), (1, 1, 1));
        assert_eq!((seen.hits, seen.entries), (0, 1));

        // Inside the TTL the answer is served from memory — right up to the edge.
        tokio::time::advance(ttl - Duration::from_millis(11)).await;
        memo.get_or_try_init("ch", || fetch(calls.clone()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(stats.snapshot().hits, 1);

        // At the TTL the base URL may be dead, so the next caller refetches.
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(!memo.has_value("ch"));
        memo.get_or_try_init("ch", || fetch(calls.clone()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let seen = stats.snapshot();
        assert_eq!((seen.expired, seen.misses, seen.fills), (1, 2, 2));

        // A failed init leaves nothing behind: the next caller retries, not errors.
        let failing = memo.get_or_try_init("bad", || async { Err(anyhow!("upstream 5xx")) });
        assert!(failing.await.is_err());
        assert_eq!(stats.snapshot().fill_errors, 1);
        memo.get_or_try_init("bad", || fetch(calls.clone()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Cap 2: a third chapter evicts one unused value, not the whole memo.
        memo.get_or_try_init("third", || fetch(calls.clone()))
            .await
            .unwrap();
        let keys: Vec<String> = memo.slots.lock().unwrap().keys().cloned().collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().any(|key| key == "third"));
        let seen = stats.snapshot();
        assert_eq!(
            (seen.evictions, seen.entries, seen.registry_full),
            (1, 2, 0)
        );
    }

    #[tokio::test]
    async fn optional_memo_never_caches_absence_or_failure() {
        let memo = TtlMemo::<i64>::new(Duration::from_secs(300), 4096, CacheStats::leak());
        assert_eq!(
            memo.get_or_try_init_optional("chapter", || async { Ok(None) })
                .await
                .unwrap(),
            None
        );
        assert!(memo
            .get_or_try_init_optional("chapter", || async { Err(anyhow!("offline")) })
            .await
            .is_err());
        assert_eq!(
            memo.get_or_try_init_optional("chapter", || async { Ok(Some(42)) })
                .await
                .unwrap(),
            Some(42)
        );
        assert_eq!(
            memo.get_or_try_init_optional("chapter", || async {
                panic!("positive owner was not cached")
            })
            .await
            .unwrap(),
            Some(42)
        );
    }

    #[tokio::test]
    async fn cancelled_initializer_leaves_memo_retryable() {
        let memo = Arc::new(TtlMemo::<i64>::new(
            Duration::from_secs(300),
            4096,
            CacheStats::leak(),
        ));
        let started = Arc::new(tokio::sync::Notify::new());
        let task = {
            let memo = memo.clone();
            let started = started.clone();
            tokio::spawn(async move {
                memo.get_or_try_init("chapter", || async {
                    started.notify_one();
                    std::future::pending::<Result<i64>>().await
                })
                .await
            })
        };
        started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            memo.get_or_try_init("chapter", || async { Ok(42) })
                .await
                .unwrap(),
            42
        );
    }

    #[tokio::test]
    async fn telemetry_counts_absent_answers_and_cancelled_waiters() {
        let stats = CacheStats::leak();
        let memo = Arc::new(TtlMemo::<i64>::new(Duration::from_secs(300), 4096, stats));
        assert_eq!(
            memo.get_or_try_init_optional("gone", || async { Ok(None) })
                .await
                .unwrap(),
            None
        );
        let seen = stats.snapshot();
        assert_eq!((seen.misses, seen.fills, seen.fill_errors), (1, 0, 0));
        let started = Arc::new(tokio::sync::Notify::new());
        let leader = {
            let (memo, started) = (memo.clone(), started.clone());
            tokio::spawn(async move {
                memo.get_or_try_init("slow", || async {
                    started.notify_one();
                    std::future::pending::<Result<i64>>().await
                })
                .await
            })
        };
        started.notified().await;
        let waiter = {
            let memo = memo.clone();
            tokio::spawn(async move { memo.get_or_try_init("slow", || async { Ok(1) }).await })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while Arc::strong_count(&memo.slots.lock().unwrap()["slow"].cell) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiter did not join the in-flight fill");
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(stats.snapshot().waiter_cancellations, 1);
        leader.abort();
        let _ = leader.await;
        assert_eq!(
            stats.snapshot().waiter_cancellations,
            1,
            "a leader is no waiter"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn capacity_and_expiry_keep_the_same_active_initializer() {
        use std::sync::atomic::AtomicUsize;
        let memo = Arc::new(TtlMemo::<u32>::new(
            Duration::from_secs(1),
            2,
            CacheStats::leak(),
        ));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let first = {
            let (memo, started, release, calls) = (
                memo.clone(),
                started.clone(),
                release.clone(),
                calls.clone(),
            );
            tokio::spawn(async move {
                memo.get_or_try_init("active", || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    Ok(7)
                })
                .await
                .unwrap()
            })
        };
        started.notified().await;
        memo.get_or_try_init("old", || async { Ok(1) })
            .await
            .unwrap();
        memo.get_or_try_init("new", || async { Ok(2) })
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        let same = memo.slot("active").unwrap();
        let original = memo.slots.lock().unwrap()["active"].cell.clone();
        assert!(Arc::ptr_eq(&same, &original));
        drop(same);
        drop(original);
        let second = {
            let (memo, calls) = (memo.clone(), calls.clone());
            tokio::spawn(async move {
                memo.get_or_try_init("active", || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(99)
                })
                .await
                .unwrap()
            })
        };
        tokio::task::yield_now().await;
        release.notify_one();
        assert_eq!(first.await.unwrap(), 7);
        assert_eq!(second.await.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            memo.get_or_try_init("active", || async { Ok(9) })
                .await
                .unwrap(),
            9
        );
    }

    #[tokio::test]
    async fn full_active_memo_sheds_new_keys_without_splitting_a_fill() {
        let stats = CacheStats::leak();
        let memo = TtlMemo::<u32>::new(Duration::from_secs(60), 1, stats);
        let active = memo.slot("active").unwrap();
        let refused = memo
            .get_or_try_init("other", || async { Ok(1) })
            .await
            .unwrap_err();
        assert!(crate::request_budget::is_upstream_busy(&refused));
        assert_eq!(stats.snapshot().registry_full, 1);
        assert_eq!(stats.snapshot().evictions, 0);
        assert!(Arc::ptr_eq(&active, &memo.slot("active").unwrap()));
        drop(active);
        assert_eq!(
            memo.get_or_try_init("other", || async { Ok(1) })
                .await
                .unwrap(),
            1
        );
    }
    #[tokio::test]
    async fn cold_memo_waiters_share_client_bound_but_warm_hits_bypass_it() {
        let stats = CacheStats::leak();
        let memo = Arc::new(TtlMemo::<i64>::new(Duration::from_secs(300), 4096, stats));
        memo.get_or_try_init("warm", || async { Ok(7) })
            .await
            .unwrap();
        let budgets = Arc::new(crate::request_budget::RequestBudgets::default());
        let started = Arc::new(tokio::sync::Notify::new());
        let first = {
            let memo = memo.clone();
            let budgets = budgets.clone();
            let started = started.clone();
            tokio::spawn(async move {
                budgets
                    .scope_upstream("memo-client".into(), async {
                        memo.get_or_try_init("cold", || async {
                            let _network = crate::request_budget::upstream_permit()?;
                            started.notify_one();
                            std::future::pending::<Result<i64>>().await
                        })
                        .await
                    })
                    .await
            })
        };
        started.notified().await;
        let mut waiters = Vec::new();
        for _ in 1..crate::request_budget::UPSTREAM_CLIENT_CONCURRENCY {
            let memo = memo.clone();
            let budgets = budgets.clone();
            waiters.push(tokio::spawn(async move {
                budgets
                    .scope_upstream("memo-client".into(), async {
                        memo.get_or_try_init("cold", || async {
                            let _network = crate::request_budget::upstream_permit()?;
                            Ok(42)
                        })
                        .await
                    })
                    .await
            }));
        }
        // Wait until the first fetch and its same-client waiters hold the complete
        // client share. A temporary probe is dropped immediately on success.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let full = budgets
                    .scope_upstream("memo-client".into(), async {
                        crate::request_budget::upstream_permit().is_err()
                    })
                    .await;
                if full {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("memo waiters did not take client shares");
        budgets
            .scope_upstream("memo-client".into(), async {
                let refused = tokio::time::timeout(
                    Duration::from_secs(1),
                    memo.get_or_try_init("cold", || async { Ok(99) }),
                )
                .await
                .expect("excess memo waiter queued");
                assert!(crate::request_budget::is_upstream_busy(
                    &refused.unwrap_err()
                ));
                assert_eq!(stats.snapshot().waiter_refusals, 1);
                assert_eq!(
                    memo.get_or_try_init("warm", || async { panic!("warm hit fetched") })
                        .await
                        .unwrap(),
                    7
                );
            })
            .await;
        assert_eq!(budgets.upstream_available_for_test(), 15);
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        for waiter in waiters {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), waiter)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
                42
            );
        }
        assert_eq!(budgets.upstream_available_for_test(), 16);
    }
}
