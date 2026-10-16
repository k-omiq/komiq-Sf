//! Refresh tickets and the short-lived chapter position memo shared by one engine client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

static EVENTS: AtomicU64 = AtomicU64::new(0);
const MEMO_TTL: Duration = Duration::from_secs(30);
const MEMO_OFF: Duration = Duration::from_secs(60);
const MEMO_CAP: usize = 4096;
pub(crate) const REFRESH_WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    pub manga_id: i64,
    pub source_order: i32,
    pub stamp: u64,
    at: Instant,
}

#[derive(Default)]
struct Manga {
    active: u32,
    last_event: u64,
    memo_off_until: Option<Instant>,
    idle: Arc<Notify>,
}

impl Manga {
    fn unchanged(&self, stamp: u64) -> bool {
        self.active == 0 && self.last_event <= stamp
    }

    fn memo_on(&self, now: Instant) -> bool {
        self.memo_off_until.is_none_or(|until| now >= until)
    }
}

#[derive(Default)]
struct State {
    // Retained for the process lifetime: removing a recent event could hide a refresh
    // from a page fetch already in flight. These entries are small.
    mangas: HashMap<i64, Manga>,
    chapters: HashMap<i64, Entry>,
    last_clear: u64,
}

#[derive(Default)]
pub(crate) struct PageLocator {
    state: Mutex<State>,
    #[cfg(test)]
    refresh_wait: Option<Duration>,
}

#[derive(Debug)]
pub(crate) struct Busy;

impl PageLocator {
    #[cfg(test)]
    pub(crate) fn with_refresh_wait(wait: Duration) -> Self {
        Self {
            refresh_wait: Some(wait),
            ..Self::default()
        }
    }

    pub(crate) fn deadline(&self) -> Instant {
        #[cfg(test)]
        let wait = self.refresh_wait.unwrap_or(REFRESH_WAIT);
        #[cfg(not(test))]
        let wait = REFRESH_WAIT;
        Instant::now() + wait
    }

    pub(crate) fn begin_refresh(self: &Arc<Self>, manga_id: i64) -> RefreshGuard {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let manga = state.mangas.entry(manga_id).or_default();
        manga.active += 1;
        manga.last_event = EVENTS.fetch_add(1, Ordering::SeqCst) + 1;
        state.chapters.retain(|_, entry| entry.manga_id != manga_id);
        RefreshGuard {
            locator: self.clone(),
            manga_id,
            finished: false,
        }
    }

    fn end_refresh(&self, manga_id: i64, known: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let manga = state.mangas.get_mut(&manga_id).unwrap();
        manga.active -= 1;
        manga.last_event = EVENTS.fetch_add(1, Ordering::SeqCst) + 1;
        if !known {
            manga.memo_off_until = Some(Instant::now() + MEMO_OFF);
        }
        if manga.active == 0 {
            manga.idle.notify_waiters();
        }
        state.chapters.retain(|_, entry| entry.manga_id != manga_id);
    }

    pub(crate) async fn wait_idle(&self, manga_id: i64, deadline: Instant) -> Result<(), Busy> {
        loop {
            let idle = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if Instant::now() >= deadline {
                    return Err(Busy);
                }
                match state.mangas.get(&manga_id) {
                    Some(manga) if manga.active > 0 => {
                        // Register under the state lock so an end cannot lose this wakeup.
                        manga.idle.clone().notified_owned()
                    }
                    _ => return Ok(()),
                }
            };
            tokio::time::timeout_at(deadline, idle)
                .await
                .map_err(|_| Busy)?;
        }
    }

    pub(crate) fn snapshot(&self) -> u64 {
        EVENTS.load(Ordering::SeqCst)
    }

    pub(crate) fn unchanged(&self, manga_id: i64, stamp: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.last_clear <= stamp
            && state
                .mangas
                .get(&manga_id)
                .is_none_or(|manga| manga.unchanged(stamp))
    }

    pub(crate) fn memo_manga(&self, chapter_id: i64) -> Option<i64> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chapters
            .get(&chapter_id)
            .map(|entry| entry.manga_id)
    }

    pub(crate) fn memo_get(&self, chapter_id: i64) -> Option<Entry> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state.chapters.get(&chapter_id)?;
        let now = Instant::now();
        if state.last_clear > entry.stamp
            || now.duration_since(entry.at) >= MEMO_TTL
            || state
                .mangas
                .get(&entry.manga_id)
                .is_some_and(|manga| !manga.memo_on(now) || !manga.unchanged(entry.stamp))
        {
            return None;
        }
        Some(*entry)
    }

    pub(crate) fn memo_put(&self, chapter_id: i64, manga_id: i64, order: i32, stamp: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if state.last_clear > stamp
            || state
                .mangas
                .get(&manga_id)
                .is_some_and(|manga| !manga.memo_on(now) || !manga.unchanged(stamp))
        {
            return;
        }
        state.chapters.insert(
            chapter_id,
            Entry {
                manga_id,
                source_order: order,
                stamp,
                at: now,
            },
        );
        if state.chapters.len() > MEMO_CAP {
            state
                .chapters
                .retain(|_, entry| now.duration_since(entry.at) < MEMO_TTL);
            if state.chapters.len() > MEMO_CAP {
                if let Some(oldest) = state
                    .chapters
                    .iter()
                    .min_by_key(|(_, entry)| entry.at)
                    .map(|(id, _)| *id)
                {
                    state.chapters.remove(&oldest);
                }
            }
        }
    }

    pub(crate) fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let stamp = EVENTS.fetch_add(1, Ordering::SeqCst) + 1;
        // Fence resets also invalidate lookups for mangas not yet in the registry.
        state.last_clear = stamp;
        for manga in state.mangas.values_mut() {
            manga.last_event = stamp;
        }
        state.chapters.clear();
    }
}

