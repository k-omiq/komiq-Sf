//! Background garbage collection of orphaned staged uploads and cover blobs.
//!
//! `POST /comment-media` inserts a `comment_media` row with `comment_id = NULL`
//! (a staged draft owned by the uploader); `postComment` later links it to the new
//! comment. A draft the user never posts stays NULL forever. Migration 0023 always
//! intended these to be "garbage-collected by age" — this task is that GC. It runs
//! hourly and deletes staged rows older than [`STAGED_TTL_HOURS`], bounding
//! unbounded SQLite growth (and the Litestream → R2 replication it drives).
//!
//! The same loop reconciles the SEPARATE cover-cache DB (`db::init_covers`) against
//! the main one — see [`sweep_cover_blobs`].

use std::time::Duration;

use sqlx::SqlitePool;

/// Staged (unlinked) uploads older than this are swept. A user posting a comment
/// always links its image within seconds, so a day is a generous grace period for
/// slow multi-step composes without letting abandoned drafts accumulate.
const STAGED_TTL_HOURS: i64 = 24;

/// How often the sweep runs.
const SWEEP_INTERVAL_SECS: u64 = 3600;

/// Spawn the recurring GC sweep. Runs until `shutdown` resolves.
pub fn spawn(pool: SqlitePool, covers: SqlitePool, shutdown: tokio::sync::watch::Receiver<bool>) {
    // Panic-supervised (crate::task::supervise): a panic in `sweep` used to end the GC
    // job silently for the process lifetime, letting the staged-media BLOB store grow
    // unbounded. `factory` re-clones the pools + shutdown handle on each restart.
    tokio::spawn(crate::task::supervise(
        "gc",
        Duration::from_secs(30),
        shutdown.clone(),
        move || run_loop(pool.clone(), covers.clone(), shutdown.clone()),
    ));
}

async fn run_loop(
    pool: SqlitePool,
    covers: SqlitePool,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Start 150s behind the scanner so the boot burst is spread across the five
    // background loops rather than landing in one instant — see scanner::run_loop.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(150),
        Duration::from_secs(SWEEP_INTERVAL_SECS),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!("GC sweep started");
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                sweep(&pool).await;
                sweep_cover_blobs(&pool, &covers, &shutdown).await;
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("GC sweep stopping");
                    break;
                }
            }
        }
    }
}

/// Delete staged (`comment_id IS NULL`) comment-media rows older than the TTL.
/// Best-effort: any error is logged and swallowed so a transient DB blip doesn't
/// kill the sweep loop.
async fn sweep(pool: &SqlitePool) {
    // created_at is written as `chrono::Utc::now().to_rfc3339()`, so a UTC RFC-3339
    // cutoff compares correctly against it lexically.
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(STAGED_TTL_HOURS)).to_rfc3339();
    match sqlx::query("DELETE FROM comment_media WHERE comment_id IS NULL AND created_at < ?")
        .bind(&cutoff)
        .execute(pool)
        .await
    {
        Ok(r) if r.rows_affected() > 0 => {
            tracing::info!(
                deleted = r.rows_affected(),
                "gc: swept orphaned comment media"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "gc: comment-media sweep failed"),
    }

    // Prune view buckets past the retention window so `series_view_bucket` stays bounded
    // (the all-time totals in `series_views` are kept). Best-effort like the sweep above.
    match crate::views::prune_buckets(pool).await {
        Ok(n) if n > 0 => tracing::info!(deleted = n, "gc: pruned old view buckets"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "gc: view-bucket prune failed"),
    }
}

// ---------------------------------------------------------------------------------
// Orphaned cover blobs
// ---------------------------------------------------------------------------------

/// Ids read from the covers DB per batch. Kept well under SQLite's bind-variable limit
/// because each id becomes one `?` in the liveness probe against the main DB.
const COVER_SCAN_BATCH: usize = 500;

