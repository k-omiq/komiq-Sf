//! Refresh tickets, the short-lived chapter position memo and the page fingerprint notes
//! shared by one engine client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hashlink::LinkedHashMap;
use tokio::sync::{Notify, OnceCell, Semaphore};
use tokio::time::Instant;

static EVENTS: AtomicU64 = AtomicU64::new(0);
const MEMO_TTL: Duration = Duration::from_secs(30);
const MEMO_OFF: Duration = Duration::from_secs(60);
const MEMO_CAP: usize = 4096;
/// Distinct chapters looked up in the engine at once; requests for the same chapter share one.
const LOOKUP_CONCURRENCY: usize = 4;
pub(crate) const REFRESH_WAIT: Duration = Duration::from_secs(10);
/// How long a page's fingerprint note can answer a revalidation without the engine: the
/// browser lifetime of a generation page (`image_delivery::GENERATION_PAGE_CACHE`).
const NOTE_TTL: Duration = Duration::from_secs(86_400);
/// Most fingerprint notes kept, the oldest written dropped first. A note is about 200
/// bytes with its map entry, so this bounds them near 13 MB.
const NOTE_CAP: usize = 65_536;

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

/// One engine lookup's answer, event stamp and start time.
pub(crate) type Lookup = (u64, Option<(i64, i32)>, Instant);

/// A page under one numeric epoch: `(epoch, chapter id, page index)`.
pub(crate) type NoteKey = (i64, i64, usize);

/// A generation page's fingerprint as of its last fetch: enough to answer a matching
/// `If-None-Match` with a 304 instead of fetching the bytes again.
#[derive(Clone, Debug)]
pub(crate) struct Note {
    pub manga_id: i64,
    /// The locator event stamp the page was fetched under. A refresh or reset of the
    /// manga's page list after it retires the note.
    pub stamp: u64,
    pub sha256: [u8; 32],
    pub content_type: String,
    at: Instant,
}

#[derive(Default)]
struct SharedLookup {
    cell: Arc<OnceCell<Lookup>>,
    callers: usize,
}

pub(crate) struct PageLocator {
    state: Mutex<State>,
    lookups: Mutex<HashMap<i64, SharedLookup>>,
    lookup_permits: Semaphore,
    /// In write order. Lost on restart, which only costs one fetch per page.
    notes: Mutex<LinkedHashMap<NoteKey, Note>>,
    #[cfg(test)]
    refresh_wait: Option<Duration>,
    #[cfg(test)]
    note_ttl: Option<Duration>,
}

impl Default for PageLocator {
    fn default() -> Self {
        Self {
            state: Default::default(),
            lookups: Default::default(),
            lookup_permits: Semaphore::new(LOOKUP_CONCURRENCY),
            notes: Default::default(),
            #[cfg(test)]
            refresh_wait: None,
            #[cfg(test)]
            note_ttl: None,
        }
    }
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

    #[cfg(test)]
    pub(crate) fn with_note_ttl(self, ttl: Duration) -> Self {
        Self {
            note_ttl: Some(ttl),
            ..self
        }
    }

    fn note_ttl(&self) -> Duration {
        #[cfg(test)]
        if let Some(ttl) = self.note_ttl {
            return ttl;
        }
        NOTE_TTL
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

    /// A chapter's cold lookup, shared by every request that arrives while it runs (a
    /// chapter opens with all its pages at once) and bounded across chapters. Each caller
    /// checks the shared answer against its stamp, so a follower trusts nothing newer than
    /// the leader saw. A failed lookup is not shared: the next waiter sends its own.
    pub(crate) async fn lookup<F, Fut>(&self, chapter_id: i64, fetch: F) -> anyhow::Result<Lookup>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<Option<(i64, i32)>>>,
    {
        let guard = LookupGuard {
            locator: self,
            chapter_id,
            cell: {
                let mut lookups = self.lookups.lock().unwrap_or_else(|e| e.into_inner());
                let lookup = lookups.entry(chapter_id).or_default();
                lookup.callers += 1;
                lookup.cell.clone()
            },
        };
        guard
            .cell
            .get_or_try_init(|| async {
                let started = Instant::now();
                let _permit = self.lookup_permits.acquire().await?;
                let stamp = self.snapshot();
                Ok::<_, anyhow::Error>((stamp, fetch().await?, started))
            })
            .await
            .copied()
    }

    pub(crate) fn unchanged(&self, manga_id: i64, stamp: u64) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.last_clear <= stamp
            && state
                .mangas
                .get(&manga_id)
                .is_none_or(|manga| manga.unchanged(stamp))
    }

