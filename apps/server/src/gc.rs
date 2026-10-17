//! Background garbage collection of orphaned staged uploads and cover files.
//!
//! `POST /comment-media` inserts a `comment_media` row with `comment_id = NULL`
//! (a staged draft owned by the uploader); `postComment` later links it to the new
//! comment. A draft the user never posts stays NULL forever. Migration 0023 always
//! intended these to be "garbage-collected by age" — this task is that GC. It runs
//! hourly and deletes staged rows older than [`STAGED_TTL_HOURS`], bounding
//! unbounded main-database growth (and the backup volume it drives).
//!
//! The same loop reclaims cover files and repairs missing objects against the
//! PostgreSQL index — see [`sweep_cover_store`].

use std::time::Duration;

use crate::cover_store::{self, CoverStore};
use crate::db::Database;

/// Staged (unlinked) uploads older than this are swept. A user posting a comment
/// always links its image within seconds, so a day is a generous grace period for
/// slow multi-step composes without letting abandoned drafts accumulate.
const STAGED_TTL_HOURS: i64 = 24;

/// Keep unread notifications indefinitely. Read notifications expire by creation
/// time after 90 days, with bounded writer turns and bounded work per hourly tick.
const READ_NOTIFICATION_TTL_DAYS: i64 = 90;
const NOTIFICATION_DELETE_BATCH: u64 = 128;
const NOTIFICATION_DELETE_BUDGET: u64 = 1024;

/// How often the sweep runs.
const SWEEP_INTERVAL_SECS: u64 = 3600;

/// Staged comment media past the TTL (T095). The rows are chosen and locked by the
/// subquery in primary-key order, and a row another transaction holds is skipped, not
/// waited for: `postComment` locks a staged row while it links it to a comment, and a
/// sweep that waited there could close a lock cycle with it. A skipped row is still
/// staged and still old at the next hourly sweep, or it has been linked and is no
/// longer a candidate. `$1` is the cutoff.
const STAGED_MEDIA_SWEEP_SQL: &str = "DELETE FROM comment_media WHERE id IN \
     (SELECT id FROM comment_media WHERE comment_id IS NULL AND created_at < $1 \
      ORDER BY id FOR UPDATE SKIP LOCKED)";

/// One batch of read notifications past the TTL (T096).
///
/// The batch is CHOSEN oldest first through the partial index
/// `idx_notifications_read_created` (innermost query, no lock, no sort). Its rows are
/// then LOCKED in primary-key order, the order `markNotificationsRead` and
/// `deleteComment` lock notifications in, and a row another transaction holds is
/// skipped. The prune therefore never waits for a row lock while it holds one, so it
/// cannot be part of a lock cycle with `DELETE FROM notifications WHERE comment_id = $1`
/// or with a mark-read; before this it could be, and one side then failed with 40P01.
/// The DELETE only touches rows this statement has already locked. A skipped row is
/// pruned by a later batch or the next hourly sweep. `$1` is the cutoff, `$2` the batch.
const READ_NOTIFICATION_PRUNE_SQL: &str = "DELETE FROM notifications WHERE id IN \
     (SELECT id FROM notifications \
       WHERE id = ANY(ARRAY( \
               SELECT id FROM notifications \
                WHERE read_at IS NOT NULL AND created_at < $1 \
                ORDER BY created_at NULLS FIRST, id NULLS FIRST LIMIT $2)) \
         AND read_at IS NOT NULL AND created_at < $1 \
       ORDER BY id FOR UPDATE SKIP LOCKED)";

/// Spawn the recurring GC sweep. Runs until `shutdown` resolves.
pub fn spawn(pool: Database, covers: CoverStore, shutdown: tokio::sync::watch::Receiver<bool>) {
    // Panic-supervised (crate::task::supervise): a panic in `sweep` used to end the GC
    // job silently for the process lifetime, letting the staged-media BLOB store grow
    // unbounded. `factory` re-clones the pools + shutdown handle on each restart.
    tokio::spawn(crate::task::supervise(
        "gc",
        Duration::from_secs(30),
        shutdown.clone(),
        move || {
            crate::db::Database::job_scope(
                crate::database::ParallelJob::Gc,
                run_loop(pool.clone(), covers.clone(), shutdown.clone()),
            )
        },
    ));
}

async fn run_loop(
    pool: Database,
    covers: CoverStore,
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
    let mut repair_cursor = CoverRepairCursor::default();
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                sweep(&pool, &shutdown).await;
                sweep_cover_store(&pool, &covers, &shutdown, &mut repair_cursor).await;
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
async fn sweep(pool: &Database, shutdown: &tokio::sync::watch::Receiver<bool>) {
    // created_at is written as `chrono::Utc::now().to_rfc3339()`, so a UTC RFC-3339
    // cutoff compares correctly against it lexically.
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(STAGED_TTL_HOURS)).to_rfc3339();
    match sweep_staged_media(pool, &cutoff).await {
        Ok(deleted) if deleted > 0 => {
            tracing::info!(deleted, "gc: swept orphaned comment media");
        }
        Ok(_) => {}
        Err(e) => {
            crate::db::record_database_error(&e);
            tracing::warn!(error = %e, "gc: comment-media sweep failed");
        }
    }

    // Prune view buckets past the retention window so `series_view_bucket` stays bounded
    // (the all-time totals in `series_views` are kept). Best-effort like the sweep above.
    match crate::views::prune_buckets(pool).await {
        Ok(n) if n > 0 => tracing::info!(deleted = n, "gc: pruned old view buckets"),
        Ok(_) => {}
        Err(e) => {
            let e = crate::db::observed_database_error(e);
            tracing::warn!(error = %e, "gc: view-bucket prune failed");
        }
    }
    // Same for the per-source request health buckets (migration 0110).
    match crate::source_health::prune_buckets(pool).await {
        Ok(n) if n > 0 => tracing::info!(deleted = n, "gc: pruned old source health buckets"),
        Ok(_) => {}
        Err(e) => {
            let e = crate::db::observed_database_error(e);
            tracing::warn!(error = %e, "gc: source-health prune failed");
        }
    }
    let notification_cutoff =
        (chrono::Utc::now() - chrono::Duration::days(READ_NOTIFICATION_TTL_DAYS)).to_rfc3339();
    match prune_read_notifications(pool, &notification_cutoff, shutdown).await {
        Ok(n) if n > 0 => tracing::info!(deleted = n, "gc: pruned old read notifications"),
        Ok(_) => {}
        Err(e) => {
            let e = crate::db::observed_database_error(e);
            tracing::warn!(error = %e, "gc: notification prune failed");
        }
    }
}

/// One statement, one implicit transaction: delete the staged media older than `cutoff`
/// that no other transaction holds (see [`STAGED_MEDIA_SWEEP_SQL`]).
async fn sweep_staged_media(pool: &Database, cutoff: &str) -> Result<u64, sqlx::Error> {
    #[cfg(test)]
    let mut tx = pool.begin().await?;
    let query = sqlx::query(STAGED_MEDIA_SWEEP_SQL)
        .bind(crate::db_types::date(cutoff).expect("internally generated UTC timestamp"));
    #[cfg(not(test))]
    let result = query.execute(pool).await?;
    #[cfg(test)]
    let result = query.execute(&mut *tx).await?;
    #[cfg(test)]
    {
        crate::catalog::guards::held().await;
        tx.commit().await?;
    }
    Ok(result.rows_affected())
}

pub(crate) async fn prune_read_notifications(
    pool: &Database,
    cutoff: &str,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<u64> {
    let mut deleted = 0;
    while deleted < NOTIFICATION_DELETE_BUDGET {
        if *shutdown.borrow() {
            break;
        }
        let batch = crate::db::retry_transaction(|| async {
            if *shutdown.borrow() {
                return Ok(0);
            }
            let mut tx = pool.begin().await?;
            // Shutdown may have started while waiting for the single writer.
            if *shutdown.borrow() {
                tx.rollback().await?;
                return Ok(0);
            }
            let result = sqlx::query(READ_NOTIFICATION_PRUNE_SQL)
                .bind(crate::db_types::date(cutoff)?)
                .bind(NOTIFICATION_DELETE_BATCH as i64)
                .execute(&mut *tx)
                .await?;
            #[cfg(test)]
            crate::catalog::guards::held().await;
            tx.commit().await?;
            Ok(result.rows_affected())
        })
        .await?;
        deleted += batch;
        if batch < NOTIFICATION_DELETE_BATCH {
            break;
        }
        tokio::task::yield_now().await;
    }
    Ok(deleted)
}

// ---------------------------------------------------------------------------------
// Cover files and their PostgreSQL index
// ---------------------------------------------------------------------------------

const COVER_SCAN_BATCH: usize = 500;
const COVER_SWEEP_BUDGET: usize = 1_000;
/// Index rows checked for a missing file per tick. A stat is cheap; at this rate a full
/// pass over a ~400k-row index takes about eight hourly ticks (requested covers are
/// repaired at once through the cover routes).
const COVER_REPAIR_SCAN_BUDGET: usize = 50_000;
/// Rows repaired per tick, apart from the deletion budget of the other sweeps: a lost
/// files volume is mostly repairs, and 1,000 an hour would take days.
const COVER_REPAIR_BUDGET: u64 = 10_000;
/// Row failures that end one repair pass (a systemic fault, not one bad row).
const COVER_REPAIR_MAX_FAILURES: u64 = 20;
/// Distinct indexed files sampled before a repair pass, and the fewest that make the
/// sample mean something (a tiny store is repaired without the check).
const COVER_PLAUSIBILITY_SAMPLE: usize = 256;
const COVER_PLAUSIBILITY_MIN_SAMPLE: usize = 32;
/// How long a plausibility answer stands for the read-path repair before it is checked
/// again (the GC pass always checks afresh and refreshes it).
const COVER_PLAUSIBILITY_TTL: Duration = Duration::from_secs(5 * 60);
/// Read-path repairs queued at once.
const READ_REPAIR_CAP: usize = 256;
const COVER_FILE_GRACE: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone, Default)]
struct CoverRepairCursor {
    asset: String,
    kind: String,
}