/// Hard cap on blobs deleted per sweep run, per table. Two jobs: it bounds how long the
/// covers DB's single writer is held on any one tick, and it bounds the blast radius if
/// the liveness probe were ever wrong — a mistake costs at most this many re-derivable
/// thumbnails before the next tick's logs make it visible.
const COVER_DELETE_BUDGET: usize = 1000;

/// A blob written more recently than this is never a candidate, no matter what the
/// liveness probe says. See `sweep_cover_blobs` for why this is belt-and-braces rather
/// than the primary safety property.
const COVER_ORPHAN_MIN_AGE_HOURS: i64 = 24;

/// Refuse to sweep at all unless the main DB holds at least this many works / cached
/// series. A host that restored `komika.sqlite3` but is still replaying, or one pointed
/// at a fresh/empty DB, would otherwise look like "every blob is orphaned" and the sweep
/// would happily empty a 20 GB cache. Production carries 112,760 works and 13,802 cached
/// series, so these floors are three orders of magnitude below normal and only trip on a
/// genuinely broken main DB.
const COVER_SWEEP_MIN_WORKS: i64 = 10_000;
const COVER_SWEEP_MIN_SERIES: i64 = 1_000;

/// Ids probed BEFORE any delete, to estimate what share of the store the sweep is
/// about to consider orphaned. See [`orphan_rate_sane`].
const COVER_RATIO_SAMPLE: usize = 2_000;

/// Abort the sweep when more than this percentage of the pre-flight sample looks
/// orphaned.
///
/// The row-count floors above only catch a main DB that is EMPTY. They do nothing for
/// the realistic misconfiguration: a process pointed at a *different but populated*
/// main DB (a staging copy, a rolled-back restore, a swapped `DATABASE_URL`) while
/// `COVERS_DATABASE_URL` still points at the production cover store. Every id then
/// probes as missing, the floor passes, and the sweep quietly eats the 20 GB cache at
/// [`COVER_DELETE_BUDGET`] rows an hour.
///
/// Measured on production, the true orphan rate is 7.30% globally (8,868 of 121,552),
/// and a 2,000-id prefix sample estimates it at 7.75% — work ids are `w_` + a random
/// UUID, so a lexicographic prefix IS a uniform sample of the key space. 25% leaves
/// better than 3x headroom over normal while the failure mode it guards against sits
/// near 100%.
///
/// Suwayomi manga ids are sequential rather than random, so their prefix is the OLDEST
/// cohort and therefore the orphan-richest — the estimate is biased HIGH, which is the
/// safe direction (it aborts more readily, it never permits more deleting). At today's
/// 1,084 rows the sample covers the whole table and the estimate is exact.
const COVER_MAX_ORPHAN_PCT: u64 = 25;

