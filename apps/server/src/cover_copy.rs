//! The one-time copy of the SQLite cover cache (`covers.sqlite3`) into cover files.
//!
//! Walks work masters, then numeric source masters, then extension icons, each by
//! ascending key, a bounded batch at a time. Every asset is copied with the same locks a
//! publication takes ([`crate::cover::lock_assets_and_files`]), and only into a gap:
//!
//! * a master is inserted only when the asset has no index row, so the copy never
//!   replaces a cover published since the files arrived;
//! * a derivative only when its SQLite master is the indexed master;
//! * a work that is gone or taken down is skipped;
//! * a numeric cover only under the engine generation captured at the start of the
//!   batch (`shared(Some(g))`): after a generation change its number may name another
//!   series, so the batch stops and the purge has already cleared the SQLite rows.
//!
//! The cursor lives in `instance_setting` ([`crate::cover_store::COPY_SETTING`]) and is
//! saved after each batch: a crash replays at most one batch, and every step is
//! idempotent. When the walk ends the store stops reading and deleting SQLite, and the
//! next boot no longer opens the file. The file stays on disk for the owner to delete.

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, NaiveDateTime, Utc};

use crate::cover::{self, lock_assets_and_files};
use crate::cover_store::{self, CopyCursor, CopyPhase, CoverStore, Kind, Staged};
use crate::db::Database;

const BATCH: i64 = 500;

/// `instance_setting` key of the recompression cursor (see `cover_recompress`).
pub const RECOMPRESS_SETTING: &str = "work_cover_recompress";

/// What one batch did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Batch {
    /// Assets that got an index row from this batch.
    pub copied: usize,
    /// Assets read but left alone (already indexed, gone, taken down).
    pub skipped: usize,
    /// The copy has finished.
    pub done: bool,
}

/// Copy the old cover cache in the background until the durable cursor is done.
pub fn spawn(main: Database, covers: CoverStore, shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::spawn(crate::task::supervise(
        "cover-copy",
        Duration::from_secs(30),
        shutdown.clone(),
        move || run_loop(main.clone(), covers.clone(), shutdown.clone()),
    ));
}

async fn run_loop(
    main: Database,
    covers: CoverStore,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(60),
        Duration::from_secs(5),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(batch = BATCH, "cover copy pass started");
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if covers.copy_done() {
                    return;
                }
                match tick(&main, &covers).await {
                    Ok(Some(batch)) if batch.done => {
                        tracing::info!("cover copy pass complete");
                        break;
                    }
                    Ok(Some(batch)) => tracing::debug!(
                        copied = batch.copied,
                        skipped = batch.skipped,
                        "cover copy tick"
                    ),
                    Ok(None) => {},
                    Err(error) => {
                        let error = crate::db::observed_database_error(error);
                        tracing::warn!(error = %error, "cover copy tick failed");
                    }
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("cover copy pass stopping");
                    break;
                }
            }
        }
    }
}

async fn tick(main: &Database, covers: &CoverStore) -> Result<Option<Batch>> {
    let Some(_lease) =
        crate::job_lease::JobLease::try_take(main, crate::job_lease::JobFamily::CoverCopy).await
    else {
        return Ok(None);
    };
    run_batch(main, covers, BATCH).await.map(Some)
}