pub(crate) struct RefreshGuard {
    locator: Arc<PageLocator>,
    manga_id: i64,
    finished: bool,
}

impl RefreshGuard {
    pub(crate) fn finish(mut self, known: bool) {
        self.locator.end_refresh(self.manga_id, known);
        self.finished = true;
    }
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.locator.end_refresh(self.manga_id, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn memo_expires_and_keeps_stale_owner_for_waiting() {
        let locator = PageLocator::default();
        locator.memo_put(8, 7, 0, locator.snapshot());
        assert!(locator.memo_get(8).is_some());
        tokio::time::advance(MEMO_TTL).await;
        assert!(locator.memo_get(8).is_none());
        assert_eq!(locator.memo_manga(8), Some(7));
    }

    #[tokio::test(start_paused = true)]
    async fn overlapping_refreshes_invalidate_tickets_and_wake_all_waiters() {
        let locator = Arc::new(PageLocator::default());
        let stamp = locator.snapshot();
        locator.memo_put(8, 7, 0, stamp);
        let first = locator.begin_refresh(7);
        let second = locator.begin_refresh(7);
        assert!(locator.memo_get(8).is_none());
        assert!(!locator.unchanged(7, stamp));
        assert!(locator.unchanged(99, stamp));
        let wait = || {
            let locator = locator.clone();
            tokio::spawn(async move { locator.wait_idle(7, locator.deadline()).await })
        };
        let one = wait();
        let two = wait();
        tokio::task::yield_now().await;
        first.finish(true);
        tokio::task::yield_now().await;
        assert!(!one.is_finished() && !two.is_finished());
        let during = locator.snapshot();
        second.finish(true);
        assert!(!locator.unchanged(7, during));
        one.await.unwrap().unwrap();
        two.await.unwrap().unwrap();
        locator.memo_put(8, 7, 0, during);
        assert!(locator.memo_get(8).is_none());
        locator.memo_put(8, 7, 1, locator.snapshot());
        assert_eq!(locator.memo_get(8).unwrap().source_order, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_and_cancelled_refreshes_disable_memo_for_sixty_seconds() {
        for cancelled in [false, true] {
            let locator = Arc::new(PageLocator::default());
            let guard = locator.begin_refresh(7);
            if cancelled {
                drop(guard);
            } else {
                guard.finish(false);
            }
            locator.memo_put(8, 7, 0, locator.snapshot());
            assert!(locator.memo_get(8).is_none());
            tokio::time::advance(MEMO_OFF - Duration::from_millis(1)).await;
            // A later known result must not erase the unknown-outcome window.
            locator.begin_refresh(7).finish(true);
            locator.memo_put(8, 7, 0, locator.snapshot());
            assert!(locator.memo_get(8).is_none());
            tokio::time::advance(Duration::from_millis(1)).await;
            locator.memo_put(8, 7, 0, locator.snapshot());
            assert!(locator.memo_get(8).is_some());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_wait_is_bounded_and_past_deadlines_are_busy() {
        let locator = Arc::new(PageLocator::default());
        let guard = locator.begin_refresh(7);
        assert!(locator.wait_idle(7, locator.deadline()).await.is_err());
        guard.finish(true);
        assert!(locator.wait_idle(7, Instant::now()).await.is_err());
        locator.wait_idle(7, locator.deadline()).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn memo_prunes_expired_then_oldest_and_clear_invalidates_tickets() {
        let locator = PageLocator::default();
        locator.memo_put(0, 7, 0, locator.snapshot());
        tokio::time::advance(MEMO_TTL).await;
        for id in 1..=MEMO_CAP as i64 {
            locator.memo_put(id, 7, 0, locator.snapshot());
        }
        assert_eq!(locator.state.lock().unwrap().chapters.len(), MEMO_CAP);
        assert_eq!(locator.memo_manga(0), None);
        tokio::time::advance(Duration::from_millis(1)).await;
        locator.memo_put(MEMO_CAP as i64 + 1, 7, 0, locator.snapshot());
        assert_eq!(locator.state.lock().unwrap().chapters.len(), MEMO_CAP);
        assert!(locator.memo_get(MEMO_CAP as i64 + 1).is_some());
        let locator = Arc::new(locator);
        locator.begin_refresh(7).finish(true);
        let stamp = locator.snapshot();
        locator.clear();
        assert!(!locator.unchanged(7, stamp));
        assert!(!locator.unchanged(99, stamp));
        locator.memo_put(9999, 99, 0, stamp);
        assert!(locator.memo_get(9999).is_none());
        assert!(locator.memo_get(1).is_none());
    }
}
