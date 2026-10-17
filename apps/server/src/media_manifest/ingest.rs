use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use super::store::{self, CollectedPage};
use crate::graphql::AppState;
use crate::telemetry::{Lane, ADMISSION};

const QUEUE_CAP: usize = 1024;
const COOLDOWN: Duration = Duration::from_secs(3600);
/// A pass runs this long after the open that queued it, so it follows the reader's own
/// page fetches and the engine answers it from its page cache (S4 spike).
const DELAY: Duration = Duration::from_secs(90);

struct Entry {
    not_before: Instant,
    running: bool,
    again: bool,
}

#[derive(Default)]
struct QueueState {
    entries: HashMap<i64, Entry>,
    finished: HashMap<i64, Instant>,
    last_prune: Option<Instant>,
}

pub(crate) struct IngestQueue {
    state: Mutex<QueueState>,
    wake: Notify,
}

impl Default for IngestQueue {
    fn default() -> Self {
        Self {
            state: Mutex::new(QueueState::default()),
            wake: Notify::new(),
        }
    }
}

impl IngestQueue {
    pub(crate) fn queue(&self, chapter_id: i64) {
        self.enqueue(chapter_id, false, false);
    }

    pub(crate) fn queue_now(&self, chapter_id: i64) {
        self.enqueue(chapter_id, true, true);
    }

    /// The reader got no exact paths although the chapter has a copy (its page list
    /// changed, or its latest revision is stale): ingest again after the usual delay,
    /// even inside the cooldown.
    pub(crate) fn requeue(&self, chapter_id: i64) {
        self.enqueue(chapter_id, false, true);
    }

    fn enqueue(&self, chapter_id: i64, immediate: bool, skip_cooldown: bool) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.get_mut(&chapter_id) {
            if immediate {
                entry.not_before = now;
                entry.again = entry.running;
                self.wake.notify_one();
            } else if skip_cooldown && entry.running {
                // The running pass may have read the old page list: run once more.
                entry.not_before = now + DELAY;
                entry.again = true;
            }
            return;
        }
        if !skip_cooldown
            && state
                .finished
                .get(&chapter_id)
                .is_some_and(|at| now.duration_since(*at) < COOLDOWN)
        {
            return;
        }
        state.finished.remove(&chapter_id);
        if state.entries.len() >= QUEUE_CAP {
            ADMISSION.lane(Lane::Ingest).refuse();
            return;
        }
        state.entries.insert(
            chapter_id,
            Entry {
                not_before: now + if immediate { Duration::ZERO } else { DELAY },
                running: false,
                again: false,
            },
        );
        self.wake.notify_one();
    }

    fn prune_due(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.last_prune.is_some_and(|at| at.elapsed() < COOLDOWN) {
            return false;
        }
        state.last_prune = Some(Instant::now());
        state.finished.retain(|_, at| at.elapsed() < COOLDOWN);
        true
    }

    fn take_ready(&self) -> Option<i64> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (chapter, entry) = state
            .entries
            .iter_mut()
            .filter(|(_, e)| !e.running && e.not_before <= Instant::now())
            .min_by_key(|(_, e)| e.not_before)?;
        entry.running = true;
        Some(*chapter)
    }

    fn finish(&self, chapter: i64, completed: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.get_mut(&chapter) {
            if entry.again {
                entry.running = false;
                entry.again = false;
                return;
            }
        }
        state.entries.remove(&chapter);
        // Abandoned passes have no result; a later open may retry them.
        if completed {
            state.finished.insert(chapter, Instant::now());
        } else {
            state.finished.remove(&chapter);
        }
    }
}

pub(crate) fn spawn(app: Arc<AppState>, shutdown: watch::Receiver<bool>) {
    if app.media_manifests.is_none() {
        return;
    }
    tokio::spawn(crate::task::supervise(
        "media-ingest",
        Duration::from_secs(30),
        shutdown.clone(),
        move || {
            crate::db::Database::job_scope(
                crate::database::ParallelJob::MediaIngest,
                run_loop(app.clone(), shutdown.clone()),
            )
        },
    ));
}