/// Copy one batch of at most `limit` assets and advance the cursor.
pub async fn run_batch(main: &Database, covers: &CoverStore, limit: i64) -> Result<Batch> {
    if covers.copy_done() {
        return Ok(Batch {
            done: true,
            ..Batch::default()
        });
    }
    let Some(legacy) = covers.legacy() else {
        return finish(main, covers).await;
    };
    let mut cursor = covers.copy_cursor();
    let mut batch = Batch::default();
    match cursor.phase {
        CopyPhase::Work => {
            let rows: Vec<(String, Vec<u8>, i64)> = sqlx::query_as(
                "SELECT work_id, webp, version FROM work_cover_blob WHERE work_id > $1 \
                 ORDER BY work_id LIMIT $2",
            )
            .bind(&cursor.after)
            .bind(limit)
            .fetch_all(legacy)
            .await?;
            let Some((last, _, _)) = rows.last() else {
                return advance(main, covers, CopyPhase::Source).await;
            };
            let last = last.clone();
            for (work_id, webp, version) in rows {
                let variants = legacy_variants(covers, &work_id).await?;
                if copy_work(main, covers, &work_id, &webp, version, &variants).await? {
                    batch.copied += 1;
                } else {
                    batch.skipped += 1;
                }
            }
            cursor.after = last;
        }
        CopyPhase::Source => {
            // Captured before the rows are read: a numeric cover read under one
            // generation is written only while that generation is still active.
            let generation = crate::engine_generation::current(main).await?;
            let after = if cursor.after.is_empty() {
                i64::MIN
            } else {
                cursor.after.parse()?
            };
            let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
                "SELECT manga_id, webp FROM suwayomi_cover_blob WHERE manga_id > $1 \
                 ORDER BY manga_id LIMIT $2",
            )
            .bind(after)
            .bind(limit)
            .fetch_all(legacy)
            .await?;
            let Some(&(last, _)) = rows.last() else {
                return advance(main, covers, CopyPhase::Icon).await;
            };
            for (manga_id, webp) in rows {
                let key = cover::suwayomi_variant_key(manga_id);
                let variants = legacy_variants(covers, &key).await?;
                match copy_source(main, covers, manga_id, &webp, &variants, generation).await? {
                    Some(true) => batch.copied += 1,
                    Some(false) => batch.skipped += 1,
                    // The generation moved: stop here, the cursor stays where it was.
                    None => return Ok(batch),
                }
            }
            cursor.after = last.to_string();
        }
        CopyPhase::Icon => {
            let rows: Vec<(String, Vec<u8>, String)> = sqlx::query_as(
                "SELECT pkg_name, png, updated_at FROM extension_icon_blob WHERE pkg_name > $1 \
                 ORDER BY pkg_name LIMIT $2",
            )
            .bind(&cursor.after)
            .bind(limit)
            .fetch_all(legacy)
            .await?;
            let Some((last, _, _)) = rows.last() else {
                return finish(main, covers).await;
            };
            let last = last.clone();
            for (pkg, png, updated_at) in rows {
                if copy_icon(main, covers, &pkg, &png, sqlite_time(&updated_at)).await? {
                    batch.copied += 1;
                } else {
                    batch.skipped += 1;
                }
            }
            cursor.after = last;
        }
        CopyPhase::Done => return finish(main, covers).await,
    }
    // Cursor last: everything before it has committed.
    cover_store::save_cursor(main, &cursor).await?;
    covers.set_copy_cursor(cursor);
    Ok(batch)
}

/// Copy one asset now, ahead of the walk: a request found it only in SQLite. Idempotent
/// with the walk; does nothing once the copy has passed the asset.
pub async fn copy_asset(main: &Database, covers: &CoverStore, asset: &str) -> Result<bool> {
    let Some(legacy) = covers.legacy_for_read(asset) else {
        return Ok(false);
    };
    // Every request for a hot uncopied cover queues this; once one copy landed the rest
    // stop here, before reading or staging anything.
    if cover_store::row(main, asset, Kind::Master).await?.is_some() {
        return Ok(false);
    }
    if let Some(id) = asset.strip_prefix("s:") {
        let manga_id: i64 = id.parse()?;
        let generation = crate::engine_generation::current(main).await?;
        let webp: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT webp FROM suwayomi_cover_blob WHERE manga_id = $1")
                .bind(manga_id)
                .fetch_optional(legacy)
                .await?;
        let Some(webp) = webp else {
            return Ok(false);
        };
        let variants = legacy_variants(covers, asset).await?;
        return Ok(
            copy_source(main, covers, manga_id, &webp, &variants, generation)
                .await?
                .unwrap_or(false),
        );
    }
    if asset.starts_with("icon:") {
        return Ok(false);
    }
    let row: Option<(Vec<u8>, i64)> =
        sqlx::query_as("SELECT webp, version FROM work_cover_blob WHERE work_id = $1")
            .bind(asset)
            .fetch_optional(legacy)
            .await?;
    let Some((webp, version)) = row else {
        return Ok(false);
    };
    let variants = legacy_variants(covers, asset).await?;
    copy_work(main, covers, asset, &webp, version, &variants).await
}

/// The SQLite derivatives of `asset`. The SQLite publishers dropped them whenever the
/// master changed, so these were made from the SQLite master.
async fn legacy_variants(covers: &CoverStore, asset: &str) -> Result<Vec<(u32, Vec<u8>)>> {
    let Some(legacy) = covers.legacy() else {
        return Ok(Vec::new());
    };
    let rows: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT width, webp FROM cover_variant WHERE asset_key = $1")
            .bind(asset)
            .fetch_all(legacy)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(w, webp)| {
            let w = u32::try_from(w).ok()?;
            cover::VARIANT_WIDTHS.contains(&w).then_some((w, webp))
        })
        .collect())
}

