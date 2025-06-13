//! Phase B3 — the two drains that populate the canonical chapter spine.
//!
//! Migration 0090 is deliberately inert: it adds `chapter.chapter_key`, `chapter.label`
//! and `chapter.scanlator` and moves no data. This module is what moves the data, in the
//! background, after boot, in small transactions, resumably.
//!
//! TWO DRAINS, BOTH SELF-DRAINING AND SELF-HEALING.
//!
//! 1. **Keys.** 877,824 existing `chapter` rows have `chapter_key IS NULL`. Their work-list
//!    is migration 0090's partial index, which holds exactly the rows still to do and
//!    shrinks to empty as the drain runs.
//! 2. **Suwayomi chapters.** 563,095 `suwayomi_chapter` rows have never had a `chapter`
//!    row at all. Their work-list is "a Suwayomi `source_series` that has cached chapters
//!    and no spine rows".
//!
//! NEITHER DRAIN KEEPS A CURSOR, and that is the point. A cursor is a second piece of
//! state that can be wrong: it goes stale across restarts, it silently skips anything that
//! becomes eligible *behind* it, and it has to be reset by hand when the definition of
//! "done" changes. Both work-lists above are instead derived from the data itself, so an
//! interrupted drain resumes simply by asking again, a series that gains a `source_series`
//! mapping tomorrow is picked up tomorrow, and the drain terminates when — and only when —
//! there is genuinely nothing left. The cost is one indexed query per batch.
//!
//! WHY IT IS PACED. Keying 877k rows rewrites every one of them, which is real WAL churn
//! on a database Litestream is replicating and the scanner is writing to concurrently. The
//! pacing below keeps that to roughly a thousand rows a second, so the whole key drain
//! costs ~15 minutes of gentle background writes rather than one lock-holding burst
//! against a 15 s `busy_timeout`.

use anyhow::Result;
use sqlx::SqlitePool;
use std::time::Duration;

use super::{replace_source_chapters, spine_key_and_label, suwayomi_spine_input, ChapterInput};

/// Rows re-keyed per transaction. Small enough that the transaction is short even under a
/// concurrent scanner write, large enough that per-statement overhead does not dominate.
const KEY_BATCH: i64 = 500;
/// Series materialised into the spine per batch. Each one is its own transaction inside
/// `replace_source_chapters`, and a series carries a median of ~50 and a maximum of ~1,200
/// chapters, so this is a few thousand rows at the top end.
const SERIES_BATCH: i64 = 25;
/// Gap between batches. This is the throttle, and the arithmetic behind it is the whole
/// safety argument: 877,824 rows to key at 500 a batch is ~1,756 batches, so a one-second
/// gap spreads the entire drain — ~1.44 M row writes across both halves — over roughly
/// half an hour of background trickle rather than a burst. Litestream is replicating this
/// database and the scanner is writing to it concurrently; there is no deadline here worth
/// competing with either for.
const BATCH_GAP: Duration = Duration::from_secs(1);
/// Stagger behind the boot write burst — the scanner's first tick, the feed and FTS
/// rebuilds and Litestream's first checkpoint all land in the first minute, and the
/// catalogue backfill records that starting at t+0 cost four works to `SQLITE_BUSY`.
const BOOT_DELAY: Duration = Duration::from_secs(90);
/// How often to re-check for work once both drains report empty. New chapters arrive
/// through `series_cache::put_chapters`, which keys them on the way in, so a drained spine
/// only re-fills when a series gains a `source_series` mapping — rare, and never urgent.
const IDLE_RECHECK: Duration = Duration::from_secs(30 * 60);

/// What one pass moved. Both counters are zero exactly when the spine is complete.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainOutcome {
    pub keyed: u64,
    pub series_materialised: u64,
}

impl DrainOutcome {
    pub fn is_idle(&self) -> bool {
        self.keyed == 0 && self.series_materialised == 0
    }
}