async fn sweep_cover_store(
    pool: &Database,
    covers: &CoverStore,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    repair_cursor: &mut CoverRepairCursor,
) {
    macro_rules! sweep_step {
        ($name:literal, $future:expr) => {
            if !*shutdown.borrow() {
                match $future.await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(count = n, concat!("gc: ", $name)),
                    Err(e) => {
                        let e = crate::db::observed_database_error(e);
                        tracing::warn!(error = %e, concat!("gc: ", $name, " failed"));
                    }
                }
            }
        };
    }
    sweep_step!("swept cover temp files", sweep_temp_files(covers, shutdown));
    sweep_step!(
        "swept unreferenced cover files",
        sweep_unreferenced_files(pool, covers, shutdown)
    );
    sweep_step!(
        "swept orphan source covers",
        sweep_orphan_source_covers(pool, shutdown)
    );
    sweep_step!(
        "repaired missing cover files",
        repair_missing_cover_files(pool, covers, shutdown, repair_cursor)
    );
}

async fn file_older_than(
    path: &std::path::Path,
    cutoff: std::time::SystemTime,
) -> std::io::Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => Ok(metadata.is_file() && metadata.modified()? < cutoff),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

async fn unlink_file(path: &std::path::Path) -> std::io::Result<bool> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

async fn sweep_temp_files(
    covers: &CoverStore,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<u64> {
    let cutoff = std::time::SystemTime::now() - COVER_FILE_GRACE;
    let mut entries = tokio::fs::read_dir(covers.tmp_dir()).await?;
    let mut deleted = 0;
    while !*shutdown.borrow() && deleted < COVER_SWEEP_BUDGET as u64 {
        let Some(entry) = entries.next_entry().await? else {
            break;
        };
        if file_older_than(&entry.path(), cutoff).await? && unlink_file(&entry.path()).await? {
            deleted += 1;
        }
    }
    Ok(deleted)
}

async fn sweep_unreferenced_files(
    pool: &Database,
    covers: &CoverStore,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<u64> {
    let cutoff = std::time::SystemTime::now() - COVER_FILE_GRACE;
    let mut directories = tokio::fs::read_dir(covers.objects_dir()).await?;
    let mut batch = Vec::with_capacity(COVER_SCAN_BATCH);
    let mut deleted = 0;
    while !*shutdown.borrow() && deleted < COVER_SWEEP_BUDGET as u64 {
        let Some(directory) = directories.next_entry().await? else {
            break;
        };
        let prefix = directory.file_name();
        let Some(prefix) = prefix
            .to_str()
            .filter(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        else {
            continue;
        };
        if !directory.file_type().await?.is_dir() {
            continue;
        }
        let mut entries = tokio::fs::read_dir(directory.path()).await?;
        while !*shutdown.borrow() && deleted < COVER_SWEEP_BUDGET as u64 {
            let Some(entry) = entries.next_entry().await? else {
                break;
            };
            let name = entry.file_name();
            let Some(name) = name
                .to_str()
                .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            else {
                continue;
            };
            if !name[..2].eq_ignore_ascii_case(prefix)
                || !file_older_than(&entry.path(), cutoff).await?
            {
                continue;
            }
            let mut sha = [0; 32];
            hex::decode_to_slice(name, &mut sha)?;
            batch.push((sha, entry.path()));
            if batch.len() == COVER_SCAN_BATCH {
                deleted += sweep_unreferenced_batch(
                    pool,
                    &batch,
                    cutoff,
                    COVER_SWEEP_BUDGET - deleted as usize,
                    shutdown,
                )
                .await?;
                batch.clear();
                tokio::task::yield_now().await;
            }
        }
    }
    if !batch.is_empty() && !*shutdown.borrow() && deleted < COVER_SWEEP_BUDGET as u64 {
        deleted += sweep_unreferenced_batch(
            pool,
            &batch,
            cutoff,
            COVER_SWEEP_BUDGET - deleted as usize,
            shutdown,
        )
        .await?;
    }
    Ok(deleted)
}

async fn sweep_unreferenced_batch(
    pool: &Database,
    batch: &[(cover_store::Sha, std::path::PathBuf)],
    cutoff: std::time::SystemTime,
    budget: usize,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<u64> {
    let shas: Vec<Vec<u8>> = batch.iter().map(|(sha, _)| sha.to_vec()).collect();
    let referenced: std::collections::HashSet<Vec<u8>> =
        sqlx::query_scalar("SELECT sha256 FROM cover_object WHERE sha256 = ANY($1)")
            .bind(&shas)
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();
    let mut deleted = 0;
    for (sha, path) in batch {
        if *shutdown.borrow() || deleted >= budget as u64 {
            break;
        }
        if referenced.contains(sha.as_slice()) {
            continue;
        }
        let mut tx = pool.begin().await?;
        crate::locks::advisory(&mut tx, &[cover_store::file_key(sha)]).await?;
        let referenced: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cover_object WHERE sha256 = $1)")
                .bind(sha.as_slice())
                .fetch_one(&mut *tx)
                .await?;
        // A publisher may have touched this file and rolled back while GC waited.
        if !referenced && file_older_than(path, cutoff).await? && unlink_file(path).await? {
            deleted += 1;
        }
        tx.commit().await?;
    }
    Ok(deleted)
}

async fn sweep_orphan_source_covers(
    pool: &Database,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<u64> {
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(24);
    let mut cursor = String::new();
    let mut deleted = 0;
    while !*shutdown.borrow() && deleted < COVER_SWEEP_BUDGET as u64 {
        let assets: Vec<String> = sqlx::query_scalar(
            "SELECT c.asset_key FROM cover_object c WHERE c.asset_key > $1 \
             AND c.asset_key LIKE 's:%' AND c.kind = 'master' AND c.updated_at < $2 \
             AND NOT EXISTS (SELECT 1 FROM suwayomi_series s WHERE 's:' || s.id::text = c.asset_key) \
             ORDER BY c.asset_key LIMIT $3",
        ).bind(&cursor).bind(cutoff).bind(COVER_SCAN_BATCH as i64).fetch_all(pool).await?;
        if assets.is_empty() {
            break;
        }
        for asset in assets {
            if *shutdown.borrow() || deleted >= COVER_SWEEP_BUDGET as u64 {
                break;
            }
            cursor.clone_from(&asset);
            let Some(id) = asset
                .strip_prefix("s:")
                .and_then(|id| id.parse::<i64>().ok())
            else {
                continue;
            };
            let mut tx = pool.begin().await?;
            crate::engine_generation::shared(&mut tx, None).await?;
            let _asset = crate::cover::lock_asset(&mut tx, &asset).await?;
            let orphan: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM cover_object WHERE asset_key = $1 \
                 AND kind = 'master' AND updated_at < $2) \
                 AND NOT EXISTS(SELECT 1 FROM suwayomi_series WHERE id = $3)",
            )
            .bind(&asset)
            .bind(cutoff)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            if orphan {
                cover_store::delete_asset(&mut tx, &asset).await?;
                deleted += 1;
            }
            tx.commit().await?;
        }
        tokio::task::yield_now().await;
    }
    Ok(deleted)
}

async fn repair_missing_cover_files(
    pool: &Database,
    covers: &CoverStore,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    cursor: &mut CoverRepairCursor,
) -> anyhow::Result<u64> {
    // While the SQLite copy runs, a cleared slot would let the copy install an older
    // SQLite master over a newer cover whose file was lost. Reads still refill such a
    // slot on demand; the repair waits until the copy is done.
    if !covers.copy_done() {
        return Ok(0);
    }
    if !store_plausible_cached(pool, covers, Duration::ZERO).await? {
        return Ok(0);
    }
    let mut scanned = 0;
    let mut repaired = 0;
    let mut failed = 0;
    while !*shutdown.borrow()
        && scanned < COVER_REPAIR_SCAN_BUDGET
        && repaired < COVER_REPAIR_BUDGET
    {
        let rows: Vec<(String, String, Vec<u8>)> = sqlx::query_as(
            "SELECT asset_key, kind, sha256 FROM cover_object WHERE (asset_key, kind) > ($1, $2) \
             ORDER BY asset_key, kind LIMIT $3",
        )
        .bind(&cursor.asset)
        .bind(&cursor.kind)
        .bind(COVER_SCAN_BATCH.min(COVER_REPAIR_SCAN_BUDGET - scanned) as i64)
        .fetch_all(pool)
        .await?;
        if rows.is_empty() {
            *cursor = CoverRepairCursor::default();
            break;
        }
        for (key, kind, raw_sha) in rows {
            if *shutdown.borrow() || repaired >= COVER_REPAIR_BUDGET {
                break;
            }
            // One failing row (an unreadable directory, upload bytes that no longer match
            // their hash, a quarantined generation) is logged and passed, so the rows after
            // it are still repaired. Many failures in one pass stop it: that is not a row.
            let result = match cover_store::Sha::try_from(raw_sha.as_slice()) {
                Ok(sha) => repair_lost_object(pool, covers, &key, &kind, sha, true).await,
                Err(_) => Err(anyhow::anyhow!("cover index row has a malformed hash")),
            };
            cursor.asset = key;
            cursor.kind = kind;
            scanned += 1;
            match result {
                Ok(true) => repaired += 1,
                Ok(false) => {}
                Err(error) => {
                    failed += 1;
                    tracing::warn!(
                        asset = %cursor.asset,
                        kind = %cursor.kind,
                        error = %format!("{error:#}"),
                        "gc: missing cover file repair failed for one row"
                    );
                    if failed >= COVER_REPAIR_MAX_FAILURES {
                        return Err(error.context("gc: too many cover repair failures"));
                    }
                }
            }
        }
        tokio::task::yield_now().await;
    }
    Ok(repaired)
}

/// Whether the cover store looks like the one the index describes. An empty, wrong or
/// unmounted `KOMIKA_COVERS_DIR` makes every file look lost; repairing then would clear
/// the whole index and send every cover back to its source. A uniform sample of indexed
/// files (by content hash) that is mostly missing or unreadable stops the pass. Covers
/// that are viewed still refill one at a time through the cover routes.
async fn store_plausible(pool: &Database, covers: &CoverStore) -> anyhow::Result<bool> {
    let sample: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT DISTINCT sha256 FROM cover_object ORDER BY sha256 LIMIT $1")
            .bind(COVER_PLAUSIBILITY_SAMPLE as i64)
            .fetch_all(pool)
            .await?;
    if sample.len() < COVER_PLAUSIBILITY_MIN_SAMPLE {
        return Ok(true);
    }
    let mut missing = 0;
    for raw in &sample {
        let Ok(sha) = cover_store::Sha::try_from(raw.as_slice()) else {
            continue;
        };
        if !covers.object_exists(&sha).await.unwrap_or(false) {
            missing += 1;
        }
    }
    if missing * 2 > sample.len() {
        tracing::error!(
            dir = %covers.dir().display(),
            sampled = sample.len(),
            missing,
            "gc: most indexed cover files are missing or unreadable; is KOMIKA_COVERS_DIR \
             the right directory? skipping the missing-file repair. If the files are really \
             gone (a restore without cover files), run `komika-server covers repair-missing` once"
        );
        return Ok(false);
    }
    Ok(true)
}

/// [`store_plausible`], reusing an answer younger than `max_age` (zero: always check).
/// The answer is kept on the store, so the GC pass and the read-path repair share it and
/// a burst of cover misses runs one check, not one each.
async fn store_plausible_cached(
    pool: &Database,
    covers: &CoverStore,
    max_age: Duration,
) -> anyhow::Result<bool> {
    let mut cached = covers.plausibility().lock().await;
    if let Some((at, plausible)) = *cached {
        if !max_age.is_zero() && at.elapsed() < max_age {
            return Ok(plausible);
        }
    }
    let plausible = store_plausible(pool, covers).await?;
    *cached = Some((tokio::time::Instant::now(), plausible));
    Ok(plausible)
}

/// Repair one index row whose file is missing: rewrite an uploaded master from
/// `cover_upload`; otherwise, when `drop_rows`, drop the row (a master takes its derivatives and the work's
/// cover pointer and origin with it, so the cover crawl refills it from its source).
/// Rechecked under the publish locks; `false` when the row changed or the file is there.
pub(crate) async fn repair_lost_object(
    pool: &Database,
    covers: &CoverStore,
    key: &str,
    kind: &str,
    sha: cover_store::Sha,
    drop_rows: bool,
) -> anyhow::Result<bool> {
    if covers.object_exists(&sha).await? {
        return Ok(false);
    }
    let asset = cover_store::asset_of(key);
    let work_id = key.strip_prefix("w:");
    let upload: Option<Vec<u8>> = if kind == "master" && work_id.is_some() {
        sqlx::query_scalar("SELECT bytes FROM cover_upload WHERE work_id = $1 AND sha256 = $2")
            .bind(work_id)
            .bind(sha.as_slice())
            .fetch_optional(pool)
            .await?
    } else {
        None
    };
    // Hash and sync the backup bytes before any publication locks.
    let mut staged = match upload {
        Some(bytes) => {
            let staged = covers.stage(&bytes).await?;
            anyhow::ensure!(
                staged.sha == sha,
                "cover upload bytes disagree with their hash"
            );
            Some(staged)
        }
        None => None,
    };
    if staged.is_none() && !drop_rows {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    if asset.starts_with("s:") {
        // A quarantined generation leaves numeric rows for a later pass, not as failures
        // that would stop the pass before the work covers after them.
        match crate::engine_generation::shared(&mut tx, None).await {
            Ok(_) => {}
            Err(e) if e.is::<crate::engine_generation::FenceError>() => {
                tx.rollback().await?;
                return Ok(false);
            }
            Err(e) => return Err(e),
        }
    }
    let _assets = crate::cover::lock_assets_and_files(&mut tx, &[asset], &[sha]).await?;
    if let Some(work_id) = work_id {
        crate::locks::lock_works(&mut tx, &[work_id]).await?;
    }
    let current: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM cover_object WHERE asset_key = $1 AND kind = $2 AND sha256 = $3)",
    )
    .bind(key)
    .bind(kind)
    .bind(sha.as_slice())
    .fetch_one(&mut *tx)
    .await?;
    let mut repaired = false;
    if current && !covers.object_exists(&sha).await? {
        if let Some(staged) = staged.as_mut() {
            covers.place(staged).await?;
        } else if kind == "master" {
            cover_store::delete_asset(&mut tx, asset).await?;
            if let Some(work_id) = work_id {
                // The origin goes with the master: a later copy is of unknown origin.
                sqlx::query(
                    "UPDATE work SET cover_cached_version = NULL, cover_source = NULL \
                     WHERE id = $1 \
                       AND (cover_cached_version IS NOT NULL OR cover_source IS NOT NULL)",
                )
                .bind(work_id)
                .execute(&mut *tx)
                .await?;
            }
        } else {
            sqlx::query(
                "DELETE FROM cover_object WHERE asset_key = $1 AND kind = $2 AND sha256 = $3",
            )
            .bind(key)
            .bind(kind)
            .bind(sha.as_slice())
            .execute(&mut *tx)
            .await?;
        }
        repaired = true;
    }
    tx.commit().await?;
    Ok(repaired)
}

/// What `komika-server covers repair-missing` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RepairAll {
    pub checked: u64,
    pub repaired: u64,
    pub failed: u64,
}