async fn run_loop(app: Arc<AppState>, mut shutdown: watch::Receiver<bool>) {
    let queue = &app.media_manifests.as_ref().unwrap().queue;
    // Supervision may restart after a worker panic; its unfinished entries retry.
    for entry in queue
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .values_mut()
    {
        entry.running = false;
    }
    let mut jobs = tokio::task::JoinSet::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if *shutdown.borrow() {
            return;
        }
        while jobs.len() < 2 {
            let Some(chapter) = queue.take_ready() else {
                break;
            };
            let app = app.clone();
            // Spawned tasks do not inherit the loop's lane: name it again.
            jobs.spawn(crate::db::Database::job_scope(
                crate::database::ParallelJob::MediaIngest,
                async move {
                    let result = ingest_chapter(&app, chapter).await;
                    if let Err(error) = &result {
                        tracing::debug!(chapter, error = %error, "media ingest abandoned");
                    }
                    app.media_manifests
                        .as_ref()
                        .unwrap()
                        .queue
                        .finish(chapter, result.is_ok());
                },
            ));
        }
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = queue.wake.notified() => {},
            result = jobs.join_next(), if !jobs.is_empty() => {
                if result.is_some_and(|r| r.is_err()) { panic!("media ingest worker panicked"); }
            },
            _ = ticker.tick() => {
                if queue.prune_due() {
                    tokio::select! {
                        _ = shutdown.changed() => return,
                        result = crate::db::Database::job_scope(
                            crate::database::ParallelJob::MediaPrune,
                            store::prune(&app.pool),
                        ) => {
                            if let Err(error) = result {
                                tracing::warn!(error = %error, "media manifest prune failed");
                            }
                        }
                    }
                }
            }
        }
    }
}