    /// A fetch begun during an unknown-outcome window stays uncacheable even if
    /// the window expires before the engine returns its bytes.
    pub(crate) fn cacheable(&self, manga_id: i64, started: Instant) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mangas
            .get(&manga_id)
            .is_none_or(|manga| manga.memo_on(started))
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

    /// A page's note, while it is younger than [`NOTE_TTL`] and its manga's page list has
    /// had no refresh or reset since the note was written.
    pub(crate) fn note(&self, key: NoteKey) -> Option<Note> {
        let note = {
            let mut notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
            let note = notes.get(&key)?.clone();
            if note.at.elapsed() >= self.note_ttl() {
                notes.remove(&key);
                return None;
            }
            note
        };
        self.unchanged(note.manga_id, note.stamp).then_some(note)
    }

    /// Note a page fetched under `stamp`, replacing an older note of the same page. Only
    /// for bytes a shared cache may keep. The oldest notes go past [`NOTE_CAP`], and expired
    /// ones as they reach the front.
    pub(crate) fn note_put(
        &self,
        key: NoteKey,
        manga_id: i64,
        stamp: u64,
        sha256: [u8; 32],
        content_type: String,
    ) {
        let now = Instant::now();
        let ttl = self.note_ttl();
        let mut notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        notes.insert(
            key,
            Note {
                manga_id,
                stamp,
                sha256,
                content_type,
                at: now,
            },
        );
        while notes.len() > NOTE_CAP
            || notes
                .front()
                .is_some_and(|(_, note)| now.duration_since(note.at) >= ttl)
        {
            notes.pop_front();
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

struct LookupGuard<'a> {
    locator: &'a PageLocator,
    chapter_id: i64,
    cell: Arc<OnceCell<Lookup>>,
}

impl Drop for LookupGuard<'_> {
    fn drop(&mut self) {
        // Keep an unfinished lookup until its last caller leaves. Never remove a
        // newer lookup's cell while an old caller still holds its result.
        let mut lookups = self
            .locator
            .lookups
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(current) = lookups
            .get_mut(&self.chapter_id)
            .filter(|current| Arc::ptr_eq(&current.cell, &self.cell))
        {
            current.callers -= 1;
            if current.cell.get().is_some() || current.callers == 0 {
                lookups.remove(&self.chapter_id);
            }
        }
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
    async fn cancelled_lookup_removes_pending_entry() {
        let locator = PageLocator::default();
        let lookup = locator.lookup(8, || async {
            assert_eq!(locator.lookups.lock().unwrap().len(), 1);
            std::future::pending().await
        });
        assert!(tokio::time::timeout_at(locator.deadline(), lookup)
            .await
            .is_err());
        assert!(locator.lookups.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelled_follower_keeps_shared_lookup_for_next_request() {
        let locator = PageLocator::default();
        let calls = AtomicU64::new(0);
        let release = Notify::new();
        let leader = locator.lookup(8, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            release.notified().await;
            Ok(Some((7, 0)))
        });
        tokio::pin!(leader);
        assert!(futures::poll!(&mut leader).is_pending());
        {
            let follower = locator.lookup(8, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some((7, 1)))
            });
            tokio::pin!(follower);
            assert!(futures::poll!(&mut follower).is_pending());
        }
        let third = locator.lookup(8, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some((7, 2)))
        });
        tokio::pin!(third);
        assert!(futures::poll!(&mut third).is_pending());
        release.notify_one();
        let (first, last) = tokio::join!(leader, third);
        let first = first.unwrap();
        assert_eq!(first.1, Some((7, 0)));
        assert_eq!(last.unwrap(), first);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(locator.lookups.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelled_shared_lookup_is_removed_after_last_caller() {
        let locator = PageLocator::default();
        let mut leader = Box::pin(locator.lookup(8, std::future::pending));
        assert!(futures::poll!(&mut leader).is_pending());
        let mut follower = Box::pin(locator.lookup(8, std::future::pending));
        assert!(futures::poll!(&mut follower).is_pending());
        drop(leader);
        assert_eq!(locator.lookups.lock().unwrap().len(), 1);
        drop(follower);
        assert!(locator.lookups.lock().unwrap().is_empty());
    }

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
    async fn unknown_refresh_cache_window_covers_fetches_that_outlive_it() {
        let locator = Arc::new(PageLocator::default());
        let before = Instant::now();
        assert!(locator.cacheable(7, before));
        locator.begin_refresh(7).finish(false);
        let during = Instant::now();
        assert!(!locator.cacheable(7, during));
        assert!(locator.cacheable(99, during));
        let release = Notify::new();
        let mut leader = Box::pin(locator.lookup(8, || async {
            release.notified().await;
            Ok(Some((7, 0)))
        }));
        assert!(futures::poll!(&mut leader).is_pending());
        tokio::time::advance(MEMO_OFF).await;
        assert!(!locator.cacheable(7, during));
        assert!(locator.cacheable(7, Instant::now()));
        let mut follower = Box::pin(locator.lookup(8, || async {
            panic!("the late caller must share the earlier lookup")
        }));
        assert!(futures::poll!(&mut follower).is_pending());
        release.notify_one();
        let (leader, follower) = tokio::join!(leader, follower);
        let follower = follower.unwrap();
        assert_eq!(follower, leader.unwrap());
        assert!(!locator.cacheable(7, follower.2));
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

    #[tokio::test(start_paused = true)]
    async fn notes_expire_drop_oldest_first_and_retire_on_a_page_list_change() {
        let locator = Arc::new(PageLocator::default());
        let note = |chapter: i64, manga: i64| {
            locator.note_put(
                (1, chapter, 0),
                manga,
                locator.snapshot(),
                [chapter as u8; 32],
                "image/png".into(),
            )
        };
        note(1, 7);
        tokio::time::advance(NOTE_TTL - Duration::from_millis(1)).await;
        let kept = locator.note((1, 1, 0)).unwrap();
        assert_eq!((kept.manga_id, kept.sha256), (7, [1; 32]));
        assert!(locator.note((2, 1, 0)).is_none(), "another epoch");
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(locator.note((1, 1, 0)).is_none(), "a day old");

        for chapter in 0..=NOTE_CAP as i64 {
            note(chapter, 7);
        }
        assert_eq!(locator.notes.lock().unwrap().len(), NOTE_CAP);
        assert!(
            locator.note((1, 0, 0)).is_none(),
            "the oldest written goes first"
        );
        // Rewriting a note makes it the newest.
        note(1, 7);
        note(NOTE_CAP as i64 + 1, 7);
        assert!(locator.note((1, 1, 0)).is_some());
        assert!(locator.note((1, 2, 0)).is_none());
        // An expired front goes on the next write, under the cap too.
        tokio::time::advance(NOTE_TTL).await;
        note(9, 8);
        assert_eq!(locator.notes.lock().unwrap().len(), 1);

        // A refresh of the manga's page list retires its notes only; a reset retires all.
        note(10, 7);
        locator.begin_refresh(7).finish(true);
        assert!(locator.note((1, 10, 0)).is_none());
        assert!(locator.note((1, 9, 0)).is_some());
        locator.clear();
        assert!(locator.note((1, 9, 0)).is_none());
    }
}