/// `komika-server covers repair-missing`: refuse a covers path that does not exist or is
/// not a directory. That is a wrong path, not lost files, and opening the store would
/// create it.
pub(crate) fn check_covers_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => anyhow::bail!(
            "KOMIKA_COVERS_DIR {} is not a directory; nothing was repaired",
            dir.display()
        ),
        Err(error) => anyhow::bail!(
            "KOMIKA_COVERS_DIR {} cannot be read ({error}); nothing was repaired",
            dir.display()
        ),
    }
}

/// The missing-file repair once over the whole index, WITHOUT [`store_plausible`]: the
/// operator's answer after a restore without cover files, when most files are really
/// gone and the guard would refuse forever. Rows are read in batches; each missing file's
/// row is repaired as the GC pass does (an upload is restored, anything else is dropped
/// so the cover crawl refills it). A failing row is logged and passed.
pub(crate) async fn repair_all_missing_cover_files(
    pool: &Database,
    covers: &CoverStore,
) -> anyhow::Result<RepairAll> {
    anyhow::ensure!(
        covers.copy_done(),
        "the old cover cache copy is still running; repair after the log says `cover copy finished`"
    );
    let mut done = RepairAll::default();
    let mut cursor = CoverRepairCursor::default();
    loop {
        let rows: Vec<(String, String, Vec<u8>)> = sqlx::query_as(
            "SELECT asset_key, kind, sha256 FROM cover_object WHERE (asset_key, kind) > ($1, $2) \
             ORDER BY asset_key, kind LIMIT $3",
        )
        .bind(&cursor.asset)
        .bind(&cursor.kind)
        .bind(COVER_SCAN_BATCH as i64)
        .fetch_all(pool)
        .await?;
        let Some((last_asset, last_kind, _)) = rows.last().cloned() else {
            break;
        };
        for (key, kind, raw_sha) in rows {
            done.checked += 1;
            let result = match cover_store::Sha::try_from(raw_sha.as_slice()) {
                Ok(sha) => repair_lost_object(pool, covers, &key, &kind, sha, true).await,
                Err(_) => Err(anyhow::anyhow!("cover index row has a malformed hash")),
            };
            match result {
                Ok(true) => done.repaired += 1,
                Ok(false) => {}
                Err(error) => {
                    done.failed += 1;
                    tracing::warn!(asset = %key, %kind, error = %format!("{error:#}"), "covers repair-missing: row failed");
                }
            }
        }
        cursor = CoverRepairCursor {
            asset: last_asset,
            kind: last_kind,
        };
    }
    Ok(done)
}

/// Queue the repair of a work master whose file a cover read found missing, so the cover
/// crawl refills it from its source without waiting for the GC pass to reach the row.
/// One queued repair per work at a time and at most [`READ_REPAIR_CAP`] in flight; a
/// miss over the cap is left to the GC pass. Nothing is queued while the SQLite copy runs
/// (see [`repair_missing_cover_files`]); see [`repair_lost_master_on_read`] for the guard.
pub(crate) fn defer_lost_master_repair(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
    sha: cover_store::Sha,
) -> bool {
    static IN_FLIGHT: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(Default::default);
    struct Slot(String);
    impl Drop for Slot {
        fn drop(&mut self) {
            IN_FLIGHT
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&self.0);
        }
    }
    if !covers.copy_done() || work_id.starts_with("s:") || work_id.starts_with("icon:") {
        return false;
    }
    {
        let mut in_flight = IN_FLIGHT.lock().unwrap_or_else(|p| p.into_inner());
        if in_flight.len() >= READ_REPAIR_CAP || !in_flight.insert(work_id.to_owned()) {
            return false;
        }
    }
    let slot = Slot(work_id.to_owned());
    let database = main.background();
    let covers = covers.clone();
    main.defer(async move {
        if let Err(error) = repair_lost_master_on_read(&database, &covers, &slot.0, sha).await {
            tracing::warn!(work_id = %slot.0, error = %format!("{error:#}"), "cover read repair failed");
        }
        Ok(())
    })
}