async fn stage_all(
    covers: &CoverStore,
    master: &[u8],
    variants: &[(u32, Vec<u8>)],
) -> Result<(Staged, Vec<(u32, Staged)>)> {
    let master = covers.stage(master).await?;
    let mut staged = Vec::with_capacity(variants.len());
    for (w, bytes) in variants {
        staged.push((*w, covers.stage(bytes).await?));
    }
    Ok((master, staged))
}

fn shas(master: &Staged, variants: &[(u32, Staged)]) -> Vec<cover_store::Sha> {
    std::iter::once(master.sha)
        .chain(variants.iter().map(|(_, s)| s.sha))
        .collect()
}

/// Place the derivatives of `master` that the index lacks, while `master` is indexed.
async fn copy_variants(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    covers: &CoverStore,
    asset: &str,
    master: &cover_store::Sha,
    variants: &mut [(u32, Staged)],
) -> Result<()> {
    let indexed = cover_store::row(&mut **tx, asset, Kind::Master).await?;
    if indexed.is_none_or(|row| &row.sha != master) {
        return Ok(());
    }
    for (w, staged) in variants {
        if cover_store::row(&mut **tx, asset, Kind::Width(*w))
            .await?
            .is_some()
        {
            continue;
        }
        covers.place(staged).await?;
        cover_store::put_derivative(tx, asset, *w, staged, master).await?;
    }
    Ok(())
}

/// Copy a work master (and its derivatives) into a gap. The pointer follows the copied
/// version when it is unset or older, never lower, with the card URLs.
async fn copy_work(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
    webp: &[u8],
    version: i64,
    variants: &[(u32, Vec<u8>)],
) -> Result<bool> {
    let (mut master, mut variants) = stage_all(covers, webp, variants).await?;
    let mut tx = main.begin().await?;
    let _guards = lock_assets_and_files(&mut tx, &[work_id], &shas(&master, &variants)).await?;
    if !crate::locks::lock_works(&mut tx, &[work_id])
        .await?
        .missing
        .is_empty()
    {
        tx.rollback().await?;
        return Ok(false);
    }
    let taken_down: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM media_takedown WHERE subject_kind = 'work' AND subject_id = $1)",
    )
    .bind(work_id)
    .fetch_one(&mut *tx)
    .await?;
    if taken_down {
        tx.rollback().await?;
        return Ok(false);
    }
    let inserted = cover_store::row(&mut *tx, work_id, Kind::Master)
        .await?
        .is_none();
    if inserted {
        covers.place(&mut master).await?;
        cover_store::put_master(&mut tx, work_id, &master, Some(version)).await?;
        let published: Option<i64> =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id = $1")
                .bind(work_id)
                .fetch_one(&mut *tx)
                .await?;
        if published.is_none_or(|p| p < version) {
            cover::publish_work_cover_version(&mut tx, work_id, version).await?;
        }
    }
    copy_variants(&mut tx, covers, work_id, &master.sha, &mut variants).await?;
    tx.commit().await?;
    Ok(inserted)
}

/// Copy a numeric master (and its derivatives) into a gap. `None`: the engine generation
/// is no longer the one the batch started under.
async fn copy_source(
    main: &Database,
    covers: &CoverStore,
    manga_id: i64,
    webp: &[u8],
    variants: &[(u32, Vec<u8>)],
    generation: Option<uuid::Uuid>,
) -> Result<Option<bool>> {
    let key = cover::suwayomi_variant_key(manga_id);
    let (mut master, mut variants) = stage_all(covers, webp, variants).await?;
    let mut tx = main.begin().await?;
    match crate::engine_generation::shared(&mut tx, generation).await {
        Ok(_) => {}
        Err(e) if e.is::<crate::engine_generation::FenceError>() => {
            tx.rollback().await?;
            return Ok(None);
        }
        Err(e) => return Err(e),
    }
    let _guards = lock_assets_and_files(&mut tx, &[&key], &shas(&master, &variants)).await?;
    if crate::media_policy::engine_series_taken_down(&mut *tx, manga_id).await? {
        tx.rollback().await?;
        return Ok(Some(false));
    }
    let inserted = cover_store::row(&mut *tx, &key, Kind::Master)
        .await?
        .is_none();
    if inserted {
        covers.place(&mut master).await?;
        cover_store::put_master(&mut tx, &key, &master, None).await?;
    }
    copy_variants(&mut tx, covers, &key, &master.sha, &mut variants).await?;
    if crate::engine_generation::commit_ready(main).is_err() {
        tx.rollback().await?;
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(inserted))
}