/// Reclaim cover blobs whose owning row no longer exists in the MAIN database.
///
/// `work_cover_blob` / `suwayomi_cover_blob` live in a separate SQLite file with no FK
/// to `work` / `suwayomi_series` (see `db::init_covers`), so no cascade in the main DB
/// can ever reach them. `catalog::reclaim_cover_blob` covers merges going forward, but
/// `delete_work_cascade` and the foreign-language purge do not, and neither touched the
/// backlog: a live audit measured 8,868 orphaned work blobs (1,464 MiB of a 20.4 GB
/// store) plus 78 orphaned Suwayomi blobs.
///
/// # Why this is safe against a concurrent insert
///
/// The sweep reads ids from the covers DB, asks the main DB which of them are live, then
/// deletes the rest — two databases, two pools, so the check and the delete cannot be one
/// transaction. What makes the window harmless is that **a work id is never reused**:
/// `catalog` mints them with `new_id("w_")` (a fresh UUID), so an id that is missing from
/// `work` at probe time cannot be re-created between the probe and the delete. The only
/// thing that can happen in the window is a work being *deleted*, which merely defers its
/// blob to the next tick. The write ordering points the same way: `cover::put_work_cover`
/// writes the blob for a work that already exists, never before it, so "blob present,
/// work absent" strictly implies the work was removed.
///
/// The "never before the work" write ordering is *almost* exact. `serve_cover` and the
/// cover drainer both check the work exists, then spend an unbounded network fetch plus
/// a CPU resize before calling `put_work_cover` — so a work deleted inside that window
/// does get a blob written after its row is gone. [`COVER_ORPHAN_MIN_AGE_HOURS`] covers
/// exactly that case: such a blob is stamped `now`, so it is protected for a full day.
///
/// Three further guards make a *wrong* answer survivable rather than destructive:
/// [`COVER_ORPHAN_MIN_AGE_HOURS`] excludes anything written in the last day (re-checked
/// atomically inside the DELETE, so a blob re-materialized after the probe is skipped),
/// [`COVER_MAX_ORPHAN_PCT`] aborts the run outright when the store and the main DB
/// disagree implausibly widely (see it for the misconfiguration this catches and the
/// row-count floors do not), and [`COVER_DELETE_BUDGET`] caps a single run. And the
/// payload itself is re-derivable — a wrongly-dropped cover re-caches on the next
/// request or crawl.
///
/// Deleting rows does NOT shrink the 20.5 GB file: the freed pages go on SQLite's
/// freelist and are reused by subsequent cover writes. That is the intended outcome
/// (the store stops growing) — reclaiming the disk itself would need a `VACUUM`, which
/// rewrites all 20.5 GB and is deliberately not done here.
async fn sweep_cover_blobs(
    pool: &SqlitePool,
    covers: &SqlitePool,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) {
    let cutoff =
        (chrono::Utc::now() - chrono::Duration::hours(COVER_ORPHAN_MIN_AGE_HOURS)).to_rfc3339();

    match sweep_work_cover_blobs(pool, covers, &cutoff, shutdown).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(deleted = n, "gc: reclaimed orphaned work cover blobs"),
        Err(e) => tracing::warn!(error = %e, "gc: work cover blob sweep failed"),
    }
    if *shutdown.borrow() {
        return;
    }
    match sweep_suwayomi_cover_blobs(pool, covers, &cutoff, shutdown).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(deleted = n, "gc: reclaimed orphaned suwayomi cover blobs"),
        Err(e) => tracing::warn!(error = %e, "gc: suwayomi cover blob sweep failed"),
    }
}

async fn sweep_work_cover_blobs(
    pool: &SqlitePool,
    covers: &SqlitePool,
    cutoff: &str,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<u64> {
    let live_works: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
        .fetch_one(pool)
        .await?;
    if live_works < COVER_SWEEP_MIN_WORKS {
        tracing::warn!(
            live_works,
            floor = COVER_SWEEP_MIN_WORKS,
            "gc: main DB has implausibly few works — skipping the cover blob sweep"
        );
        return Ok(0);
    }

    // Pre-flight: sample the key space and refuse to delete anything if the orphan rate
    // is implausible. Runs BEFORE the first DELETE, every run, so a main DB that is
    // merely populated-but-wrong (which the row-count floor above waves through) costs
    // four probe queries instead of the cover store.
    let sample: Vec<String> =
        sqlx::query_scalar("SELECT work_id FROM work_cover_blob ORDER BY work_id LIMIT ?")
            .bind(COVER_RATIO_SAMPLE as i64)
            .fetch_all(covers)
            .await?;
    let mut sample_orphans = 0usize;
    for chunk in sample.chunks(COVER_SCAN_BATCH) {
        sample_orphans += missing_work_ids(pool, chunk).await?.len();
    }
    if !orphan_rate_sane(sample_orphans, sample.len()) {
        tracing::error!(
            sampled = sample.len(),
            orphans = sample_orphans,
            max_pct = COVER_MAX_ORPHAN_PCT,
            "gc: implausible work cover orphan rate — is the main DB the right one? \
             skipping the sweep"
        );
        return Ok(0);
    }

    let mut deleted = 0u64;
    // Keyset pagination over the PK index. `work_id` is the whole index, so selecting
    // only that column is an index-ONLY scan — it never touches the (blob-bearing) table
    // rows, which is what keeps an hourly full pass over 121k rows off the 20 GB of blob
    // pages.
    let mut cursor = String::new();
    loop {
        if *shutdown.borrow() {
            break;
        }
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT work_id FROM work_cover_blob WHERE work_id > ? ORDER BY work_id LIMIT ?",
        )
        .bind(&cursor)
        .bind(COVER_SCAN_BATCH as i64)
        .fetch_all(covers)
        .await?;
        let Some(last) = ids.last().cloned() else {
            break;
        };
        cursor = last;

        let orphans = missing_work_ids(pool, &ids).await?;
        if !orphans.is_empty() {
            let placeholders = std::iter::repeat_n("?", orphans.len())
                .collect::<Vec<_>>()
                .join(",");
            // `updated_at < ?` is re-evaluated here, inside the delete — a blob
            // re-materialized since the probe has a newer stamp and survives.
            let sql = format!(
                "DELETE FROM work_cover_blob WHERE work_id IN ({placeholders}) AND updated_at < ?"
            );
            let mut q = sqlx::query(&sql);
            for id in &orphans {
                q = q.bind(id);
            }
            let res = q.bind(cutoff).execute(covers).await?;
            deleted += res.rows_affected();
            if deleted as usize >= COVER_DELETE_BUDGET {
                tracing::info!(
                    deleted,
                    "gc: cover blob delete budget reached — resuming next tick"
                );
                break;
            }
        }
        // Hand the covers writer back between batches so a cover materialization never
        // queues behind the whole sweep.
        tokio::task::yield_now().await;
    }
    Ok(deleted)
}