/// Give `chapter_key` to one batch of rows that have none. Returns how many were keyed;
/// `0` means the key drain is complete.
///
/// The key is computed in Rust, by the same `chapter_display` call the write path uses,
/// NOT by a SQL `round(number * 100)`. Doing it in SQL looks obviously cheaper and is
/// obviously wrong: `CAST('Extra' AS REAL)` is `0.0`, so every one of the 23,254
/// non-numeric chapters would be keyed as chapter 0 and collide with real chapter zeros —
/// which is precisely the bug the `x:<external_id>` namespace exists to prevent.
pub async fn drain_chapter_keys(pool: &SqlitePool) -> Result<u64> {
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, external_id, number, title FROM chapter \
         WHERE chapter_key IS NULL LIMIT ?",
    )
    .bind(KEY_BATCH)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let mut tx = pool.begin().await?;
    let mut keyed = 0u64;
    for (id, external_id, number, title) in rows {
        let (key, label) = spine_key_and_label(&ChapterInput {
            external_id,
            number,
            title,
            ..Default::default()
        });
        sqlx::query("UPDATE chapter SET chapter_key = ?, label = ? WHERE id = ?")
            .bind(&key)
            .bind(&label)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        keyed += 1;
    }
    tx.commit().await?;
    Ok(keyed)
}

/// Materialise one batch of Suwayomi series into the spine. Returns how many series were
/// written; `0` means the Suwayomi drain is complete.
///
/// The work-list is "has cached chapters, has no spine rows". `NOT EXISTS` over
/// `idx_chapter_source_series` and `EXISTS` over `idx_suwayomi_chapter_manga` are both
/// index probes, and the outer scan is bounded by the 14,103 Suwayomi `source_series`
/// rows, so this stays a cheap query even when it returns nothing — which is what it will
/// do for the rest of the deployment's life once the drain finishes.
pub async fn drain_suwayomi_series(pool: &SqlitePool) -> Result<u64> {
    let pending: Vec<(String, i64)> = sqlx::query_as(
        "SELECT ss.id, CAST(ss.source_key AS INTEGER) \
         FROM source_series ss \
         WHERE ss.source_type = 'suwayomi' \
           AND EXISTS (SELECT 1 FROM suwayomi_chapter sc \
                        WHERE sc.manga_id = CAST(ss.source_key AS INTEGER)) \
           AND NOT EXISTS (SELECT 1 FROM chapter c WHERE c.source_series_id = ss.id) \
         LIMIT ?",
    )
    .bind(SERIES_BATCH)
    .fetch_all(pool)
    .await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let mut done = 0u64;
    for (ssid, manga_id) in pending {
        let chapters: Vec<(i64, String, f64, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT id, name, chapter_number, scanlator, upload_date \
             FROM suwayomi_chapter WHERE manga_id = ?",
        )
        .bind(manga_id)
        .fetch_all(pool)
        .await?;
        if chapters.is_empty() {
            // Raced with a `put_chapters` that emptied the list. Nothing to do, and the
            // `EXISTS` guard will stop offering it.
            continue;
        }
        let inputs: Vec<ChapterInput> = chapters
            .iter()
            .map(|(id, name, number, scanlator, upload_date)| {
                suwayomi_spine_input(
                    *id,
                    name,
                    *number,
                    scanlator.as_deref(),
                    upload_date.as_deref(),
                )
            })
            .collect();
        replace_source_chapters(pool, &ssid, &inputs).await?;
        done += 1;
    }
    Ok(done)
}

/// One pass of both drains.
pub async fn drain_once(pool: &SqlitePool) -> Result<DrainOutcome> {
    Ok(DrainOutcome {
        keyed: drain_chapter_keys(pool).await?,
        series_materialised: drain_suwayomi_series(pool).await?,
    })
}

/// One pass of the ledger seed — but ONLY once the spine is complete.
///
/// THE ORDERING IS A CORRECTNESS CONSTRAINT, NOT A TIDINESS ONE. `release_event` is
/// written with `INSERT OR IGNORE`, which is what makes first-source-wins impossible to
/// regress — and also means a row, once written, is never corrected. Seeding a work while
/// half its sources are still missing from the spine would therefore record the MangaDex
/// mirror as the first source for every chapter, permanently, even for chapters a Suwayomi
/// source published weeks earlier. The wrong winner would then be un-fixable by any later
/// pass.
///
/// So the seed does not start until both spine drains report genuinely empty. Returns
/// `None` while the spine is still filling.
pub async fn seed_ledger_if_spine_complete(pool: &SqlitePool) -> Result<Option<u64>> {
    if remaining(pool).await? != (0, 0) {
        return Ok(None);
    }
    Ok(Some(super::ledger::seed_batch(pool).await?))
}

