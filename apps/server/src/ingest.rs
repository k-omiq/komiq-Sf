//! "Add all from source" bulk-ingest jobs (EXT-4 / S1).
//!
//! A background tokio task that walks one Suwayomi source's POPULAR listing page
//! by page and runs every entry through the same Tier-2 add flow as
//! `bulkAddSourceSeries` (ensure-in-library → cover pHash → dedup core). A source
//! catalogue can be tens of thousands of entries, so this is a persisted job, not
//! a mutation: progress/state live in `source_ingest_job` and survive restarts
//! (a job that was `running` when the process died is marked failed at startup).
//!
//! Control plane is the DB row itself: `cancelSourceIngest` flips the row to
//! `cancelled` and the runner observes it between items — restart-safe with no
//! in-memory channel to lose. One running job per source is enforced by a partial
//! unique index (`uq_source_ingest_running`), so a concurrent second start loses
//! the INSERT race cleanly.
//!
//! Mirrors the `scanner`/`mangadex::spawn_recurring` background-task conventions:
//! spawned detached, every per-item error is recorded and skipped, and the task
//! never panics the process.

use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use sqlx::SqlitePool;

use crate::graphql::AppState;
use crate::suwayomi::FetchType;

/// Delay between browse pages — polite throttle toward the upstream source
/// (every page is one upstream fetch through the engine).
const PAGE_DELAY_MS: u64 = 750;
/// Max items processed concurrently within a page (S3). Each item is a detail
/// fetch + library write + cover download through the engine, so a bounded fan-out
/// (not unbounded) keeps the engine/source from being hammered while still cutting
/// the per-page wall time by ~this factor vs the old sequential loop.
const ITEM_CONCURRENCY: usize = 6;
/// Flush progress to the job row every this many completed items (S3), so the
/// admin sees live within-page progress instead of only per-page jumps.
const PROGRESS_FLUSH_EVERY: i64 = 5;
/// Hard ceiling on pages walked, so a source with a pathological/looping
/// pagination can't run a job forever (20k+ items at 20/page).
const MAX_PAGES: i64 = 1_000;

/// Job states. `running` is the only live state; the rest are terminal.
pub const STATE_RUNNING: &str = "running";
pub const STATE_COMPLETED: &str = "completed";
pub const STATE_CANCELLED: &str = "cancelled";
pub const STATE_FAILED: &str = "failed";

/// One persisted ingest job row (mirrors `source_ingest_job`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct IngestJob {
    pub id: String,
    pub source_id: String,
    pub state: String,
    pub pages_done: i64,
    pub items_seen: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub new_works: i64,
    pub auto_merged: i64,
    pub queued_for_review: i64,
    pub already_existing: i64,
    pub error: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

/// In-memory progress counters, flushed to the row after every page (and at the
/// end). Kept separate from the row so the hot loop doesn't re-read the DB.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub pages_done: i64,
    pub items_seen: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub new_works: i64,
    pub auto_merged: i64,
    pub queued_for_review: i64,
    pub already_existing: i64,
}

impl Progress {
    /// Fold one item's dedup decision into the counters.
    pub fn record_decision(&mut self, decision: &str) {
        self.succeeded += 1;
        match decision {
            "new" => self.new_works += 1,
            // Exact MangaDex-UUID consolidation is an auto-merge onto the canonical
            // work — bucket it with auto_merge so category totals reconcile with
            // `succeeded` (else UUID consolidations vanish from the ingest summary).
            "auto_merge" | "mangadex_id" => self.auto_merged += 1,
            // Both the bare review path and the consolidated-review path (a candidate
            // corroborated onto an existing work but held for a human) land in the
            // review bucket, so the category totals reconcile with `succeeded`.
            "review" | "review_consolidated" => self.queued_for_review += 1,
            "existing" => self.already_existing += 1,
            _ => {}
        }
    }

    pub fn record_failure(&mut self) {
        self.failed += 1;
    }
}