/// Whether `orphans` out of `sampled` probed ids is a believable orphan rate.
///
/// An empty sample means an empty cover store — nothing to delete, so nothing to guard;
/// say yes rather than tripping the breaker on a legitimately fresh host. Integer
/// arithmetic on `u64` (the sample is at most [`COVER_RATIO_SAMPLE`], so `* 100` cannot
/// overflow).
fn orphan_rate_sane(orphans: usize, sampled: usize) -> bool {
    if sampled == 0 {
        return true;
    }
    (orphans as u64) * 100 <= (sampled as u64) * COVER_MAX_ORPHAN_PCT
}

/// Of `ids`, those with no `work` row. One `IN (...)` probe per batch.
async fn missing_work_ids(pool: &SqlitePool, ids: &[String]) -> anyhow::Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new()); // `IN ()` is a syntax error; never build it
    }
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id FROM work WHERE id IN ({placeholders})");
    let mut q = sqlx::query_scalar::<_, String>(&sql);
    for id in ids {
        q = q.bind(id);
    }
    let live: std::collections::HashSet<String> = q.fetch_all(pool).await?.into_iter().collect();
    Ok(ids
        .iter()
        .filter(|id| !live.contains(*id))
        .cloned()
        .collect())
}

/// Of `ids`, those with no `suwayomi_series` row. Bound as `?` parameters rather than
/// interpolated: the SQL text is then identical for every full batch, so sqlx's prepared
/// statement cache holds ONE entry instead of one per batch.
async fn missing_series_ids(pool: &SqlitePool, ids: &[i64]) -> anyhow::Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(Vec::new()); // `IN ()` is a syntax error; never build it
    }
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id FROM suwayomi_series WHERE id IN ({placeholders})");
    let mut q = sqlx::query_scalar::<_, i64>(&sql);
    for id in ids {
        q = q.bind(*id);
    }
    let live: std::collections::HashSet<i64> = q.fetch_all(pool).await?.into_iter().collect();
    Ok(ids
        .iter()
        .copied()
        .filter(|id| !live.contains(id))
        .collect())
}