/// How much is left, for the log line and for the exit-criteria check.
///
/// Only called once the drains report idle, i.e. when both answers should be 0 — so
/// `COUNT(*)` over migration 0090's partial index reads an index that is by then empty,
/// not the 877k-row table.
pub async fn remaining(pool: &SqlitePool) -> Result<(i64, i64)> {
    let unkeyed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chapter WHERE chapter_key IS NULL")
        .fetch_one(pool)
        .await?;
    let unmaterialised: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM source_series ss \
         WHERE ss.source_type = 'suwayomi' \
           AND EXISTS (SELECT 1 FROM suwayomi_chapter sc \
                        WHERE sc.manga_id = CAST(ss.source_key AS INTEGER)) \
           AND NOT EXISTS (SELECT 1 FROM chapter c WHERE c.source_series_id = ss.id)",
    )
    .fetch_one(pool)
    .await?;
    Ok((unkeyed, unmaterialised))
}

/// One unit of background work: spine first, ledger once the spine is complete. Returns
/// how much moved — `0` means everything is caught up.
async fn pass(pool: &SqlitePool) -> Result<u64> {
    let spine = drain_once(pool).await?;
    if !spine.is_idle() {
        return Ok(spine.keyed + spine.series_materialised);
    }
    Ok(seed_ledger_if_spine_complete(pool).await?.unwrap_or(0))
}

/// Say what state we ended in, once, on the transition to idle. A `warn` rather than an
/// `info` when work remains: "the drains report nothing to do but there IS something left"
/// means rows are unreachable, not merely pending, and that is a bug to look at.
async fn report(pool: &SqlitePool) {
    match remaining(pool).await {
        Ok((0, 0)) => match super::ledger::remaining(pool).await {
            Ok((events, 0)) => {
                // The deployment guard: a future-dated event means something took its time
                // from `publishAt` or from `now()`, and page 1 of /updates is wrong.
                match super::ledger::assert_no_future_events(pool).await {
                    Ok(()) => tracing::info!(
                        events,
                        "spine: drained — chapter spine and release ledger complete"
                    ),
                    Err(e) => tracing::error!(error = %e, "spine: RELEASE LEDGER IS CORRUPT"),
                }
            }
            Ok((events, pending)) => tracing::warn!(
                events,
                pending,
                "spine: ledger seed idle but chapters remain unrecorded"
            ),
            Err(e) => tracing::warn!(error = %e, "spine: ledger remaining check failed"),
        },
        Ok((unkeyed, unmaterialised)) => tracing::warn!(
            unkeyed,
            unmaterialised,
            "spine: drains reported idle but work remains — \
             rows are unreachable, not merely pending"
        ),
        Err(e) => tracing::warn!(error = %e, "spine: remaining check failed"),
    }
}

/// Run the drains to completion in the background, then keep idling cheaply.
pub fn spawn(pool: SqlitePool, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::spawn(async move {
        /// Sleep unless shutdown fires first. `true` => stop.
        ///
        /// A dropped sender means the process is going away and `changed()` returns `Err`
        /// instantly and forever; treating that as "keep going" would collapse every gap
        /// in this loop to a no-op and turn the throttle off, which is exactly the
        /// mistake the catalogue backfill documents having made.
        async fn nap(d: Duration, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> bool {
            tokio::select! {
                _ = tokio::time::sleep(d) => false,
                r = shutdown.changed() => r.is_err() || *shutdown.borrow(),
            }
        }
        if nap(BOOT_DELAY, &mut shutdown).await {
            return;
        }
        let mut announced = false;
        loop {
            if *shutdown.borrow() {
                return;
            }
            match pass(&pool).await {
                Ok(0) => {
                    if !announced {
                        report(&pool).await;
                        announced = true;
                    }
                    if nap(IDLE_RECHECK, &mut shutdown).await {
                        return;
                    }
                }
                Ok(_) => {
                    // Log on the transition into work, not per batch: at 500 rows a batch
                    // the key drain alone is ~1,750 batches.
                    if announced {
                        tracing::info!("spine: work appeared, draining again");
                    }
                    announced = false;
                    if nap(BATCH_GAP, &mut shutdown).await {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "spine: drain pass failed — retrying");
                    if nap(IDLE_RECHECK, &mut shutdown).await {
                        return;
                    }
                }
            }
        }
    });
}