/// Copy an icon (or a recorded miss: empty bytes) into a gap, keeping its age so a
/// miss still expires on time.
async fn copy_icon(
    main: &Database,
    covers: &CoverStore,
    pkg: &str,
    png: &[u8],
    updated_at: DateTime<Utc>,
) -> Result<bool> {
    let asset = cover_store::icon_asset(pkg);
    let mut staged = covers.stage(png).await?;
    let mut tx = main.begin().await?;
    let _guards = lock_assets_and_files(&mut tx, &[&asset], &[staged.sha]).await?;
    if cover_store::row(&mut *tx, &asset, Kind::Master)
        .await?
        .is_some()
    {
        tx.rollback().await?;
        return Ok(false);
    }
    covers.place(&mut staged).await?;
    cover_store::put_master(&mut tx, &asset, &staged, None).await?;
    sqlx::query("UPDATE cover_object SET updated_at = $2 WHERE asset_key = $1 AND kind = 'master'")
        .bind(cover_store::index_key(&asset))
        .bind(updated_at)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// SQLite timestamps here are `datetime('now')` (icons) or RFC 3339 (covers), both UTC.
fn sqlite_time(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S").map(|t| t.and_utc()))
        .unwrap_or_else(|_| Utc::now())
}

async fn advance(main: &Database, covers: &CoverStore, phase: CopyPhase) -> Result<Batch> {
    let cursor = CopyCursor {
        phase,
        after: String::new(),
    };
    cover_store::save_cursor(main, &cursor).await?;
    covers.set_copy_cursor(cursor);
    Ok(Batch::default())
}

/// End of the walk. A recompression pass the SQLite cache had finished stays finished.
async fn finish(main: &Database, covers: &CoverStore) -> Result<Batch> {
    if let Some(legacy) = covers.legacy() {
        let done: Option<i64> =
            sqlx::query_scalar("SELECT done FROM cover_maintenance WHERE key = $1")
                .bind(RECOMPRESS_SETTING)
                .fetch_optional(legacy)
                .await
                .unwrap_or(None);
        if done.is_some_and(|d| d != 0) {
            sqlx::query(
                "INSERT INTO instance_setting (key, value, updated_at) VALUES ($1, $2, now()) \
                 ON CONFLICT (key) DO NOTHING",
            )
            .bind(RECOMPRESS_SETTING)
            .bind(r#"{"after":"","done":true}"#)
            .execute(main)
            .await?;
        }
    }
    // Every asset is copied, so pointers the SQLite cache could not back (a lost master)
    // now point at nothing. Clear them before marking the copy done: a failure here
    // retries on the next batch instead of waiting for a restart.
    let repaired = crate::db::reconcile_cover_pointers(main).await?;
    if repaired > 0 {
        tracing::info!(repaired, "cover pointers reconciled with the file index");
    }
    let cursor = CopyCursor::done();
    cover_store::save_cursor(main, &cursor).await?;
    covers.set_copy_cursor(cursor);
    tracing::info!("cover copy finished; covers.sqlite3 is no longer used for covers");
    Ok(Batch {
        done: true,
        ..Batch::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cover_store::test_support::{legacy_pool, store_with_legacy, TestDir};
    use crate::job_lease::{JobFamily, JobLease};

    #[tokio::test]
    async fn held_cover_copy_lease_skips_tick_without_advancing_cursor() {
        let main = crate::database::TestPoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(main.write())
            .await
            .unwrap();
        let dir = TestDir::new();
        let covers = store_with_legacy(&dir, legacy_pool().await, CopyCursor::default());
        let held = JobLease::try_acquire(&main, JobFamily::CoverCopy)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(tick(&main, &covers).await.unwrap(), None);
        assert_eq!(covers.copy_cursor(), CopyCursor::default());
        assert!(cover_store::load_cursor(&main).await.unwrap().is_none());

        held.release().await;
        assert_eq!(tick(&main, &covers).await.unwrap(), Some(Batch::default()));
        assert_eq!(covers.copy_cursor().phase, CopyPhase::Source);
    }
}