pub(crate) async fn ingest_chapter(app: &AppState, chapter: i64) -> Result<Option<i32>> {
    let Some(_) = &app.media_manifests else {
        return Ok(None);
    };
    let engine = &app.suwayomi;
    engine.wait_for_generation().await?;
    let generation = engine.read_generation();
    let Some((identity, bound_manga)) = store::identity_for_chapter(&app.pool, chapter).await?
    else {
        bail!("no active copy identity");
    };
    let policy = app.policy.current(&app.pool).await?;
    if policy.denies_pages(bound_manga) || policy.denies_chapter(chapter) {
        bail!("series blocked");
    }
    let stamp = engine.page_locator.snapshot();
    let Some(live) = engine.chapter_origin(chapter).await? else {
        bail!("chapter missing");
    };
    // The pages are filed under the stored source keys, so the engine must still answer
    // this chapter id with exactly those keys: a renumbered engine that no sample caught
    // would otherwise sign another chapter's pages under this copy (the write guard).
    if live.manga_id != bound_manga
        || live.source_id != identity.source_id
        || live.manga_url != identity.manga_key
        || live.chapter_url != identity.chapter_key
    {
        return crate::engine_generation::observe_write(
            &app.pool,
            Err(crate::engine_generation::FenceError::BindingMismatch {
                manga_id: live.manga_id,
                chapter_id: Some(chapter),
            }
            .into()),
        );
    }
    let (manga, order) = (live.manga_id, live.source_order);
    if !engine.page_locator.unchanged(manga, stamp) {
        bail!("chapter moved");
    }
    let count = engine.pages(chapter).await?.len();
    let mut pages = Vec::with_capacity(count);
    let mut total_bytes = 0u64;
    let lane = ADMISSION.lane(Lane::Ingest);
    for idx in 0..count {
        let deadline = Instant::now() + Duration::from_secs(60);
        while engine.page_permits_available() < 2 {
            if Instant::now() >= deadline {
                lane.refuse();
                bail!("page pool busy");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if !app
            .source_budgets
            .spend_media_page(&identity.source_id, Duration::from_secs(60))
            .await
        {
            lane.refuse();
            bail!("ingest source budget exhausted");
        }
        let path = format!("/api/v1/manga/{manga}/chapter/{order}/page/{idx}");
        let (bytes, mime) = match engine.fetch_page_now(&path).await {
            Ok(page) => page,
            Err(crate::suwayomi::PageFetchError::Busy) => {
                lane.refuse();
                bail!("page pool busy");
            }
            Err(crate::suwayomi::PageFetchError::Upstream(error)) => return Err(error),
        };
        lane.admitted();
        let dimensions = image::ImageReader::new(Cursor::new(bytes.as_ref()))
            .with_guessed_format()
            .ok()
            .and_then(|reader| reader.into_dimensions().ok());
        total_bytes += bytes.len() as u64;
        pages.push(CollectedPage {
            sha256: Sha256::digest(&bytes).to_vec(),
            length: bytes.len() as i64,
            mime,
            width: dimensions.and_then(|(w, _)| i32::try_from(w).ok()),
            height: dimensions.and_then(|(_, h)| i32::try_from(h).ok()),
        });
    }
    if !engine.page_locator.unchanged(manga, stamp) {
        bail!("chapter refreshed during ingest");
    }
    // A refresh the server did not start (the engine's own updater) moves no stamp:
    // the chapter must still be the same one at the same position after the pass.
    let Some(after) = engine.chapter_origin(chapter).await? else {
        bail!("chapter missing after ingest");
    };
    if after.manga_id != live.manga_id
        || after.source_id != live.source_id
        || after.manga_url != live.manga_url
        || after.chapter_url != live.chapter_url
    {
        return crate::engine_generation::observe_write(
            &app.pool,
            Err(crate::engine_generation::FenceError::BindingMismatch {
                manga_id: after.manga_id,
                chapter_id: Some(chapter),
            }
            .into()),
        );
    }
    if after.source_order != order {
        bail!("chapter moved during ingest");
    }
    engine.validate_read_generation(generation).await?;
    let (copy, revision) = store::publish(&app.pool, generation.0, &identity, &pages).await?;
    tracing::info!(chapter, copy_id = %copy, revision = %revision.map_or_else(|| "unchanged".into(), |r| r.to_string()), pages = count, bytes = total_bytes, "media ingest finished");
    Ok(revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn ingest_queue_delay_dedup_cooldown_and_stale_bypass() {
        let queue = IngestQueue::default();
        queue.queue(8);
        queue.queue(8);
        assert_eq!(queue.state.lock().unwrap().entries.len(), 1);
        assert_eq!(queue.take_ready(), None);
        tokio::time::advance(Duration::from_secs(90)).await;
        assert_eq!(queue.take_ready(), Some(8));
        assert_eq!(queue.take_ready(), None);
        queue.finish(8, true);
        queue.queue(8);
        assert_eq!(queue.take_ready(), None);
        queue.queue_now(8);
        assert_eq!(queue.take_ready(), Some(8));
        // A mismatch arriving during a pass schedules another immediate attempt.
        queue.queue_now(8);
        queue.finish(8, true);
        assert_eq!(queue.take_ready(), Some(8));
        queue.finish(8, false);
        queue.queue(8);
        tokio::time::advance(Duration::from_secs(90)).await;
        assert_eq!(queue.take_ready(), Some(8));
        queue.finish(8, true);
        tokio::time::advance(COOLDOWN).await;
        queue.queue(8);
        tokio::time::advance(Duration::from_secs(90)).await;
        assert_eq!(queue.take_ready(), Some(8));
    }

    #[tokio::test(start_paused = true)]
    async fn ingest_requeue_skips_cooldown_but_keeps_delay() {
        let queue = IngestQueue::default();
        queue.queue(8);
        tokio::time::advance(Duration::from_secs(90)).await;
        assert_eq!(queue.take_ready(), Some(8));
        queue.finish(8, true);
        queue.queue(8);
        assert!(queue.state.lock().unwrap().entries.is_empty());
        queue.requeue(8);
        assert_eq!(queue.take_ready(), None);
        tokio::time::advance(Duration::from_secs(90)).await;
        assert_eq!(queue.take_ready(), Some(8));
        // A page-list change seen while that pass runs schedules one more, delayed.
        queue.requeue(8);
        queue.finish(8, true);
        assert_eq!(queue.take_ready(), None);
        tokio::time::advance(Duration::from_secs(90)).await;
        assert_eq!(queue.take_ready(), Some(8));
    }

    #[tokio::test(start_paused = true)]
    async fn ingest_queue_is_bounded_and_abandoned_pass_can_retry() {
        let queue = IngestQueue::default();
        for chapter in 0..=QUEUE_CAP {
            queue.queue(chapter as i64);
        }
        assert_eq!(queue.state.lock().unwrap().entries.len(), QUEUE_CAP);
        assert!(!queue
            .state
            .lock()
            .unwrap()
            .entries
            .contains_key(&(QUEUE_CAP as i64)));
        queue.queue_now(8);
        assert_eq!(queue.take_ready(), Some(8));
        queue.finish(8, false);
        queue.queue(8);
        assert!(queue.state.lock().unwrap().entries.contains_key(&8));
    }
}