/// Try to create a `running` job for a source. `None` when another job is
/// already running for it — the partial unique index makes this race-free (the
/// losing concurrent INSERT violates `uq_source_ingest_running`).
pub async fn try_start_job(pool: &SqlitePool, source_id: &str) -> Result<Option<IngestJob>> {
    let id = format!("ing_{}", uuid::Uuid::new_v4().simple());
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "INSERT INTO source_ingest_job (id, source_id, state, started_at) VALUES (?, ?, 'running', ?)",
    )
    .bind(&id)
    .bind(source_id)
    .bind(&now)
    .execute(pool)
    .await;
    match res {
        Ok(_) => Ok(load_job(pool, &id).await?),
        // X3: detect the lost one-running-job-per-source race by the SQLite
        // extended result code (SQLITE_CONSTRAINT_UNIQUE = 2067), not a brittle
        // message-substring match.
        Err(sqlx::Error::Database(e)) if is_unique_violation(e.as_ref()) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// True when a database error is a UNIQUE-constraint violation, matched by the
/// SQLite extended result code `SQLITE_CONSTRAINT_UNIQUE` (2067) rather than the
/// human-readable message (X3).
fn is_unique_violation(e: &dyn sqlx::error::DatabaseError) -> bool {
    e.code().as_deref() == Some("2067")
}

/// Load the currently-running job for a source, if any (F1: so an extension-level
/// start can report the already-running source alongside the newly-started ones).
pub async fn load_running_job(pool: &SqlitePool, source_id: &str) -> Result<Option<IngestJob>> {
    Ok(sqlx::query_as::<_, IngestJob>(
        "SELECT * FROM source_ingest_job WHERE source_id = ? AND state = 'running' LIMIT 1",
    )
    .bind(source_id)
    .fetch_optional(pool)
    .await?)
}

/// Cancel every running job among `source_ids` (F1: extension-level cancel).
/// Returns the affected job rows (their post-cancel state).
pub async fn cancel_running_for_sources(
    pool: &SqlitePool,
    source_ids: &[String],
) -> Result<Vec<IngestJob>> {
    let mut out = Vec::new();
    for sid in source_ids {
        if let Some(job) = load_running_job(pool, sid).await? {
            if let Some(cancelled) = cancel_job(pool, &job.id).await? {
                out.push(cancelled);
            }
        }
    }
    Ok(out)
}

/// Request cancellation: flip a still-`running` job to `cancelled`. The runner
/// observes the state between items/pages and stops. Returns the updated row,
/// or `None` when the job doesn't exist. A job already terminal is returned
/// unchanged (cancel is then a no-op, not an error).
pub async fn cancel_job(pool: &SqlitePool, job_id: &str) -> Result<Option<IngestJob>> {
    sqlx::query(
        "UPDATE source_ingest_job SET state = ?, finished_at = ? WHERE id = ? AND state = ?",
    )
    .bind(STATE_CANCELLED)
    .bind(Utc::now().to_rfc3339())
    .bind(job_id)
    .bind(STATE_RUNNING)
    .execute(pool)
    .await?;
    load_job(pool, job_id).await
}

/// The job's current state string (the runner's cancellation check).
async fn job_state(pool: &SqlitePool, job_id: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT state FROM source_ingest_job WHERE id = ?")
            .bind(job_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn load_job(pool: &SqlitePool, job_id: &str) -> Result<Option<IngestJob>> {
    Ok(
        sqlx::query_as::<_, IngestJob>("SELECT * FROM source_ingest_job WHERE id = ?")
            .bind(job_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// List jobs, newest first. `active_only` keeps only `running` ones.
pub async fn list_jobs(pool: &SqlitePool, active_only: bool) -> Result<Vec<IngestJob>> {
    let sql = if active_only {
        "SELECT * FROM source_ingest_job WHERE state = 'running' ORDER BY started_at DESC LIMIT 100"
    } else {
        "SELECT * FROM source_ingest_job ORDER BY started_at DESC LIMIT 100"
    };
    Ok(sqlx::query_as::<_, IngestJob>(sql).fetch_all(pool).await?)
}

/// Flush the in-memory progress counters onto the row. Progress-only — never
/// touches `state`, so it can't resurrect a row the admin just cancelled.
async fn write_progress(pool: &SqlitePool, job_id: &str, p: &Progress) -> Result<()> {
    sqlx::query(
        "UPDATE source_ingest_job SET pages_done = ?, items_seen = ?, succeeded = ?, \
         failed = ?, new_works = ?, auto_merged = ?, queued_for_review = ?, \
         already_existing = ? WHERE id = ?",
    )
    .bind(p.pages_done)
    .bind(p.items_seen)
    .bind(p.succeeded)
    .bind(p.failed)
    .bind(p.new_works)
    .bind(p.auto_merged)
    .bind(p.queued_for_review)
    .bind(p.already_existing)
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Terminal transition. Guarded on `state = 'running'` so a cancel that landed
/// first wins (the runner's completed/failed write then becomes a no-op).
async fn finish_job(
    pool: &SqlitePool,
    job_id: &str,
    state: &str,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE source_ingest_job SET state = ?, error = ?, finished_at = ? \
         WHERE id = ? AND state = 'running'",
    )
    .bind(state)
    .bind(error)
    .bind(Utc::now().to_rfc3339())
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Startup recovery: any job still `running` was interrupted by a restart —
/// its task is gone, so mark it failed (the row would otherwise block new jobs
/// for its source forever via the partial unique index).
pub async fn mark_interrupted_jobs(pool: &SqlitePool) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE source_ingest_job SET state = 'failed', \
         error = 'interrupted by server restart', finished_at = ? WHERE state = 'running'",
    )
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Spawn the detached runner for a freshly-created job. Never panics; every
/// outcome (including runner errors) lands in the job row.
pub fn spawn_runner(state: Arc<AppState>, job_id: String, source_id: String) {
    tokio::spawn(async move {
        let pool = state.pool.clone();
        match run_job(state.clone(), &job_id, &source_id).await {
            Ok(RunOutcome::Completed) => {
                if let Err(e) = finish_job(&pool, &job_id, STATE_COMPLETED, None).await {
                    tracing::warn!(job_id, error = %e, "ingest: failed to mark job completed");
                }
                tracing::info!(job_id, source_id, "ingest: job completed");
            }
            Ok(RunOutcome::Cancelled) => {
                // The cancel mutation already wrote the terminal state.
                tracing::info!(job_id, source_id, "ingest: job cancelled");
            }
            Err(e) => {
                let msg = e.to_string();
                if let Err(e2) = finish_job(&pool, &job_id, STATE_FAILED, Some(&msg)).await {
                    tracing::warn!(job_id, error = %e2, "ingest: failed to mark job failed");
                }
                tracing::warn!(job_id, source_id, error = %msg, "ingest: job failed");
            }
        }
    });
}

enum RunOutcome {
    Completed,
    Cancelled,
}

/// The page-walk loop (S3: items within a page are processed CONCURRENTLY, bounded
/// by `ITEM_CONCURRENCY`, and the next page is PREFETCHED while the current page's
/// items run). Per-ITEM errors are counted and skipped; a PAGE fetch error fails the
/// job (progress preserved — the admin can restart). Cancel + progress + the
/// one-job-per-source guarantees are unchanged (cancel is observed between pages;
/// progress is folded once per page from the concurrent results).
async fn run_job(state: Arc<AppState>, job_id: &str, source_id: &str) -> Result<RunOutcome> {
    let pool = state.pool.clone();
    let mut progress = Progress::default();
    let mut page: i64 = 1;
    // Prefetch buffer: the browse result for the page we're about to process.
    let mut pending = Some(
        state
            .suwayomi
            .browse_source(source_id, FetchType::Popular, page as i32, None)
            .await?,
    );
    loop {
        // Cancellation check between pages (cheap single-row read).
        if job_state(&pool, job_id).await?.as_deref() != Some(STATE_RUNNING) {
            write_progress(&pool, job_id, &progress).await?;
            return Ok(RunOutcome::Cancelled);
        }
        let (has_next, mangas) = pending.take().expect("page buffered");

        // Kick off the NEXT page browse concurrently with processing this page's
        // items, so browse latency overlaps item work.
        let prefetch = if has_next && !mangas.is_empty() && page < MAX_PAGES {
            let st = state.clone();
            let sid = source_id.to_string();
            let next = page + 1;
            Some(tokio::spawn(async move {
                st.suwayomi
                    .browse_source(&sid, FetchType::Popular, next as i32, None)
                    .await
            }))
        } else {
            None
        };

        // Process this page's items with bounded concurrency.
        let sem = Arc::new(tokio::sync::Semaphore::new(ITEM_CONCURRENCY));
        let mut set = tokio::task::JoinSet::new();
        for m in &mangas {
            let st = state.clone();
            let sem = sem.clone();
            let mid = m.id;
            set.spawn(async move {
                let _permit = sem.acquire().await.ok();
                (
                    mid,
                    crate::graphql::ingest_source_series(&st, &mid.to_string()).await,
                )
            });
        }
        while let Some(joined) = set.join_next().await {
            progress.items_seen += 1;
            match joined {
                Ok((_, Ok(r))) => progress.record_decision(&r.decision),
                Ok((mid, Err(e))) => {
                    progress.record_failure();
                    tracing::warn!(job_id, manga_id = mid, error = %e, "ingest: item failed");
                }
                Err(e) => {
                    progress.record_failure();
                    tracing::warn!(job_id, error = %e, "ingest: item task panicked");
                }
            }
            // Flush progress incrementally (every few items) so the admin sees live
            // progress within a page, not just per-page jumps.
            if progress.items_seen % PROGRESS_FLUSH_EVERY == 0 {
                write_progress(&pool, job_id, &progress).await?;
            }
        }
        progress.pages_done += 1;
        write_progress(&pool, job_id, &progress).await?;
        tracing::info!(
            job_id,
            source_id,
            page,
            items = progress.items_seen,
            succeeded = progress.succeeded,
            "ingest: page done"
        );

        // Resolve the prefetched next page (or finish).
        match prefetch {
            None => return Ok(RunOutcome::Completed),
            Some(handle) => match handle.await {
                Ok(Ok(next)) => pending = Some(next),
                Ok(Err(e)) => return Err(e),
                Err(e) => return Err(anyhow::anyhow!("prefetch task panicked: {e}")),
            },
        }
        page += 1;
        tokio::time::sleep(std::time::Duration::from_millis(PAGE_DELAY_MS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn unique_violation_is_matched_by_extended_code_not_message() {
        // X3: the one-running-job-per-source race is detected by the SQLite
        // extended result code (2067), returning None rather than an error.
        let pool = pool().await;
        assert!(try_start_job(&pool, "src-x").await.unwrap().is_some());
        // Second concurrent start hits uq_source_ingest_running → 2067 → None.
        let dup = try_start_job(&pool, "src-x").await;
        assert!(
            matches!(dup, Ok(None)),
            "duplicate running job must map the 2067 unique violation to Ok(None), got {dup:?}"
        );
    }

    #[tokio::test]
    async fn extension_ingest_fan_out_and_skip_running() {
        // F1: an extension with several sources starts a job per source; a source
        // that already has a running job is represented by its existing job, not a
        // new one, and never errors the batch.
        let pool = pool().await;
        // Pretend "src-b" already has a running job (as if from an earlier start).
        let running_b = try_start_job(&pool, "src-b").await.unwrap().unwrap();

        // Simulate the resolver's per-source loop over the extension's 3 sources.
        let mut jobs = Vec::new();
        for sid in ["src-a", "src-b", "src-c"] {
            match try_start_job(&pool, sid).await.unwrap() {
                Some(job) => jobs.push(job),
                None => jobs.push(load_running_job(&pool, sid).await.unwrap().unwrap()),
            }
        }
        assert_eq!(jobs.len(), 3, "one job entry per source");
        // src-b reused the pre-existing running job (same id), a/c are fresh.
        let b = jobs.iter().find(|j| j.source_id == "src-b").unwrap();
        assert_eq!(b.id, running_b.id, "already-running source reuses its job");
        // Exactly 3 running rows total (no duplicate for src-b).
        let running = list_jobs(&pool, true).await.unwrap();
        assert_eq!(running.len(), 3);

        // Extension-level cancel stops all three.
        let cancelled =
            cancel_running_for_sources(&pool, &["src-a".into(), "src-b".into(), "src-c".into()])
                .await
                .unwrap();
        assert_eq!(cancelled.len(), 3);
        assert!(cancelled.iter().all(|j| j.state == STATE_CANCELLED));
        assert_eq!(
            list_jobs(&pool, true).await.unwrap().len(),
            0,
            "none running"
        );
    }

    #[tokio::test]
    async fn one_running_job_per_source_and_full_lifecycle() {
        let pool = pool().await;

        // Start: creates a running row.
        let job = try_start_job(&pool, "src-1").await.unwrap().expect("job");
        assert_eq!(job.state, STATE_RUNNING);
        assert_eq!(job.source_id, "src-1");

        // A second start for the SAME source is refused while one runs...
        assert!(try_start_job(&pool, "src-1").await.unwrap().is_none());
        // ...but another source is independent.
        let other = try_start_job(&pool, "src-2").await.unwrap();
        assert!(other.is_some());

        // Progress writes update counters without touching state.
        let p = Progress {
            pages_done: 2,
            items_seen: 40,
            succeeded: 38,
            failed: 2,
            new_works: 30,
            auto_merged: 3,
            queued_for_review: 1,
            already_existing: 4,
        };
        write_progress(&pool, &job.id, &p).await.unwrap();
        let row = load_job(&pool, &job.id).await.unwrap().unwrap();
        assert_eq!(row.state, STATE_RUNNING);
        assert_eq!(row.items_seen, 40);
        assert_eq!(row.new_works, 30);

        // Completion is terminal and frees the source for a new job.
        finish_job(&pool, &job.id, STATE_COMPLETED, None)
            .await
            .unwrap();
        let row = load_job(&pool, &job.id).await.unwrap().unwrap();
        assert_eq!(row.state, STATE_COMPLETED);
        assert!(row.finished_at.is_some());
        assert!(
            try_start_job(&pool, "src-1").await.unwrap().is_some(),
            "a finished job no longer blocks its source"
        );
    }

    #[tokio::test]
    async fn cancel_flips_running_only_and_finish_does_not_resurrect() {
        let pool = pool().await;
        let job = try_start_job(&pool, "src-1").await.unwrap().unwrap();

        // Cancel a running job → cancelled + finished_at stamped.
        let row = cancel_job(&pool, &job.id).await.unwrap().unwrap();
        assert_eq!(row.state, STATE_CANCELLED);
        assert!(row.finished_at.is_some());

        // The runner's late "completed" write must NOT overwrite the cancel.
        finish_job(&pool, &job.id, STATE_COMPLETED, None)
            .await
            .unwrap();
        let row = load_job(&pool, &job.id).await.unwrap().unwrap();
        assert_eq!(row.state, STATE_CANCELLED, "cancel wins over a late finish");

        // Cancelling an already-terminal job is a no-op that returns the row.
        let again = cancel_job(&pool, &job.id).await.unwrap().unwrap();
        assert_eq!(again.state, STATE_CANCELLED);
        // Cancelling an unknown id yields None.
        assert!(cancel_job(&pool, "ing_nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn startup_marks_interrupted_jobs_failed() {
        let pool = pool().await;
        let a = try_start_job(&pool, "src-1").await.unwrap().unwrap();
        let b = try_start_job(&pool, "src-2").await.unwrap().unwrap();
        finish_job(&pool, &b.id, STATE_COMPLETED, None)
            .await
            .unwrap();

        let n = mark_interrupted_jobs(&pool).await.unwrap();
        assert_eq!(n, 1, "only the still-running job is marked");
        let ra = load_job(&pool, &a.id).await.unwrap().unwrap();
        assert_eq!(ra.state, STATE_FAILED);
        assert_eq!(ra.error.as_deref(), Some("interrupted by server restart"));
        let rb = load_job(&pool, &b.id).await.unwrap().unwrap();
        assert_eq!(rb.state, STATE_COMPLETED, "terminal rows untouched");

        // The freed source can start a new job.
        assert!(try_start_job(&pool, "src-1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn list_jobs_filters_active() {
        let pool = pool().await;
        let a = try_start_job(&pool, "src-1").await.unwrap().unwrap();
        let b = try_start_job(&pool, "src-2").await.unwrap().unwrap();
        finish_job(&pool, &b.id, STATE_FAILED, Some("boom"))
            .await
            .unwrap();

        let all = list_jobs(&pool, false).await.unwrap();
        assert_eq!(all.len(), 2);
        let active = list_jobs(&pool, true).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, a.id);
    }

    #[test]
    fn progress_records_decisions() {
        let mut p = Progress::default();
        for d in [
            "new",
            "auto_merge",
            "review",
            "existing",
            "new",
            "review_consolidated",
        ] {
            p.record_decision(d);
        }
        p.record_failure();
        assert_eq!(p.succeeded, 6);
        assert_eq!(p.new_works, 2);
        assert_eq!(p.auto_merged, 1);
        // "review" + "review_consolidated" both land in the review bucket.
        assert_eq!(p.queued_for_review, 2);
        assert_eq!(p.already_existing, 1);
        assert_eq!(p.failed, 1);
        // Categories must reconcile with the success total (no uncounted decisions).
        assert_eq!(
            p.new_works + p.auto_merged + p.queued_for_review + p.already_existing,
            p.succeeded
        );
    }
}