async fn sweep_suwayomi_cover_blobs(
    pool: &SqlitePool,
    covers: &SqlitePool,
    cutoff: &str,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<u64> {
    let live_series: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM suwayomi_series")
        .fetch_one(pool)
        .await?;
    if live_series < COVER_SWEEP_MIN_SERIES {
        tracing::warn!(
            live_series,
            floor = COVER_SWEEP_MIN_SERIES,
            "gc: main DB has implausibly few cached series — skipping the suwayomi cover sweep"
        );
        return Ok(0);
    }

    // Same pre-flight breaker as the work sweep — see `orphan_rate_sane` and
    // `COVER_MAX_ORPHAN_PCT`. The row-count floor above cannot tell a wrong-but-populated
    // main DB from the right one; this can.
    let sample: Vec<i64> =
        sqlx::query_scalar("SELECT manga_id FROM suwayomi_cover_blob ORDER BY manga_id LIMIT ?")
            .bind(COVER_RATIO_SAMPLE as i64)
            .fetch_all(covers)
            .await?;
    let mut sample_orphans = 0usize;
    for chunk in sample.chunks(COVER_SCAN_BATCH) {
        sample_orphans += missing_series_ids(pool, chunk).await?.len();
    }
    if !orphan_rate_sane(sample_orphans, sample.len()) {
        tracing::error!(
            sampled = sample.len(),
            orphans = sample_orphans,
            max_pct = COVER_MAX_ORPHAN_PCT,
            "gc: implausible suwayomi cover orphan rate — is the main DB the right one? \
             skipping the sweep"
        );
        return Ok(0);
    }

    let mut deleted = 0u64;
    let mut cursor: i64 = i64::MIN;
    loop {
        if *shutdown.borrow() {
            break;
        }
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT manga_id FROM suwayomi_cover_blob WHERE manga_id > ? \
             ORDER BY manga_id LIMIT ?",
        )
        .bind(cursor)
        .bind(COVER_SCAN_BATCH as i64)
        .fetch_all(covers)
        .await?;
        let Some(last) = ids.last().copied() else {
            break;
        };
        cursor = last;

        // Suwayomi ids come from the source rather than being minted by us, so the
        // "never reused" property that makes the work sweep exact does NOT hold here,
        // and "orphan" is weaker: `serve_suwayomi_image` materializes a downscaled cover
        // for any manga id a browse/discovery feed returned, including ones never cached
        // as a series. Measured live, 78 of 1,075 blobs are in that state, 9 of them
        // written within the last day. The consequence of sweeping one is a single
        // re-downscale on the next request, and the age gate then protects it for
        // another day — so the worst case is one refetch per cover per 24h, which is
        // the re-derivable-cache contract this store is built on.
        let orphans = missing_series_ids(pool, &ids).await?;
        if !orphans.is_empty() {
            let list = orphans
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let res = sqlx::query(&format!(
                "DELETE FROM suwayomi_cover_blob WHERE manga_id IN ({list}) AND updated_at < ?"
            ))
            .bind(cutoff)
            .execute(covers)
            .await?;
            deleted += res.rows_affected();
            if deleted as usize >= COVER_DELETE_BUDGET {
                break;
            }
        }
        tokio::task::yield_now().await;
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    /// Main DB with the real schema. `max_connections(1)` because a `sqlite::memory:`
    /// pool gives every connection its OWN empty database.
    async fn main_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    /// Covers DB, schema copied from `db::init_covers` (which can't be reused here: it
    /// opens a 4-connection pool, and four `sqlite::memory:` connections are four
    /// different databases).
    async fn covers_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for ddl in [
            "CREATE TABLE work_cover_blob (work_id TEXT PRIMARY KEY, webp BLOB NOT NULL, \
             version INTEGER NOT NULL, updated_at TEXT NOT NULL)",
            "CREATE TABLE suwayomi_cover_blob (manga_id INTEGER PRIMARY KEY, \
             webp BLOB NOT NULL, updated_at TEXT NOT NULL)",
        ] {
            sqlx::query(ddl).execute(&pool).await.unwrap();
        }
        pool
    }

    /// `w_%05d` works for `lo..hi`, generated in SQL so a 10k-row fixture is one
    /// statement rather than 10k round trips.
    async fn seed_works(pool: &SqlitePool, lo: i64, hi: i64) {
        sqlx::query(
            "INSERT INTO work (id, primary_title, created_at, updated_at) \
             WITH RECURSIVE n(i) AS (SELECT ? UNION ALL SELECT i + 1 FROM n WHERE i < ?) \
             SELECT printf('w_%05d', i), 'T', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z' \
             FROM n",
        )
        .bind(lo)
        .bind(hi - 1)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_work_blobs(covers: &SqlitePool, lo: i64, hi: i64, updated_at: &str) {
        sqlx::query(
            "INSERT INTO work_cover_blob (work_id, webp, version, updated_at) \
             WITH RECURSIVE n(i) AS (SELECT ? UNION ALL SELECT i + 1 FROM n WHERE i < ?) \
             SELECT printf('w_%05d', i), x'00', 1, ? FROM n",
        )
        .bind(lo)
        .bind(hi - 1)
        .bind(updated_at)
        .execute(covers)
        .await
        .unwrap();
    }

    async fn blob_count(covers: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM work_cover_blob")
            .fetch_one(covers)
            .await
            .unwrap()
    }

    fn cutoff() -> String {
        (chrono::Utc::now() - chrono::Duration::hours(COVER_ORPHAN_MIN_AGE_HOURS)).to_rfc3339()
    }

    fn never_shutdown() -> (
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Receiver<bool>,
    ) {
        tokio::sync::watch::channel(false)
    }

    /// The decisive safety property: an aged orphan is reclaimed, and NOTHING else is.
    /// A live work's blob and an orphan written inside `COVER_ORPHAN_MIN_AGE_HOURS`
    /// both survive — the latter is the exact shape `serve_cover` leaves behind when a
    /// work is deleted during its fetch+resize window (checked live → fetch → write).
    #[tokio::test]
    async fn work_sweep_reclaims_only_aged_orphans() {
        let (pool, covers) = (main_pool().await, covers_pool().await);
        // 10,000 live works, all with blobs (clears COVER_SWEEP_MIN_WORKS).
        seed_works(&pool, 0, 10_000).await;
        seed_work_blobs(&covers, 0, 10_000, "2020-01-01T00:00:00+00:00").await;
        // 100 aged orphans + 10 orphans written just now.
        seed_work_blobs(&covers, 90_000, 90_100, "2020-01-01T00:00:00+00:00").await;
        seed_work_blobs(&covers, 91_000, 91_010, &chrono::Utc::now().to_rfc3339()).await;
        assert_eq!(blob_count(&covers).await, 10_110);

        let (_tx, rx) = never_shutdown();
        let deleted = sweep_work_cover_blobs(&pool, &covers, &cutoff(), &rx)
            .await
            .unwrap();
        assert_eq!(deleted, 100, "exactly the aged orphans");
        assert_eq!(blob_count(&covers).await, 10_010);

        // Every live work still has its cover — the property whose violation would be
        // silent data loss.
        let live_kept: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work_cover_blob WHERE work_id < 'w_90000'")
                .fetch_one(&covers)
                .await
                .unwrap();
        assert_eq!(live_kept, 10_000, "no live work lost its cover");
        let fresh_kept: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM work_cover_blob WHERE work_id >= 'w_91000'")
                .fetch_one(&covers)
                .await
                .unwrap();
        assert_eq!(
            fresh_kept, 10,
            "the 24h age gate protected the fresh orphans"
        );

        // Idempotent: a second pass finds nothing left to do.
        assert_eq!(
            sweep_work_cover_blobs(&pool, &covers, &cutoff(), &rx)
                .await
                .unwrap(),
            0
        );
    }

    /// The DR floor: a main DB with implausibly few works must delete NOTHING, even
    /// though every single blob probes as orphaned.
    #[tokio::test]
    async fn work_sweep_refuses_when_the_main_db_looks_empty() {
        let (pool, covers) = (main_pool().await, covers_pool().await);
        seed_works(&pool, 0, 50).await;
        seed_work_blobs(&covers, 0, 3_000, "2020-01-01T00:00:00+00:00").await;
        let (_tx, rx) = never_shutdown();
        assert_eq!(
            sweep_work_cover_blobs(&pool, &covers, &cutoff(), &rx)
                .await
                .unwrap(),
            0
        );
        assert_eq!(blob_count(&covers).await, 3_000, "nothing was deleted");
    }

    /// The gap the row-count floor leaves open: a main DB that is POPULATED but WRONG
    /// (a staging copy, a rolled-back restore, a swapped `DATABASE_URL`) sails past the
    /// 10,000-work floor while every blob probes as orphaned. The orphan-rate breaker
    /// is what stops the 20 GB cache being eaten 1,000 rows an hour.
    #[tokio::test]
    async fn work_sweep_refuses_an_implausible_orphan_rate() {
        let (pool, covers) = (main_pool().await, covers_pool().await);
        // 10,000 works — floor satisfied — but a disjoint id range from the blobs.
        seed_works(&pool, 10_000, 20_000).await;
        seed_work_blobs(&covers, 0, 3_000, "2020-01-01T00:00:00+00:00").await;
        let (_tx, rx) = never_shutdown();
        assert_eq!(
            sweep_work_cover_blobs(&pool, &covers, &cutoff(), &rx)
                .await
                .unwrap(),
            0,
            "100% orphan rate must abort the sweep, not drive it"
        );
        assert_eq!(blob_count(&covers).await, 3_000, "nothing was deleted");
    }

    /// A run is capped, and the cap does not lose ground: the next run resumes.
    #[tokio::test]
    async fn work_sweep_honours_the_per_run_delete_budget() {
        let (pool, covers) = (main_pool().await, covers_pool().await);
        seed_works(&pool, 0, 10_000).await;
        seed_work_blobs(&covers, 0, 10_000, "2020-01-01T00:00:00+00:00").await;
        // 1,500 aged orphans — more than one run's budget.
        seed_work_blobs(&covers, 90_000, 91_500, "2020-01-01T00:00:00+00:00").await;
        let (_tx, rx) = never_shutdown();

        let first = sweep_work_cover_blobs(&pool, &covers, &cutoff(), &rx)
            .await
            .unwrap();
        assert!(
            (COVER_DELETE_BUDGET as u64..COVER_DELETE_BUDGET as u64 + COVER_SCAN_BATCH as u64)
                .contains(&first),
            "one run stops at the budget (got {first})"
        );
        let second = sweep_work_cover_blobs(&pool, &covers, &cutoff(), &rx)
            .await
            .unwrap();
        assert_eq!(first + second, 1_500, "the next run finishes the backlog");
        assert_eq!(blob_count(&covers).await, 10_000, "live blobs untouched");
    }

    /// Suwayomi blobs use the same machinery against `suwayomi_series`.
    #[tokio::test]
    async fn suwayomi_sweep_reclaims_only_aged_orphans() {
        let (pool, covers) = (main_pool().await, covers_pool().await);
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, status, source_id, updated_at) \
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000) \
             SELECT i, 'T', 'ONGOING', 'src', '2026-01-01T00:00:00Z' FROM n",
        )
        .execute(&pool)
        .await
        .unwrap();
        // 100 blobs for live series — a realistic ratio, so the orphan-rate breaker
        // (which would otherwise reject a fixture that is half orphans) stays out of
        // the way. Production runs at 7.2%.
        sqlx::query(
            "INSERT INTO suwayomi_cover_blob (manga_id, webp, updated_at) \
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100) \
             SELECT i, x'00', '2020-01-01T00:00:00+00:00' FROM n",
        )
        .execute(&covers)
        .await
        .unwrap();
        for (id, at) in [
            (90_001_i64, "2020-01-01T00:00:00+00:00".to_string()), // aged orphan
            (90_002, chrono::Utc::now().to_rfc3339()),             // fresh orphan
        ] {
            sqlx::query(
                "INSERT INTO suwayomi_cover_blob (manga_id, webp, updated_at) VALUES (?, x'00', ?)",
            )
            .bind(id)
            .bind(at)
            .execute(&covers)
            .await
            .unwrap();
        }

        let (_tx, rx) = never_shutdown();
        assert_eq!(
            sweep_suwayomi_cover_blobs(&pool, &covers, &cutoff(), &rx)
                .await
                .unwrap(),
            1,
            "only the aged orphan"
        );
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM suwayomi_cover_blob")
            .fetch_one(&covers)
            .await
            .unwrap();
        assert_eq!(
            left, 101,
            "100 live + the fresh orphan the age gate protected"
        );
        let gone: Option<i64> =
            sqlx::query_scalar("SELECT manga_id FROM suwayomi_cover_blob WHERE manga_id = 90001")
                .fetch_optional(&covers)
                .await
                .unwrap();
        assert!(gone.is_none());
    }

    /// The suwayomi floor, mirroring the work one.
    #[tokio::test]
    async fn suwayomi_sweep_refuses_when_the_main_db_looks_empty() {
        let (pool, covers) = (main_pool().await, covers_pool().await);
        sqlx::query(
            "INSERT INTO suwayomi_cover_blob (manga_id, webp, updated_at) \
             VALUES (7, x'00', '2020-01-01T00:00:00+00:00')",
        )
        .execute(&covers)
        .await
        .unwrap();
        let (_tx, rx) = never_shutdown();
        assert_eq!(
            sweep_suwayomi_cover_blobs(&pool, &covers, &cutoff(), &rx)
                .await
                .unwrap(),
            0
        );
    }

    #[test]
    fn orphan_rate_breaker_thresholds() {
        // An empty store is not a red flag — there is nothing to delete either way.
        assert!(orphan_rate_sane(0, 0));
        // Production today: 155 of a 2,000-id sample (7.75%).
        assert!(orphan_rate_sane(155, 2_000));
        // Exactly at the threshold is still allowed; one past it is not.
        assert!(orphan_rate_sane(500, 2_000));
        assert!(!orphan_rate_sane(501, 2_000));
        // The misconfiguration this exists for.
        assert!(!orphan_rate_sane(2_000, 2_000));
    }

    /// `IN ()` is a SQLite syntax error; neither probe may ever build one.
    #[tokio::test]
    async fn empty_probes_are_not_sent_to_sqlite() {
        let pool = main_pool().await;
        assert!(missing_work_ids(&pool, &[]).await.unwrap().is_empty());
        assert!(missing_series_ids(&pool, &[]).await.unwrap().is_empty());
    }

    /// The age gate is a STRING comparison against a `to_rfc3339()` cutoff, and chrono
    /// emits 0/3/6/9 fractional digits depending on the value — so the two sides can
    /// differ in width. Ordering still has to be chronological, or the gate silently
    /// fails open. It holds because '+' (0x2B) sorts below both '.' (0x2E) and every
    /// digit, so a shorter fraction always compares as the earlier instant.
    #[test]
    fn rfc3339_string_ordering_matches_chronological_ordering() {
        use chrono::{DateTime, Utc};
        let stamps = [
            "2026-07-20T14:28:07.629549225+00:00",
            "2026-07-20T14:28:07.629549+00:00",
            "2026-07-20T14:28:07.629+00:00",
            "2026-07-20T14:28:07+00:00",
            "2026-07-20T14:28:08+00:00",
            "2026-07-21T00:00:00+00:00",
        ];
        for a in stamps {
            for b in stamps {
                let (ta, tb) = (
                    DateTime::parse_from_rfc3339(a).unwrap().with_timezone(&Utc),
                    DateTime::parse_from_rfc3339(b).unwrap().with_timezone(&Utc),
                );
                assert_eq!(
                    a.cmp(b),
                    ta.cmp(&tb),
                    "string order must match time order for {a} vs {b}"
                );
            }
        }
        // And the shape the cutoff itself takes round-trips through the same ordering.
        let cutoff =
            (Utc::now() - chrono::Duration::hours(COVER_ORPHAN_MIN_AGE_HOURS)).to_rfc3339();
        assert!(
            stamps[0] < cutoff.as_str(),
            "a 2026-07-20 blob is past a now-24h cutoff"
        );
        assert!(
            Utc::now().to_rfc3339() > cutoff,
            "a just-written blob is not"
        );
    }
}