/// The read-path repair of one work master. It takes the GC pass's guard: while the store
/// looks wrong (most sampled files missing: a wrong, empty or unmounted
/// `KOMIKA_COVERS_DIR`, or files still being copied in), no index row or cover pointer is
/// dropped. An uploaded master is still restored from `cover_upload`.
async fn repair_lost_master_on_read(
    pool: &Database,
    covers: &CoverStore,
    work_id: &str,
    sha: cover_store::Sha,
) -> anyhow::Result<bool> {
    let plausible = store_plausible_cached(pool, covers, COVER_PLAUSIBILITY_TTL).await?;
    let key = cover_store::index_key(work_id);
    repair_lost_object(pool, covers, &key, "master", sha, plausible).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::TestPoolOptions;

    #[tokio::test]
    async fn notification_gc_keeps_unread_recent_boundary_and_release_history() {
        let pool = main_pool().await;
        sqlx::query("INSERT INTO users(id, username, email, password_hash, created_at) VALUES ('8834b774-e415-4d35-952e-299c9fb4381a', '8834b774-e415-4d35-952e-299c9fb4381a', 'gc@example.test', 'unused', '2026-01-01T00:00:00+00:00')")
            .execute(&pool).await.unwrap();
        let cutoff = "2026-07-10T12:00:00+00:00";
        for (id, created_at, read_at) in [
            (
                "old-read",
                "2026-07-09T12:00:00+00:00",
                Some("2026-10-08T12:00:00+00:00"),
            ),
            ("old-unread", "2020-01-01T00:00:00+00:00", None),
            ("recent-read", "2026-07-11T12:00:00+00:00", Some(cutoff)),
            ("boundary-read", cutoff, Some(cutoff)),
        ] {
            sqlx::query("INSERT INTO notifications(id, user_id, kind, created_at, read_at) VALUES ($1, '8834b774-e415-4d35-952e-299c9fb4381a', 'reply', $2, $3)")
                .bind(crate::db_types::test_uuid(id)).bind(crate::db_types::date(created_at).unwrap()).bind(crate::db_types::optional_date(read_at).unwrap()).execute(&pool).await.unwrap();
        }
        seed_works(&pool, 0, 1).await;
        sqlx::query("INSERT INTO release_event(work_id, chapter_key, label, first_seen_at) VALUES ('w_00000', '10', '1', TIMESTAMPTZ '1970-01-01 00:00:00.001+00')")
            .execute(&pool).await.unwrap();
        assert_eq!(
            prune_read_notifications(&pool, cutoff, &never_shutdown().1)
                .await
                .unwrap(),
            1
        );
        let kept: Vec<String> =
            crate::db_types::query_scalar("SELECT id FROM notifications ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let mut expected: Vec<String> = ["boundary-read", "old-unread", "recent-read"]
            .map(|id| crate::db_types::test_uuid(id).to_string())
            .to_vec();
        expected.sort();
        assert_eq!(kept, expected);
        let release: (String, String, String, i64) = crate::db_types::query_tuple(
            "SELECT work_id, chapter_key, label, first_seen_at FROM release_event",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(release, ("w_00000".into(), "10".into(), "1".into(), 1));
        assert_eq!(
            prune_read_notifications(&pool, cutoff, &never_shutdown().1)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn notification_gc_batches_uses_partial_index_and_resumes_backlog() {
        let pool = main_pool().await;
        sqlx::query("INSERT INTO users(id, username, email, password_hash, created_at) VALUES ('8834b774-e415-4d35-952e-299c9fb4381a', '8834b774-e415-4d35-952e-299c9fb4381a', 'gc@example.test', 'unused', '2026-01-01T00:00:00+00:00')")
            .execute(&pool).await.unwrap();
        sqlx::query("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<1400) INSERT INTO notifications(id,user_id,kind,created_at,read_at) SELECT md5('gc-old-' || i)::uuid, '8834b774-e415-4d35-952e-299c9fb4381a', 'reply', '2020-01-01T00:00:00+00:00', '2020-01-02T00:00:00+00:00' FROM n")
            .execute(&pool).await.unwrap();
        sqlx::query("ANALYZE notifications")
            .execute(&pool)
            .await
            .unwrap();
        let plans: Vec<String> = crate::db_types::query_scalar("EXPLAIN SELECT id FROM notifications WHERE read_at IS NOT NULL AND created_at < $1 ORDER BY created_at NULLS FIRST, id NULLS FIRST LIMIT 128")
            .bind(crate::db_types::date("2026-07-10T12:00:00+00:00").unwrap()).fetch_all(&pool).await.unwrap();
        assert!(
            plans
                .iter()
                .any(|row| row.contains("idx_notifications_read_created")),
            "{plans:?}"
        );
        assert!(!plans.iter().any(|row| row.contains("Sort")), "{plans:?}");
        assert_eq!(
            prune_read_notifications(&pool, "2026-07-10T12:00:00+00:00", &never_shutdown().1)
                .await
                .unwrap(),
            NOTIFICATION_DELETE_BUDGET
        );
        assert_eq!(
            prune_read_notifications(&pool, "2026-07-10T12:00:00+00:00", &never_shutdown().1)
                .await
                .unwrap(),
            376
        );
    }

    #[tokio::test]
    async fn notification_gc_rechecks_shutdown_after_waiting_for_writer() {
        let pool = main_pool().await;
        sqlx::query("INSERT INTO users(id, username, email, password_hash, created_at) VALUES ('8834b774-e415-4d35-952e-299c9fb4381a', '8834b774-e415-4d35-952e-299c9fb4381a', 'gc@example.test', 'unused', '2020-01-01T00:00:00+00:00')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO notifications(id,user_id,kind,created_at,read_at) VALUES ('102bf39e-f4ca-4574-b7bc-4afd5cb24482','8834b774-e415-4d35-952e-299c9fb4381a','reply','2020-01-01T00:00:00+00:00','2020-01-02T00:00:00+00:00')")
            .execute(&pool).await.unwrap();
        let writer = pool.begin().await.unwrap();
        let (signal, shutdown) = never_shutdown();
        let pruning_pool = pool.clone();
        let pruning = tokio::spawn(async move {
            prune_read_notifications(&pruning_pool, "2026-07-10T12:00:00+00:00", &shutdown).await
        });
        tokio::task::yield_now().await;
        signal.send(true).unwrap();
        writer.rollback().await.unwrap();
        assert_eq!(pruning.await.unwrap().unwrap(), 0);
        let remaining: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM notifications")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 1);
    }

    async fn social_gc_fixture(pool: &Database) -> (String, uuid::Uuid) {
        let user = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users(id,username,email,password_hash,created_at) VALUES ($1,$2,$3,'unused',now())")
            .bind(crate::db_types::uuid(&user).unwrap()).bind(&user).bind(format!("{user}@example.test"))
            .execute(pool).await.unwrap();
        let media = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO comment_media(id,user_id,webp,width,height,created_at) VALUES ($1,$2,decode('00','hex'),1,1,'2020-01-01')")
            .bind(media).bind(crate::db_types::uuid(&user).unwrap()).execute(pool).await.unwrap();
        (user, media)
    }

    async fn social_schema(
        pool: &Database,
        user: &str,
    ) -> (cover_store::test_support::TestDir, crate::graphql::ApiSchema) {
        use crate::graphql::{AppState, RateLimiter};
        use std::sync::Arc;

        sqlx::query("INSERT INTO sessions(token,user_id,created_at,expires_at) VALUES ($1,$2,now(),now()+interval '1 day')")
            .bind(crate::auth::hash_token("ov1-token")).bind(crate::db_types::uuid(user).unwrap())
            .execute(pool).await.unwrap();
        let cover_dir = cover_store::test_support::TestDir::new();
        let schema = crate::graphql::build_schema(
            Arc::new(AppState {
                pool: pool.foreground(),
                cover_pool: cover_store::test_support::store(&cover_dir),
                suwayomi: crate::suwayomi::SuwayomiClient::new(
                    "http://127.0.0.1:1".into(),
                    None,
                    None,
                ),
                mangadex: Arc::new(crate::mangadex::MangaDexClient::new("test-ua", 5.0, 40.0)),
                admin_users: vec![],
                scan_health: Default::default(),
                auth_limiter: RateLimiter::new(100, 60),
                comment_limiter: crate::graphql::comment_limiter(),
                rating_limiter: crate::graphql::rating_limiter(),
                password_work: Arc::new(tokio::sync::Semaphore::new(4)),
                federated_limiter: RateLimiter::new(100, 60),
                session_ttl_secs: 30 * 24 * 60 * 60,
                series_inflight: Default::default(),
                chapters_inflight: Default::default(),
                source_budgets: Default::default(),
                catalogue_cover_phash: false,
                ext_icons_dir: Default::default(),
                media_manifests: None,
                policy: Default::default(),
                setup_code: Default::default(),
            }),
            false,
        );
        (cover_dir, schema)
    }

    async fn social_mutation(
        schema: &crate::graphql::ApiSchema,
        query: String,
    ) -> async_graphql::Response {
        Database::foreground_scope(
            schema.execute(
                async_graphql::Request::new(query)
                    .data(crate::graphql::RequestAuth(Some("ov1-token".into())))
                    .data(crate::graphql::ClientIp(Some("127.0.0.1".into()))),
            ),
        )
        .await
    }

    async fn post_staged_media(
        schema: &crate::graphql::ApiSchema,
        work: &str,
        media: uuid::Uuid,
    ) -> anyhow::Result<bool> {
        let response = social_mutation(schema, format!(
            r#"mutation {{ postComment(input:{{targetType:"series",targetId:"{work}",body:"image",hasSpoiler:false,mediaId:"{media}"}}) {{ id }} }}"#,
        )).await;
        if response.errors.len() == 1 && response.errors[0].message == "attached image not found" {
            return Ok(false);
        }
        anyhow::ensure!(
            response.errors.is_empty(),
            "postComment failed: {:?}",
            response.errors
        );
        Ok(true)
    }

    // Keep the real resolver's transaction open after its writes, without exposing a
    // private GraphQL helper. The deferred trigger runs during COMMIT with row locks
    // still held. The test owns the advisory gate it waits on.
    async fn hold_social_commit(
        pool: &Database,
        table: &str,
    ) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
        assert!(["comment_media", "notifications"].contains(&table));
        sqlx::raw_sql(&format!(
            "CREATE FUNCTION bg_social_commit_gate() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN PERFORM pg_advisory_xact_lock(4930189972093103::bigint); RETURN NEW; END $$; \
             CREATE CONSTRAINT TRIGGER bg_social_commit_gate AFTER UPDATE ON {table} \
             DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION bg_social_commit_gate()",
        ))
        .execute(pool)
        .await
        .unwrap();
        let mut gate = pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(4930189972093103::bigint)")
            .execute(&mut *gate)
            .await
            .unwrap();
        let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate)
            .await
            .unwrap();
        (gate, pid)
    }

    async fn wait_for_social_commit(pool: &Database, gate: i32) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity a WHERE a.datname=current_database() \
                     AND a.query LIKE 'COMMIT%' AND a.wait_event_type='Lock' AND a.wait_event='advisory' \
                     AND $1=ANY(pg_blocking_pids(a.pid)))",
                ).bind(gate).fetch_one(pool).await.unwrap();
                if waiting { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("the mutation never reached COMMIT holding its row locks");
    }

    #[tokio::test]
    async fn staged_media_gc_then_post_waits_and_cleanly_refuses_the_deleted_draft() {
        use crate::catalog::phase4_tests::{overlap, pool, work};

        let pool = pool().await;
        let work = work(&pool, "media target").await;
        let (user, media) = social_gc_fixture(&pool).await;
        let (_cover_dir, schema) = social_schema(&pool, &user).await;
        let (swept, posted) = overlap(
            &pool,
            |pool| async move { Ok(sweep_staged_media(&pool, "2026-07-10T12:00:00Z").await?) },
            move |_| async move { post_staged_media(&schema, &work, media).await },
        )
        .await;
        assert_eq!(swept.unwrap(), 1);
        assert!(
            !posted.unwrap(),
            "a deleted draft must refuse the entire post"
        );
        let counts: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM comments),(SELECT count(*) FROM comment_media)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(counts, (0, 0));
        pool.close().await;
    }

    #[tokio::test]
    async fn post_then_staged_media_gc_skips_the_attached_draft_without_waiting() {
        use crate::catalog::phase4_tests::{pool, work};

        let pool = pool().await;
        let work = work(&pool, "media target").await;
        let (user, media) = social_gc_fixture(&pool).await;
        let (_cover_dir, schema) = social_schema(&pool, &user).await;
        let (gate, pid) = hold_social_commit(&pool, "comment_media").await;
        let post = tokio::spawn(async move { post_staged_media(&schema, &work, media).await });
        wait_for_social_commit(&pool, pid).await;
        // No wait: SKIP LOCKED excludes the draft the post already attached, even
        // though the reader snapshot still sees it as staged until the post commits.
        let swept = tokio::time::timeout(
            Duration::from_secs(2),
            sweep_staged_media(&pool, "2026-07-10T12:00:00Z"),
        )
        .await;
        assert!(!post.is_finished());
        gate.commit().await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), post)
            .await
            .unwrap()
            .unwrap()
            .unwrap());
        assert_eq!(
            swept.expect("GC waited for the post's media row").unwrap(),
            0
        );
        let linked: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT comment_id FROM comment_media WHERE id=$1")
                .bind(media)
                .fetch_one(&pool)
                .await
                .unwrap();
        let comment = linked.expect("the attached media lost its comment");
        let comments: i64 = sqlx::query_scalar("SELECT count(*) FROM comments WHERE id=$1")
            .bind(comment)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(comments, 1);
        assert_eq!(
            sweep_staged_media(&pool, "2026-07-10T12:00:00Z")
                .await
                .unwrap(),
            0
        );
        pool.close().await;
    }

    async fn mark_gc_notification_read(
        schema: &crate::graphql::ApiSchema,
        id: uuid::Uuid,
    ) -> anyhow::Result<u64> {
        let response = social_mutation(
            schema,
            format!(r#"mutation {{ markNotificationsRead(ids:["{id}"]) }}"#),
        )
        .await;
        anyhow::ensure!(
            response.errors.is_empty(),
            "markNotificationsRead failed: {:?}",
            response.errors
        );
        response.data.into_json()?["markNotificationsRead"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("markNotificationsRead did not return a count"))
    }

    async fn old_read_notification(pool: &Database) -> (String, uuid::Uuid) {
        let (user, _) = social_gc_fixture(pool).await;
        let id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO notifications(id,user_id,kind,created_at,read_at) VALUES ($1,$2,'reply','2020-01-01','2020-01-02')")
            .bind(id).bind(crate::db_types::uuid(&user).unwrap()).execute(pool).await.unwrap();
        (user, id)
    }

    #[tokio::test]
    async fn notification_gc_then_mark_read_waits_and_skips_the_deleted_row() {
        use crate::catalog::phase4_tests::{overlap, pool};

        let pool = pool().await;
        let (user, id) = old_read_notification(&pool).await;
        let (_cover_dir, schema) = social_schema(&pool, &user).await;
        let (pruned, marked) = overlap(
            &pool,
            |pool| async move {
                prune_read_notifications(&pool, "2026-07-10T12:00:00Z", &never_shutdown().1).await
            },
            move |_| async move { mark_gc_notification_read(&schema, id).await },
        )
        .await;
        assert_eq!(pruned.unwrap(), 1);
        assert_eq!(
            marked.unwrap(),
            0,
            "a deleted notification is a clean no-op"
        );
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
        pool.close().await;
    }

    #[tokio::test]
    async fn mark_read_then_notification_gc_skips_the_row_without_waiting() {
        use crate::catalog::phase4_tests::pool;

        let pool = pool().await;
        let (user, id) = old_read_notification(&pool).await;
        sqlx::query("UPDATE notifications SET read_at=NULL WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        let (_cover_dir, schema) = social_schema(&pool, &user).await;
        let (gate, pid) = hold_social_commit(&pool, "notifications").await;
        let mark = tokio::spawn(async move { mark_gc_notification_read(&schema, id).await });
        wait_for_social_commit(&pool, pid).await;
        // No wait: an unread row is not a GC candidate in the committed snapshot.
        // It becomes eligible only when the mark-read commits; a later pass prunes it.
        let pruned = tokio::time::timeout(
            Duration::from_secs(2),
            prune_read_notifications(&pool, "2026-07-10T12:00:00Z", &never_shutdown().1),
        )
        .await;
        assert!(!mark.is_finished());
        gate.commit().await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), mark)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            1
        );
        assert_eq!(pruned.expect("GC waited for mark-read's row").unwrap(), 0);
        let kept: (uuid::Uuid, chrono::DateTime<chrono::Utc>) =
            sqlx::query_as("SELECT id,read_at FROM notifications")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kept.0, id);
        assert!(kept.1 > crate::db_types::date("2020-01-02T00:00:00Z").unwrap());
        assert_eq!(
            prune_read_notifications(&pool, "2026-07-10T12:00:00Z", &never_shutdown().1)
                .await
                .unwrap(),
            1
        );
        pool.close().await;
    }

    /// A second session of the test database, outside the one-connection pool.
    async fn other_session(pool: &Database) -> sqlx::PgConnection {
        use sqlx::ConnectOptions;
        pool.connect_options()
            .as_ref()
            .clone()
            .connect()
            .await
            .expect("second session")
    }

    const SKIP_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

    /// T096. Another transaction holds one old read notification (as `deleteComment`
    /// and `markNotificationsRead` do while they work). The prune must not wait for
    /// it: it deletes the rows nobody holds and leaves the held one for a later run.
    /// With the plain `DELETE ... WHERE id IN (...)` the prune waited behind the holder
    /// while it already held other rows, which is one half of a deadlock.
    #[tokio::test]
    async fn notification_gc_skips_rows_another_writer_holds_and_never_waits() {
        let pool = main_pool().await;
        sqlx::query("INSERT INTO users(id, username, email, password_hash, created_at) VALUES ('8834b774-e415-4d35-952e-299c9fb4381a', '8834b774-e415-4d35-952e-299c9fb4381a', 'gc@example.test', 'unused', '2020-01-01T00:00:00+00:00')")
            .execute(&pool).await.unwrap();
        sqlx::query("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<40) INSERT INTO notifications(id,user_id,kind,created_at,read_at) SELECT md5('gc-held-' || i)::uuid, '8834b774-e415-4d35-952e-299c9fb4381a', 'reply', '2020-01-01T00:00:00+00:00', '2020-01-02T00:00:00+00:00' FROM n")
            .execute(&pool).await.unwrap();
        // The holder takes the rows that sort FIRST and LAST by id, so whichever order
        // the prune walks in, it meets a held row after it has locked others.
        let held: Vec<uuid::Uuid> = sqlx::query_scalar(
            "(SELECT id FROM notifications ORDER BY id LIMIT 1) UNION ALL \
             (SELECT id FROM notifications ORDER BY id DESC LIMIT 1)",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut holder = other_session(&pool).await;
        sqlx::query("BEGIN").execute(&mut holder).await.unwrap();
        let locked: Vec<uuid::Uuid> =
            sqlx::query_scalar("SELECT id FROM notifications WHERE id = ANY($1) FOR UPDATE")
                .bind(&held)
                .fetch_all(&mut holder)
                .await
                .unwrap();
        assert_eq!(locked.len(), 2);

        let cutoff = "2026-07-10T12:00:00+00:00";
        let (pruned, seen) = crate::db::test_counters::scope(tokio::time::timeout(
            SKIP_BOUND,
            prune_read_notifications(&pool, cutoff, &never_shutdown().1),
        ))
        .await;
        let pruned = pruned
            .expect("the prune waited for a row another transaction holds")
            .unwrap();
        assert_eq!(pruned, 38, "every row nobody holds is pruned");
        assert_eq!(seen.database.deadlocks, 0);
        assert_eq!(seen.database.lock_timeouts, 0);
        assert_eq!(seen.database.retries, 0);
        let mut kept: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM notifications")
            .fetch_all(&pool)
            .await
            .unwrap();
        kept.sort_unstable();
        let mut expected = held.clone();
        expected.sort_unstable();
        assert_eq!(kept, expected, "exactly the held rows are left");

        // The holder decides to keep its rows (a rollback): the next sweep prunes them.
        sqlx::query("ROLLBACK").execute(&mut holder).await.unwrap();
        assert_eq!(
            prune_read_notifications(&pool, cutoff, &never_shutdown().1)
                .await
                .unwrap(),
            2
        );
        let _ = sqlx::Connection::close(holder).await;
    }

    /// The lock order of the prune statement: the rows are locked by a `LockRows` node
    /// that sits above the sort on the primary key, and the batch itself is still read
    /// oldest first through the partial index, without sorting the backlog.
    #[tokio::test]
    async fn notification_gc_locks_its_batch_in_id_order_and_reads_it_through_the_partial_index() {
        let pool = main_pool().await;
        sqlx::query("INSERT INTO users(id, username, email, password_hash, created_at) VALUES ('8834b774-e415-4d35-952e-299c9fb4381a', '8834b774-e415-4d35-952e-299c9fb4381a', 'gc@example.test', 'unused', '2020-01-01T00:00:00+00:00')")
            .execute(&pool).await.unwrap();
        sqlx::query("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<1400) INSERT INTO notifications(id,user_id,kind,created_at,read_at) SELECT md5('gc-plan-' || i)::uuid, '8834b774-e415-4d35-952e-299c9fb4381a', 'reply', '2020-01-01T00:00:00+00:00', '2020-01-02T00:00:00+00:00' FROM n")
            .execute(&pool).await.unwrap();
        sqlx::query("ANALYZE notifications")
            .execute(&pool)
            .await
            .unwrap();
        let plan: Vec<String> =
            crate::db_types::query_scalar(&format!("EXPLAIN {READ_NOTIFICATION_PRUNE_SQL}"))
                .bind(crate::db_types::date("2026-07-10T12:00:00+00:00").unwrap())
                .bind(NOTIFICATION_DELETE_BATCH as i64)
                .fetch_all(&pool)
                .await
                .unwrap();
        let line = |needle: &str| plan.iter().position(|row| row.contains(needle));
        let lock = line("LockRows").unwrap_or_else(|| panic!("no LockRows: {plan:?}"));
        let sort = line("Sort Key: notifications").unwrap_or_else(|| panic!("no sort: {plan:?}"));
        assert!(
            lock < sort,
            "rows must be locked after the id sort: {plan:?}"
        );
        assert!(
            plan[sort].contains("id") && !plan[sort].contains("created_at"),
            "the lock order is the primary key: {plan:?}"
        );
        assert!(
            line("idx_notifications_read_created").is_some(),
            "the batch is chosen through the partial index: {plan:?}"
        );
        assert_eq!(
            plan.iter().filter(|row| row.contains("Sort Key")).count(),
            1,
            "only the batch of ids is sorted, never the backlog: {plan:?}"
        );
    }

    /// T095. `postComment` holds a staged media row while it links it. The sweep
    /// deletes the other old drafts and does not wait for that row.
    #[tokio::test]
    async fn staged_media_sweep_skips_a_draft_another_writer_holds_and_never_waits() {
        let pool = main_pool().await;
        sqlx::query("INSERT INTO users(id, username, email, password_hash, created_at) VALUES ('8834b774-e415-4d35-952e-299c9fb4381a', '8834b774-e415-4d35-952e-299c9fb4381a', 'gc@example.test', 'unused', '2020-01-01T00:00:00+00:00')")
            .execute(&pool).await.unwrap();
        for (name, created_at) in [
            ("held-draft", "2020-01-01T00:00:00+00:00"),
            ("free-draft", "2020-01-01T00:00:00+00:00"),
            ("fresh-draft", "2026-07-11T00:00:00+00:00"),
        ] {
            sqlx::query("INSERT INTO comment_media(id,comment_id,user_id,webp,width,height,created_at) VALUES ($1,NULL,'8834b774-e415-4d35-952e-299c9fb4381a','\\x00',1,1,$2)")
                .bind(crate::db_types::test_uuid(name))
                .bind(crate::db_types::date(created_at).unwrap())
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut holder = other_session(&pool).await;
        sqlx::query("BEGIN").execute(&mut holder).await.unwrap();
        sqlx::query("SELECT id FROM comment_media WHERE id = $1 FOR UPDATE")
            .bind(crate::db_types::test_uuid("held-draft"))
            .fetch_one(&mut holder)
            .await
            .unwrap();
        let cutoff = "2026-07-10T12:00:00+00:00";
        let swept = tokio::time::timeout(SKIP_BOUND, sweep_staged_media(&pool, cutoff))
            .await
            .expect("the sweep waited for a draft another transaction holds")
            .unwrap();
        assert_eq!(swept, 1);
        let mut kept: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT id FROM comment_media")
            .fetch_all(&pool)
            .await
            .unwrap();
        kept.sort_unstable();
        let mut expected = vec![
            crate::db_types::test_uuid("held-draft"),
            crate::db_types::test_uuid("fresh-draft"),
        ];
        expected.sort_unstable();
        assert_eq!(kept, expected);
        sqlx::query("ROLLBACK").execute(&mut holder).await.unwrap();
        assert_eq!(sweep_staged_media(&pool, cutoff).await.unwrap(), 1);
        let _ = sqlx::Connection::close(holder).await;
    }

    /// Main DB with the real schema through the PostgreSQL test harness.
    async fn main_pool() -> Database {
        let pool = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    /// `w_%05d` works for `lo..hi`, generated in SQL so a 10k-row fixture is one
    /// statement rather than 10k round trips.
    async fn seed_works(pool: &Database, lo: i64, hi: i64) {
        sqlx::query(
            "INSERT INTO work (id, primary_title, created_at, updated_at) \
             WITH RECURSIVE n(i) AS (SELECT $1 UNION ALL SELECT i + 1 FROM n WHERE i < $2) \
             SELECT 'w_' || lpad(i::text, 5, '0'), 'T', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z' \
             FROM n",
        )
        .bind(lo)
        .bind(hi - 1)
        .execute(pool)
        .await
        .unwrap();
    }

    fn never_shutdown() -> (
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Receiver<bool>,
    ) {
        tokio::sync::watch::channel(false)
    }

    fn age_file(path: &std::path::Path) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - COVER_FILE_GRACE - Duration::from_secs(60))
            .unwrap();
    }

    async fn place_file(pool: &Database, covers: &CoverStore, bytes: &[u8]) -> cover_store::Sha {
        let mut staged = covers.stage(bytes).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        let _guards = crate::cover::lock_assets_and_files(&mut tx, &[], &[staged.sha])
            .await
            .unwrap();
        covers.place(&mut staged).await.unwrap();
        tx.commit().await.unwrap();
        staged.sha
    }

    async fn seed_object(
        pool: &Database,
        covers: &CoverStore,
        asset: &str,
        kind: &str,
        bytes: &[u8],
    ) -> cover_store::Sha {
        let mut staged = covers.stage(bytes).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        if asset.starts_with("s:") {
            crate::engine_generation::shared(&mut tx, None)
                .await
                .unwrap();
        }
        let _guards = crate::cover::lock_assets_and_files(&mut tx, &[asset], &[staged.sha])
            .await
            .unwrap();
        if !asset.starts_with("s:") && !asset.starts_with("icon:") {
            crate::locks::lock_works(&mut tx, &[asset]).await.unwrap();
        }
        covers.place(&mut staged).await.unwrap();
        if kind == "master" {
            let version = (!asset.starts_with("s:") && !asset.starts_with("icon:")).then_some(7);
            cover_store::put_master(&mut tx, asset, &staged, version)
                .await
                .unwrap();
        } else {
            let master = cover_store::row(&mut *tx, asset, cover_store::Kind::Master)
                .await
                .unwrap()
                .unwrap();
            let width = kind.strip_prefix('w').unwrap().parse().unwrap();
            assert!(
                cover_store::put_derivative(&mut tx, asset, width, &staged, &master.sha)
                    .await
                    .unwrap()
            );
        }
        tx.commit().await.unwrap();
        staged.sha
    }

    async fn object_count(pool: &Database, asset: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM cover_object WHERE asset_key = $1")
            .bind(cover_store::index_key(asset))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn unreferenced_old_cover_file_is_removed() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        let sha = place_file(&pool, &covers, b"unreferenced").await;
        let path = covers.object_path(&sha);
        age_file(&path);
        assert_eq!(
            sweep_unreferenced_files(&pool, &covers, &never_shutdown().1)
                .await
                .unwrap(),
            1
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn referenced_old_cover_file_is_kept() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        let sha = place_file(&pool, &covers, b"referenced").await;
        seed_object(&pool, &covers, "icon:kept", "master", b"referenced").await;
        age_file(&covers.object_path(&sha));
        assert_eq!(
            sweep_unreferenced_files(&pool, &covers, &never_shutdown().1)
                .await
                .unwrap(),
            0
        );
        assert!(covers.object_path(&sha).exists());
    }

    #[tokio::test]
    async fn young_unreferenced_cover_file_and_unknown_paths_are_kept() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        let sha = place_file(&pool, &covers, b"young").await;
        for relative in ["unknown/file", "ab/unknown", "ab/00", "cd/ab"] {
            let path = covers.objects_dir().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"keep").unwrap();
            age_file(&path);
        }
        let wrong_prefix = covers.objects_dir().join("ab").join("cd".repeat(32));
        std::fs::write(&wrong_prefix, b"keep").unwrap();
        age_file(&wrong_prefix);
        assert_eq!(
            sweep_unreferenced_files(&pool, &covers, &never_shutdown().1)
                .await
                .unwrap(),
            0
        );
        assert!(covers.object_path(&sha).exists());
        assert!(wrong_prefix.exists());
        assert!(covers.objects_dir().join("ab/unknown").exists());
    }

    #[tokio::test]
    async fn unreferenced_sweep_waits_for_a_publisher_and_rechecks_references_and_mtime() {
        for commit in [true, false] {
            let pool = main_pool().await.foreground();
            let dir = cover_store::test_support::TestDir::new();
            let covers = cover_store::test_support::store(&dir);
            let sha = place_file(&pool, &covers, b"reused").await;
            age_file(&covers.object_path(&sha));
            let mut staged = covers.stage(b"reused").await.unwrap();
            let mut holder = pool.begin().await.unwrap();
            let guards = crate::cover::lock_assets_and_files(&mut holder, &["icon:reused"], &[sha])
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO cover_object(asset_key,kind,sha256,byte_length,updated_at) \
                 VALUES ('icon:reused','master',$1,6,now())",
            )
            .bind(sha.as_slice())
            .execute(&mut *holder)
            .await
            .unwrap();
            let (writer, store) = (pool.clone(), covers.clone());
            let (_signal, shutdown) = never_shutdown();
            let mut sweep =
                tokio::spawn(
                    async move { sweep_unreferenced_files(&writer, &store, &shutdown).await },
                );
            crate::cover::wait_for_writer_lock(&pool).await;
            assert!(tokio::time::timeout(Duration::from_millis(50), &mut sweep)
                .await
                .is_err());
            if !commit {
                // Even a rolled-back publication restarts the file's grace period.
                covers.place(&mut staged).await.unwrap();
                holder.rollback().await.unwrap();
            } else {
                holder.commit().await.unwrap();
            }
            drop(guards);
            assert_eq!(sweep.await.unwrap().unwrap(), 0);
            assert!(covers.object_path(&sha).exists());
        }
    }

    #[tokio::test]
    async fn temp_file_sweep_keeps_young_files_and_directories() {
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        let old = covers.tmp_dir().join("old");
        let young = covers.tmp_dir().join("young");
        let directory = covers.tmp_dir().join("directory");
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&young, b"young").unwrap();
        std::fs::create_dir(&directory).unwrap();
        age_file(&old);
        assert_eq!(
            sweep_temp_files(&covers, &never_shutdown().1)
                .await
                .unwrap(),
            1
        );
        assert!(!old.exists());
        assert!(young.exists());
        assert!(directory.exists());
    }

    #[tokio::test]
    async fn temp_file_sweep_obeys_budget_and_shutdown() {
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        for n in 0..=COVER_SWEEP_BUDGET {
            let path = covers.tmp_dir().join(n.to_string());
            std::fs::write(&path, b"old").unwrap();
            age_file(&path);
        }
        let (_signal, shutdown) = tokio::sync::watch::channel(true);
        assert_eq!(sweep_temp_files(&covers, &shutdown).await.unwrap(), 0);
        assert_eq!(
            sweep_temp_files(&covers, &never_shutdown().1)
                .await
                .unwrap(),
            COVER_SWEEP_BUDGET as u64
        );
        assert_eq!(
            sweep_temp_files(&covers, &never_shutdown().1)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn orphan_source_sweep_removes_only_old_unenrolled_numeric_assets() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        sqlx::query("INSERT INTO suwayomi_series(id,title,status,source_id,updated_at) VALUES (1,'Live','ONGOING','1',now())")
            .execute(&pool).await.unwrap();
        for asset in ["s:1", "s:2", "s:3", "s:invalid", "icon:4"] {
            seed_object(&pool, &covers, asset, "master", &[1; 32]).await;
            seed_object(&pool, &covers, asset, "w192", &[2; 32]).await;
        }
        sqlx::query("UPDATE cover_object SET updated_at = now() - interval '25 hours' WHERE asset_key <> 's:3'")
            .execute(&pool).await.unwrap();
        assert_eq!(
            sweep_orphan_source_covers(&pool, &never_shutdown().1)
                .await
                .unwrap(),
            1
        );
        assert_eq!(object_count(&pool, "s:2").await, 0);
        for asset in ["s:1", "s:3", "s:invalid", "icon:4"] {
            assert_eq!(object_count(&pool, asset).await, 2, "{asset}");
        }
    }

    #[tokio::test]
    async fn orphan_source_sweep_obeys_budget_and_shutdown() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        for id in 1..=1001 {
            seed_object(&pool, &covers, &format!("s:{id}"), "master", b"source").await;
        }
        sqlx::query("UPDATE cover_object SET updated_at = now() - interval '25 hours'")
            .execute(&pool)
            .await
            .unwrap();
        let (_signal, shutdown) = tokio::sync::watch::channel(true);
        assert_eq!(
            sweep_orphan_source_covers(&pool, &shutdown).await.unwrap(),
            0
        );
        assert_eq!(
            sweep_orphan_source_covers(&pool, &never_shutdown().1)
                .await
                .unwrap(),
            1000
        );
        assert_eq!(
            sweep_orphan_source_covers(&pool, &never_shutdown().1)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn source_cover_gc_rechecks_recreated_series_before_deleting_variants() {
        for republish in [false, true] {
            let pool = main_pool().await.foreground();
            let dir = cover_store::test_support::TestDir::new();
            let covers = cover_store::test_support::store(&dir);
            seed_object(&pool, &covers, "s:42", "master", &[1; 32]).await;
            seed_object(&pool, &covers, "s:42", "w192", &[2; 32]).await;
            sqlx::query("UPDATE cover_object SET updated_at = now() - interval '25 hours'")
                .execute(&pool)
                .await
                .unwrap();
            let mut master = covers.stage(b"new master").await.unwrap();
            let mut derivative = covers.stage(b"new derivative").await.unwrap();
            let mut holder = pool.begin().await.unwrap();
            crate::engine_generation::shared(&mut holder, None)
                .await
                .unwrap();
            let guards = crate::cover::lock_assets_and_files(
                &mut holder,
                &["s:42"],
                &[master.sha, derivative.sha],
            )
            .await
            .unwrap();
            let writer = pool.clone();
            let (_signal, shutdown) = never_shutdown();
            let mut sweep =
                tokio::spawn(async move { sweep_orphan_source_covers(&writer, &shutdown).await });
            crate::cover::wait_for_writer_lock(&pool).await;
            assert!(tokio::time::timeout(Duration::from_millis(50), &mut sweep)
                .await
                .is_err());
            sqlx::query("INSERT INTO suwayomi_series(id,title,status,source_id,updated_at) VALUES (42,'Recreated','ONGOING','1',now())")
                .execute(&mut *holder).await.unwrap();
            if republish {
                covers.place(&mut master).await.unwrap();
                cover_store::put_master(&mut holder, "s:42", &master, None)
                    .await
                    .unwrap();
                covers.place(&mut derivative).await.unwrap();
                assert!(cover_store::put_derivative(
                    &mut holder,
                    "s:42",
                    192,
                    &derivative,
                    &master.sha
                )
                .await
                .unwrap());
            }
            holder.commit().await.unwrap();
            drop(guards);
            assert_eq!(sweep.await.unwrap().unwrap(), 0);
            assert_eq!(object_count(&pool, "s:42").await, 2);
            let current = cover_store::row(&pool, "s:42", cover_store::Kind::Master)
                .await
                .unwrap()
                .unwrap();
            let small = cover_store::row(&pool, "s:42", cover_store::Kind::Width(192))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                current.sha,
                if republish {
                    master.sha
                } else {
                    cover_store::sha256(&[1; 32])
                }
            );
            assert_eq!(
                small.sha,
                if republish {
                    derivative.sha
                } else {
                    cover_store::sha256(&[2; 32])
                }
            );
        }
    }

    #[tokio::test]
    async fn missing_master_file_deletes_asset_and_clears_pointer_but_keeps_cards() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        seed_works(&pool, 0, 1).await;
        let asset = "w_00000";
        let sha = seed_object(&pool, &covers, asset, "master", &[1; 32]).await;
        seed_object(&pool, &covers, asset, "w192", &[2; 32]).await;
        seed_object(&pool, &covers, asset, "w320", &[3; 32]).await;
        std::fs::remove_file(covers.object_path(&sha)).unwrap();
        sqlx::query("UPDATE work SET cover_cached_version = 7 WHERE id=$1")
            .bind(asset)
            .execute(&pool)
            .await
            .unwrap();
        let url = crate::cover::cover_path(asset, 7);
        sqlx::query("INSERT INTO feed_updates(work_id,title,latest_at,cover_url) VALUES ($1,'Cover',now(),$2)")
            .bind(asset).bind(&url).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO feed_series_updates(work_id,reader_id,title,released_at,cover_url) VALUES ($1,$1,'Cover',now(),$2)")
            .bind(asset).bind(&url).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO browse_catalogue(work_id,reader_id,title,created_at,cover_url) VALUES ($1,$1,'Cover',now(),$2)")
            .bind(asset).bind(&url).execute(&pool).await.unwrap();
        repair_missing_cover_files(
            &pool,
            &covers,
            &never_shutdown().1,
            &mut CoverRepairCursor::default(),
        )
        .await
        .unwrap();
        assert_eq!(object_count(&pool, asset).await, 0);
        let pointer: Option<i64> =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id=$1")
                .bind(asset)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pointer, None);
        for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
            let actual: String =
                sqlx::query_scalar(&format!("SELECT cover_url FROM {table} WHERE work_id=$1"))
                    .bind(asset)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(actual, url, "{table}");
        }
    }

    #[tokio::test]
    async fn missing_uploaded_master_is_restored_with_its_index_and_pointer() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        seed_works(&pool, 0, 1).await;
        crate::cover::put_uploaded_work_cover(&pool, &covers, "w_00000", b"admin upload")
            .await
            .unwrap();
        let before = cover_store::row(&pool, "w_00000", cover_store::Kind::Master)
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_file(covers.object_path(&before.sha)).unwrap();
        assert_eq!(
            repair_missing_cover_files(
                &pool,
                &covers,
                &never_shutdown().1,
                &mut CoverRepairCursor::default()
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            covers.read_object(&before.sha).await.unwrap().as_deref(),
            Some(b"admin upload".as_slice())
        );
        assert_eq!(
            cover_store::row(&pool, "w_00000", cover_store::Kind::Master)
                .await
                .unwrap(),
            Some(before.clone())
        );
        let pointer: Option<i64> =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id='w_00000'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pointer, before.version);
    }

    #[tokio::test]
    async fn missing_derivative_deletes_only_that_row() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        seed_works(&pool, 0, 1).await;
        crate::cover::put_work_cover(&pool, &covers, "w_00000", b"master")
            .await
            .unwrap();
        let (master, _) = covers
            .read_master(&pool, "w_00000", None)
            .await
            .unwrap()
            .unwrap();
        crate::cover::put_current_variant(&pool, &covers, "w_00000", 192, &master, b"small")
            .await
            .unwrap();
        crate::cover::put_current_variant(&pool, &covers, "w_00000", 320, &master, b"large")
            .await
            .unwrap();
        let derivative = cover_store::row(&pool, "w_00000", cover_store::Kind::Width(192))
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_file(covers.object_path(&derivative.sha)).unwrap();
        assert_eq!(
            repair_missing_cover_files(
                &pool,
                &covers,
                &never_shutdown().1,
                &mut CoverRepairCursor::default()
            )
            .await
            .unwrap(),
            1
        );
        assert!(
            cover_store::row(&pool, "w_00000", cover_store::Kind::Width(192))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(object_count(&pool, "w_00000").await, 2);
        let pointer: Option<i64> =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id='w_00000'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pointer, master.version);
    }

    #[tokio::test]
    async fn missing_file_repair_refuses_a_store_that_holds_almost_none_of_the_index() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        let rows = COVER_PLAUSIBILITY_MIN_SAMPLE + 8;
        for n in 0..rows {
            seed_object(
                &pool,
                &covers,
                &format!("icon:{n:03}"),
                "master",
                format!("icon {n}").as_bytes(),
            )
            .await;
        }
        // A wrong or freshly created KOMIKA_COVERS_DIR: every indexed file looks lost.
        let empty_dir = cover_store::test_support::TestDir::new();
        let empty = cover_store::test_support::store(&empty_dir);
        let mut cursor = CoverRepairCursor::default();
        assert_eq!(
            repair_missing_cover_files(&pool, &empty, &never_shutdown().1, &mut cursor)
                .await
                .unwrap(),
            0
        );
        let indexed: i64 = sqlx::query_scalar("SELECT count(*) FROM cover_object")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(indexed, rows as i64, "nothing is cleared");
        // The right directory with one file really lost: that row is repaired.
        let lost = cover_store::sha256(b"icon 0");
        std::fs::remove_file(covers.object_path(&lost)).unwrap();
        assert_eq!(
            repair_missing_cover_files(&pool, &covers, &never_shutdown().1, &mut cursor)
                .await
                .unwrap(),
            1
        );
        assert_eq!(object_count(&pool, "icon:000").await, 0);
    }

    /// The read-path repair takes the GC guard: against a store that holds almost none of
    /// the index it drops no row and no cover pointer (an upload is still restored), and
    /// against the right store it drops the row whose file is really lost.
    #[tokio::test]
    async fn read_repair_refuses_a_store_that_holds_almost_none_of_the_index() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        for n in 0..COVER_PLAUSIBILITY_MIN_SAMPLE + 8 {
            seed_object(
                &pool,
                &covers,
                &format!("icon:{n:03}"),
                "master",
                format!("icon {n}").as_bytes(),
            )
            .await;
        }
        seed_works(&pool, 0, 2).await;
        let lost = seed_object(&pool, &covers, "w_00000", "master", b"lost master").await;
        let uploaded = seed_object(&pool, &covers, "w_00001", "master", b"uploaded").await;
        sqlx::query("INSERT INTO cover_upload(work_id,bytes,sha256,created_at) VALUES ('w_00001',$1,$2,now())")
            .bind(b"uploaded".as_slice())
            .bind(uploaded.as_slice())
            .execute(&pool)
            .await
            .unwrap();
        let pointer = |work: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, Option<i64>>(
                    "SELECT cover_cached_version FROM work WHERE id = $1",
                )
                .bind(work)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        sqlx::query("UPDATE work SET cover_cached_version = 7 WHERE id = 'w_00000'")
            .execute(&pool)
            .await
            .unwrap();
        let before = pointer("w_00000").await;
        assert_eq!(before, Some(7));
        // A wrong or unmounted KOMIKA_COVERS_DIR: every indexed file looks lost.
        let empty_dir = cover_store::test_support::TestDir::new();
        let empty = cover_store::test_support::store(&empty_dir);
        assert!(!repair_lost_master_on_read(&pool, &empty, "w_00000", lost)
            .await
            .unwrap());
        assert_eq!(object_count(&pool, "w_00000").await, 1, "the row stays");
        assert_eq!(pointer("w_00000").await, before, "the pointer stays");
        assert!(
            repair_lost_master_on_read(&pool, &empty, "w_00001", uploaded)
                .await
                .unwrap()
        );
        assert!(
            empty.object_path(&uploaded).exists(),
            "an upload is restored"
        );
        // The right directory with one file really lost: that row is repaired.
        std::fs::remove_file(covers.object_path(&lost)).unwrap();
        assert!(repair_lost_master_on_read(&pool, &covers, "w_00000", lost)
            .await
            .unwrap());
        assert_eq!(object_count(&pool, "w_00000").await, 0);
        assert_eq!(pointer("w_00000").await, None);
    }

    /// The operator's one-shot repair ignores the guard: with most files gone it clears
    /// every row whose file is missing and keeps the rows whose files exist. A covers
    /// path that is missing or not a directory is refused before anything is opened.
    #[tokio::test]
    async fn repair_missing_command_clears_lost_rows_even_when_most_are_lost() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        let rows = COVER_PLAUSIBILITY_MIN_SAMPLE + 8;
        let mut shas = Vec::new();
        for n in 0..rows {
            shas.push(
                seed_object(
                    &pool,
                    &covers,
                    &format!("icon:{n:03}"),
                    "master",
                    format!("icon {n}").as_bytes(),
                )
                .await,
            );
        }
        // A restore without cover files: all but the first four are gone.
        for sha in &shas[4..] {
            std::fs::remove_file(covers.object_path(sha)).unwrap();
        }
        assert!(!store_plausible(&pool, &covers).await.unwrap());
        assert_eq!(
            repair_all_missing_cover_files(&pool, &covers)
                .await
                .unwrap(),
            RepairAll {
                checked: rows as u64,
                repaired: rows as u64 - 4,
                failed: 0
            }
        );
        let kept: Vec<String> =
            sqlx::query_scalar("SELECT asset_key FROM cover_object ORDER BY asset_key")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(kept, ["icon:000", "icon:001", "icon:002", "icon:003"]);
        check_covers_dir(covers.dir()).unwrap();
        assert!(check_covers_dir(&covers.dir().join("absent")).is_err());
        assert!(check_covers_dir(&covers.object_path(&shas[0])).is_err());
    }

    #[tokio::test]
    async fn missing_file_repair_passes_a_row_that_fails_and_repairs_the_rest() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        seed_works(&pool, 0, 2).await;
        let bad = seed_object(&pool, &covers, "w_00000", "master", b"first").await;
        let good = seed_object(&pool, &covers, "w_00001", "master", b"second").await;
        // An upload row whose bytes no longer hash to the master: its restore fails.
        sqlx::query("INSERT INTO cover_upload(work_id,bytes,sha256,created_at) VALUES ('w_00000',$1,$2,now())")
            .bind(b"other bytes".as_slice())
            .bind(bad.as_slice())
            .execute(&pool)
            .await
            .unwrap();
        std::fs::remove_file(covers.object_path(&bad)).unwrap();
        std::fs::remove_file(covers.object_path(&good)).unwrap();
        let mut cursor = CoverRepairCursor::default();
        assert_eq!(
            repair_missing_cover_files(&pool, &covers, &never_shutdown().1, &mut cursor)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            object_count(&pool, "w_00000").await,
            1,
            "the failing row stays"
        );
        assert_eq!(
            object_count(&pool, "w_00001").await,
            0,
            "the row after it is repaired"
        );
    }

    #[tokio::test]
    async fn missing_file_repair_waits_for_the_sqlite_copy() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let legacy = cover_store::test_support::legacy_pool().await;
        let covers = cover_store::test_support::store_with_legacy(
            &dir,
            legacy,
            cover_store::CopyCursor::default(),
        );
        let sha = seed_object(&pool, &covers, "icon:pkg", "master", b"icon").await;
        std::fs::remove_file(covers.object_path(&sha)).unwrap();
        let mut cursor = CoverRepairCursor::default();
        assert_eq!(
            repair_missing_cover_files(&pool, &covers, &never_shutdown().1, &mut cursor)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            object_count(&pool, "icon:pkg").await,
            1,
            "the row waits for the copy"
        );
        covers.set_copy_cursor(cover_store::CopyCursor::done());
        assert_eq!(
            repair_missing_cover_files(&pool, &covers, &never_shutdown().1, &mut cursor)
                .await
                .unwrap(),
            1
        );
        assert_eq!(object_count(&pool, "icon:pkg").await, 0);
    }

    #[tokio::test]
    async fn missing_file_repair_resumes_wraps_and_obeys_scan_and_repair_budgets() {
        let pool = main_pool().await;
        let dir = cover_store::test_support::TestDir::new();
        let covers = cover_store::test_support::store(&dir);
        let sha = place_file(&pool, &covers, b"shared").await;
        let budget = COVER_REPAIR_SCAN_BUDGET;
        let key = |n: usize| format!("icon:{n:06}");
        sqlx::query(
            "INSERT INTO cover_object(asset_key,kind,sha256,byte_length,updated_at) \
             SELECT 'icon:' || lpad(n::text,6,'0'),'master',$1,1,now() FROM generate_series(1,$2) n",
        )
        .bind(sha.as_slice())
        .bind(budget as i64)
        .execute(&pool)
        .await
        .unwrap();
        let missing = seed_object(&pool, &covers, &key(budget + 1), "master", &[0; 32]).await;
        std::fs::remove_file(covers.object_path(&missing)).unwrap();
        let mut cursor = CoverRepairCursor::default();
        let (_signal, shutdown) = tokio::sync::watch::channel(true);
        assert_eq!(
            repair_missing_cover_files(&pool, &covers, &shutdown, &mut cursor)
                .await
                .unwrap(),
            0
        );
        assert!(cursor.asset.is_empty());
        assert_eq!(
            repair_missing_cover_files(&pool, &covers, &never_shutdown().1, &mut cursor)
                .await
                .unwrap(),
            0
        );
        assert_eq!(cursor.asset, key(budget));
        assert_eq!(object_count(&pool, &key(budget + 1)).await, 1);
        assert_eq!(
            repair_missing_cover_files(&pool, &covers, &never_shutdown().1, &mut cursor)
                .await
                .unwrap(),
            1
        );
        assert!(
            cursor.asset.is_empty(),
            "the cursor wraps after reaching the end"
        );
        std::fs::remove_file(covers.object_path(&sha)).unwrap();
        // The repair has its own budget, well above the other sweeps' deletion budget.
        assert_eq!(
            repair_missing_cover_files(&pool, &covers, &never_shutdown().1, &mut cursor)
                .await
                .unwrap(),
            COVER_REPAIR_BUDGET
        );
        assert_eq!(cursor.asset, key(COVER_REPAIR_BUDGET as usize));
    }
}
