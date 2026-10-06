//! Bounded success-only upstream lookups, with single-flight initialization.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use tokio::sync::{Mutex, MutexGuard, OnceCell};

/// A TTL memo with single-flight initialisation: concurrent callers for the same key
/// share ONE in-flight fetch instead of each spending a request. A slot expires `ttl`
/// after its fetch was *started*, and the value arrives later than that, so no caller
/// ever sees a value the upstream issued more than `ttl` ago. A failed init leaves the
/// slot uninitialised; waiters and later callers retry one at a time.
///
/// `tokio::time::Instant` rather than `std`: identical in production, but it obeys
/// `tokio::time::pause`/`advance`, which is what lets the expiry test be exact.
pub(crate) struct TtlMemo<V> {
    ttl: Duration,
    cap: usize,
    slots: std::sync::Mutex<HashMap<String, MemoSlot<V>>>,
}

/// When the slot's fetch was started, and the cell its value lands in (shared by
/// every caller that joined while the fetch was in flight).
type MemoSlot<V> = (tokio::time::Instant, Arc<MemoCell<V>>);

struct MemoCell<V> {
    value: OnceCell<V>,
    initializing: Mutex<()>,
}

impl<V> MemoCell<V> {
    async fn initializer(&self) -> Result<Option<MutexGuard<'_, ()>>> {
        match self.initializing.try_lock() {
            Ok(guard) => Ok(Some(guard)),
            Err(_) => {
                // Initialization may have completed between the caller's fast
                // path and try_lock; that answer needs no waiter admission.
                if self.value.get().is_some() {
                    return Ok(None);
                }
                // A cold same-key waiter is scarce too: do not let it fill local
                // HTTP admission while one source request hangs. Its client share
                // is released before it can become the next network initializer.
                let wait = crate::request_budget::cache_wait_permit()?;
                let guard = self.initializing.lock().await;
                drop(wait);
                Ok(Some(guard))
            }
        }
    }
}

impl<V: Clone> TtlMemo<V> {
    pub(crate) fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            slots: std::sync::Mutex::new(HashMap::new()),
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
            .is_some_and(|(started, cell)| {
                now.duration_since(*started) < self.ttl && cell.value.get().is_some()
            })
    }

    /// The live slot for `key`, replacing an expired one. The lock is never held
    /// across an await, so a slow upstream fetch blocks only its own key's waiters.
    fn slot(&self, key: &str) -> Arc<MemoCell<V>> {
        let now = tokio::time::Instant::now();
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((started, cell)) = slots.get(key) {
            if now.duration_since(*started) < self.ttl {
                return cell.clone();
            }
        }
        if slots.len() >= self.cap {
            let ttl = self.ttl;
            slots.retain(|_, (started, _)| now.duration_since(*started) < ttl);
            if slots.len() >= self.cap {
                // Every entry is live and the memo is still full: the working set is
                // larger than the cap. Start over rather than grow without bound — one
                // extra upstream round per chapter is the cost, never wrong data.
                slots.clear();
            }
        }
        let cell = Arc::new(MemoCell {
            value: OnceCell::new(),
            initializing: Mutex::new(()),
        });
        slots.insert(key.to_string(), (now, cell.clone()));
        cell
    }

    pub(crate) async fn get_or_try_init<F, Fut>(&self, key: &str, init: F) -> Result<V>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V>>,
    {
        let cell = self.slot(key);
        if let Some(value) = cell.value.get() {
            return Ok(value.clone());
        }
        let _guard = cell.initializer().await?;
        Ok(cell.value.get_or_try_init(init).await?.clone())
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
        let cell = self.slot(key);
        if let Some(value) = cell.value.get() {
            return Ok(Some(value.clone()));
        }
        let _guard = cell.initializer().await?;
        match cell
            .value
            .get_or_try_init(|| async { init().await.map_err(Some)?.ok_or(None) })
            .await
        {
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
        let memo = TtlMemo::<Vec<String>>::new(ttl, 2);
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

        // Inside the TTL the answer is served from memory — right up to the edge.
        tokio::time::advance(ttl - Duration::from_millis(11)).await;
        memo.get_or_try_init("ch", || fetch(calls.clone()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // At the TTL the base URL may be dead, so the next caller refetches.
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(!memo.has_value("ch"));
        memo.get_or_try_init("ch", || fetch(calls.clone()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // A failed init leaves nothing behind: the next caller retries, not errors.
        let failing = memo.get_or_try_init("bad", || async { Err(anyhow!("upstream 5xx")) });
        assert!(failing.await.is_err());
        memo.get_or_try_init("bad", || fetch(calls.clone()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Cap 2, both slots live, a third chapter arrives: the memo resets to hold
        // only the newcomer rather than grow past its cap.
        memo.get_or_try_init("third", || fetch(calls.clone()))
            .await
            .unwrap();
        let keys: Vec<String> = memo.slots.lock().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["third"]);
    }

    #[tokio::test]
    async fn optional_memo_never_caches_absence_or_failure() {
        let memo = TtlMemo::<i64>::new(Duration::from_secs(300), 4096);
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
        let memo = Arc::new(TtlMemo::<i64>::new(Duration::from_secs(300), 4096));
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
    async fn cold_memo_waiters_share_client_bound_but_warm_hits_bypass_it() {
        let memo = Arc::new(TtlMemo::<i64>::new(Duration::from_secs(300), 4096));
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
