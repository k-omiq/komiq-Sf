//! One-shot recompression of cover masters left over from the LOSSLESS era.
//!
//! # Why this exists
//!
//! `cover::process_cover` encodes lossy WebP today, but the covers written before that
//! switch are lossless (`VP8L`) and run 150–200 KB for art that lossy encodes in 20–40
//! KB. A live measurement of one home page put 1.4 MB of its 1.86 MB "Improve image
//! delivery" budget on exactly these blobs — a single 361x510 cover weighing 185 KB.
//!
//! Migration 0065 explicitly deferred this, on the grounds that re-materializing ~88k
//! covers means ~71 GB of MangaDex refetch traffic. **That premise does not hold.** A
//! lossless master is a pixel-exact copy of the downscaled source, so the bytes needed
//! to produce the lossy master are already on our own disk: decode the stored file and
//! re-encode it. No network, no rate limiter, no upstream politeness budget — just CPU.
//!
//! # Shape of the pass
//!
//! The one owner server runs this after the SQLite copy finishes. It walks work masters
//! in the PostgreSQL file index by asset key, a bounded batch per tick:
//!
//!  * Only the first 16 bytes of each object file are read initially; a master already
//!    encoded `VP8 ` is dismissed without reading its full contents.
//!  * Everything else is re-encoded through `process_cover`, which is a no-op resize
//!    here (the stored master is already inside the edge ladder's top rung) and so only
//!    changes the ENCODING, never the dimensions.
//!  * The result is kept only when it is meaningfully smaller ([`MIN_SHRINK_PCT`]), so a
//!    cover that does not benefit is left exactly as it was.
//!
//! Writes go through `cover::put_work_cover_if_current`, which checks the old master,
//! drops the stale derivatives, keeps uploaded bytes in step and
//! bumps `work.cover_cached_version`. That version bump is what makes the saving VISIBLE:
//! `/covers/{id}.webp` is served `immutable` for a year, so without a fresh `?v=` the
//! edge would keep handing out the old fat bytes.
//!
//! # Why a durable cursor rather than a predicate
//!
//! "Still lossless" needs the file's header, not just its index row. A monotonic cursor
//! in `instance_setting` visits each row exactly once, survives restarts, and lets the
//! pass declare itself finished and go quiet.
//!
//! # Scope
//!
//! Work masters only. Numeric source masters are deliberately untouched: those masters
//! are already lossy (the warmer that writes them postdates the switch), AND their public
//! URL — `/api/v1/manga/{id}/thumbnail` — carries no version token, so a rewrite there
//! could not bust the year-long edge entry anyway. Rewriting them would spend CPU for a
//! saving no client would see for a year.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::db::Database;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use crate::cover;
use crate::cover_store::{self, CoverStore};

/// Keep a re-encode only when it saves at least this share of the original. A cover that
/// barely shrinks is not worth a new `?v=` (which costs every client a refetch of bytes
/// it already has) nor the write amplification in a 20 GB store.
const MIN_SHRINK_PCT: u64 = 15;

/// …and at least this many bytes. Guards the tail where a percentage is technically met
/// on an already-small cover: 15% of a 9 KB blob is not worth a cache bust.
const MIN_SHRINK_BYTES: usize = 8 * 1024;

/// Outcome of one tick, for logging and for the loop's "am I finished?" test.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Rows read this tick.
    pub scanned: usize,
    /// Rows re-encoded and written back.
    pub rewritten: usize,
    /// Bytes the rewritten rows gave back.
    pub bytes_saved: u64,
    /// True when the walk reached the end of the table.
    pub finished: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Cursor {
    after: String,
    done: bool,
}

/// Read the stored cursor. A missing row means "never run" — start from before every
/// possible `work_id`, i.e. the empty string.
async fn load_cursor(main: &Database) -> Result<Cursor> {
    let value: Option<String> =
        sqlx::query_scalar("SELECT value FROM instance_setting WHERE key = $1")
            .bind(crate::cover_copy::RECOMPRESS_SETTING)
            .fetch_optional(main)
            .await?;
    Ok(value
        .map(|value| serde_json::from_str(&value))
        .transpose()?
        .unwrap_or_default())
}

/// Persist the cursor (and, at the end of the walk, the terminal `done` flag).
async fn save_cursor(main: &Database, cursor: &str, done: bool) -> Result<()> {
    sqlx::query(
        "INSERT INTO instance_setting (key, value, updated_at) VALUES ($1, $2, now()) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
         updated_at = excluded.updated_at",
    )
    .bind(crate::cover_copy::RECOMPRESS_SETTING)
    .bind(serde_json::to_string(&Cursor {
        after: cursor.to_owned(),
        done,
    })?)
    .execute(main)
    .await?;
    Ok(())
}

/// Walk one batch of masters forward from the stored cursor.
///
/// Returns `Progress::finished` once the table is exhausted; the caller stops ticking.
/// Never returns an error for a single bad row — a master that will not decode is left
/// alone and the cursor moves past it, because a cover we cannot re-encode is still a
/// cover we can serve.
pub async fn run_batch(main: &Database, covers: &CoverStore, batch: i64) -> Result<Progress> {
    if !covers.copy_done() {
        return Ok(Progress::default());
    }
    let cursor = load_cursor(main).await?;
    if cursor.done {
        return Ok(Progress {
            finished: true,
            ..Default::default()
        });
    }
    let start = cursor.after;

    let masters: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT asset_key, sha256 FROM cover_object \
         WHERE kind = 'master' AND asset_key LIKE 'w:%' AND asset_key > $1 \
         ORDER BY asset_key LIMIT $2",
    )
    .bind(format!("w:{start}"))
    .bind(batch)
    .fetch_all(main)
    .await?;

    if masters.is_empty() {
        save_cursor(main, &start, true).await?;
        return Ok(Progress {
            finished: true,
            ..Default::default()
        });
    }

    let mut progress = Progress {
        scanned: masters.len(),
        ..Default::default()
    };
    let mut cursor = start;
    for (asset, sha) in masters {
        let work_id = cover_store::asset_of(&asset).to_owned();
        cursor = work_id.clone();
        let sha = cover_store::Sha::try_from(sha.as_slice())?;
        // Already-lossy files need only a header read, even when their full bytes are large.
        let file = match tokio::fs::File::open(covers.object_path(&sha)).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let mut header = Vec::with_capacity(16);
        file.take(16).read_to_end(&mut header).await?;
        if cover::webp_is_lossy(&header) {
            continue;
        }
        // A file reclaimed since the index or header read is simply stepped over.
        let Some(master) = covers.read_object(&sha).await? else {
            continue;
        };
        let original_len = master.len();
        // Decode + re-encode is CPU-bound; keep it off the async runtime exactly like
        // every other cover resize. The ladder's top edge (512) is >= the stored master's
        // longest edge, so the scale clamps to 1.0 and this only re-encodes.
        let expected = master.clone();
        let encoded = match tokio::task::spawn_blocking(move || cover::process_cover(&master)).await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => {
                // Leave the row alone and move on. Most likely a master this build's
                // decoder cannot read; it still serves fine as stored bytes.
                let e = crate::db::observed_database_error(e);
                tracing::warn!(error = %e, work_id, "cover recompress: re-encode failed");
                continue;
            }
            Err(e) => {
                let e = crate::db::observed_database_error(e.into());
                tracing::error!(error = %e, work_id, "cover recompress: re-encode task panicked");
                continue;
            }
        };
        let shrink = original_len.saturating_sub(encoded.len());
        let enough = shrink >= MIN_SHRINK_BYTES
            && (shrink as u64) * 100 >= (original_len as u64) * MIN_SHRINK_PCT;
        if !enough {
            continue;
        }
        match cover::put_work_cover_if_current(main, covers, &work_id, &encoded, Some(&expected))
            .await
        {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                crate::db::observe_database_error(&e);
                tracing::warn!(error = %e, work_id, "cover recompress: store failed");
                continue;
            }
        }
        progress.rewritten += 1;
        progress.bytes_saved += shrink as u64;
    }

    // Cursor last: a crash mid-batch simply replays the batch, and every step in it is
    // idempotent (a row already rewritten now reads as `VP8 ` and is skipped).
    save_cursor(main, &cursor, false).await?;
    // Every rewrite minted a new `cover_cached_version`, so every memoized `coverUrl` for
    // one of these works still carries the PREVIOUS `?v=`. That URL is already in the edge
    // and browser caches behind a one-year `immutable` entry holding the fat, pre-recompress
    // bytes — so until the memo turns over, the work we just did is invisible to anyone who
    // has seen the page before. Drop it here rather than waiting out `FEED_TTL`.
    if progress.rewritten > 0 {
        crate::graphql::clear_feed_caches();
    }
    Ok(progress)
}

/// Recurring, self-terminating background pass. Ticks every `interval_secs` until the
/// walk completes, then stays idle for the rest of the process's life.
///
/// The owner server publishes the files; this pass waits until the copy finishes.
pub fn spawn(
    main: Database,
    covers: CoverStore,
    interval_secs: u64,
    batch: i64,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(crate::task::supervise(
        "cover-recompress",
        Duration::from_secs(30),
        shutdown.clone(),
        move || {
            run_loop(
                main.clone(),
                covers.clone(),
                interval_secs,
                batch,
                shutdown.clone(),
            )
        },
    ));
}

async fn run_loop(
    main: Database,
    covers: CoverStore,
    interval_secs: u64,
    batch: i64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Start 210s in, behind the scanner (0s), the cover drainer (90s) and the GC (150s),
    // so the boot burst stays spread across the background loops rather than landing in
    // one instant on the database writer.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(210),
        Duration::from_secs(interval_secs),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Latched by the first `finished` tick. A supervised restart re-reads the durable
    // `done` flag, so a panic cannot make the pass start over.
    let finished = AtomicBool::new(false);
    tracing::info!(interval_secs, batch, "cover recompress pass started");
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if finished.load(Ordering::Relaxed) {
                    continue;
                }
                match run_batch(&main, &covers, batch).await {
                    Ok(p) if p.finished => {
                        finished.store(true, Ordering::Relaxed);
                        tracing::info!("cover recompress pass complete — no lossless masters left");
                    }
                    Ok(p) => tracing::info!(
                        scanned = p.scanned,
                        rewritten = p.rewritten,
                        mib_saved = p.bytes_saved / (1024 * 1024),
                        "cover recompress tick"
                    ),
                    Err(e) => { let e = crate::db::observed_database_error(e); tracing::warn!(error = %e, "cover recompress tick failed"); },
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("cover recompress pass stopping");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cover_store::test_support::{self, TestDir};
    use image::{DynamicImage, RgbImage};

    /// High-entropy art, so lossless encoding is genuinely expensive and the lossy
    /// re-encode has a real saving to find (a flat test image compresses losslessly to
    /// almost nothing and would never clear `MIN_SHRINK_*`).
    fn noisy_png(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let r = ((x * 73 + y * 151) % 256) as u8;
            let g = ((x * 199 + y * 37) % 256) as u8;
            let b = ((x ^ (y.wrapping_mul(101))) % 256) as u8;
            *px = image::Rgb([r, g, b]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("png encodes");
        bytes
    }

    /// A cover exactly as the pre-switch pipeline stored it: downscaled to the edge
    /// ladder's top rung, encoded LOSSLESS.
    fn legacy_lossless_master(w: u32, h: u32) -> Vec<u8> {
        let img = image::load_from_memory(&noisy_png(w, h)).expect("decodes");
        let long = img.width().max(img.height()).max(1);
        let scale = (512.0_f32 / long as f32).min(1.0);
        let rgba = image::imageops::resize(
            &img.to_rgba8(),
            ((img.width() as f32 * scale).round() as u32).max(1),
            ((img.height() as f32 * scale).round() as u32).max(1),
            image::imageops::FilterType::Lanczos3,
        );
        crate::avatar::encode_lossless(&rgba).expect("lossless encodes")
    }

    /// Main DB with the version pointer and materialized URLs `put_work_cover` publishes.
    async fn main_db() -> Database {
        let pool = crate::database::TestPoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        pool
    }

    async fn insert_master(main: &Database, covers: &CoverStore, id: &str, webp: &[u8]) {
        sqlx::query("INSERT INTO work (id, created_at, updated_at) VALUES ($1, now(), now())")
            .bind(id)
            .execute(main)
            .await
            .unwrap();
        cover::put_work_cover(main, covers, id, webp).await.unwrap();
    }

    async fn stored(main: &Database, covers: &CoverStore, id: &str) -> cover_store::Master {
        covers.read_master(main, id, None).await.unwrap().unwrap().0
    }

    #[tokio::test]
    async fn a_lossless_master_is_re_encoded_and_gets_a_fresh_version() {
        let main = main_db().await;
        let dir = TestDir::new();
        let covers = test_support::store(&dir);
        let legacy = legacy_lossless_master(600, 900);
        insert_master(&main, &covers, "w_a", &legacy).await;
        let before = stored(&main, &covers, "w_a").await;
        // A derivative of the OLD master, which must not survive the rewrite.
        cover::put_current_variant(&main, &covers, "w_a", 192, &before, &[0])
            .await
            .unwrap();
        assert!(
            cover_store::row(&main, "w_a", cover_store::Kind::Width(192))
                .await
                .unwrap()
                .is_some()
        );

        let p = run_batch(&main, &covers, 10).await.unwrap();
        assert_eq!(p.rewritten, 1, "the lossless master must be rewritten");

        let now = stored(&main, &covers, "w_a").await.bytes;
        assert!(cover::webp_is_lossy(&now), "rewritten as lossy WebP");
        assert!(
            now.len() * 2 < legacy.len(),
            "expected a large saving, got {} from {} bytes",
            now.len(),
            legacy.len()
        );

        // Same art, same dimensions — this pass re-encodes, it never resizes.
        let dims = |b: &[u8]| {
            let i = image::load_from_memory(b).expect("decodes");
            (i.width(), i.height())
        };
        assert_eq!(dims(&now), dims(&legacy), "dimensions must not change");

        // The `?v=` bust is the whole point: without it the year-long immutable edge entry
        // keeps serving the old fat bytes.
        let version: i64 =
            crate::db_types::query_scalar("SELECT cover_cached_version FROM work WHERE id = 'w_a'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert!(
            Some(version) > before.version,
            "cover_cached_version must be bumped"
        );

        let variants: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM cover_object WHERE asset_key = 'w:w_a' AND kind <> 'master'",
        )
        .fetch_one(&main)
        .await
        .unwrap();
        assert_eq!(variants, 0, "derivatives of the old master must be dropped");
    }

    #[tokio::test]
    async fn an_already_lossy_master_is_left_alone() {
        let main = main_db().await;
        let dir = TestDir::new();
        let covers = test_support::store(&dir);
        let modern = cover::process_cover(&noisy_png(600, 900)).unwrap();
        insert_master(&main, &covers, "w_a", &modern).await;
        let before = stored(&main, &covers, "w_a").await;

        let p = run_batch(&main, &covers, 10).await.unwrap();
        assert_eq!(p.scanned, 1);
        assert_eq!(p.rewritten, 0, "a lossy master needs no work");
        assert_eq!(
            stored(&main, &covers, "w_a").await.bytes,
            modern,
            "bytes untouched"
        );
        let version: i64 =
            crate::db_types::query_scalar("SELECT cover_cached_version FROM work WHERE id = 'w_a'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert_eq!(Some(version), before.version, "no needless cache bust");
    }

    #[tokio::test]
    async fn the_walk_resumes_then_terminates() {
        let main = main_db().await;
        let dir = TestDir::new();
        let covers = test_support::store(&dir);
        let legacy = legacy_lossless_master(600, 900);
        for id in ["w_a", "w_b", "w_c"] {
            insert_master(&main, &covers, id, &legacy).await;
        }

        // Batch of one: three ticks to cover three rows, each resuming where the last
        // stopped rather than restarting at the top.
        for expected in ["w_a", "w_b", "w_c"] {
            let p = run_batch(&main, &covers, 1).await.unwrap();
            assert_eq!(p.scanned, 1, "one row per tick");
            assert_eq!(p.rewritten, 1);
            assert!(!p.finished);
            let cursor = load_cursor(&main).await.unwrap();
            assert_eq!(cursor.after, expected, "cursor advances one row at a time");
            assert!(!cursor.done);
        }

        let p = run_batch(&main, &covers, 1).await.unwrap();
        assert!(p.finished, "the walk must declare itself finished");
        assert!(load_cursor(&main).await.unwrap().done);
        // …and stay finished, so the loop goes quiet instead of re-reading 20 GB forever.
        let again = run_batch(&main, &covers, 1).await.unwrap();
        assert_eq!(
            again,
            Progress {
                finished: true,
                ..Default::default()
            },
            "a finished pass must not scan again"
        );
    }

    #[tokio::test]
    async fn a_master_that_barely_shrinks_keeps_its_bytes_and_its_version() {
        let main = main_db().await;
        let dir = TestDir::new();
        let covers = test_support::store(&dir);
        // Flat art: lossless already encodes it in a few hundred bytes, so the lossy
        // re-encode cannot clear MIN_SHRINK_BYTES. Rewriting it would spend a cache bust
        // (every client refetches) to save nothing.
        let flat = crate::avatar::encode_lossless(&image::RgbaImage::from_pixel(
            400,
            600,
            image::Rgba([200, 40, 60, 255]),
        ))
        .unwrap();
        insert_master(&main, &covers, "w_a", &flat).await;
        let before = stored(&main, &covers, "w_a").await;

        let p = run_batch(&main, &covers, 10).await.unwrap();
        assert_eq!(p.scanned, 1);
        assert_eq!(p.rewritten, 0, "not worth a cache bust");
        let after = stored(&main, &covers, "w_a").await;
        assert_eq!(after.bytes, flat, "bytes untouched");
        assert_eq!(after.version, before.version, "no needless cache bust");
    }

    #[tokio::test]
    async fn waits_for_the_copy_before_scanning_or_advancing() {
        let main = main_db().await;
        let dir = TestDir::new();
        let covers = test_support::store_with_legacy(
            &dir,
            test_support::legacy_pool().await,
            cover_store::CopyCursor::default(),
        );
        let legacy = legacy_lossless_master(600, 900);
        insert_master(&main, &covers, "w_a", &legacy).await;

        assert_eq!(
            run_batch(&main, &covers, 10).await.unwrap(),
            Progress::default()
        );
        assert_eq!(stored(&main, &covers, "w_a").await.bytes, legacy);
        let cursor: Option<String> =
            sqlx::query_scalar("SELECT value FROM instance_setting WHERE key = $1")
                .bind(crate::cover_copy::RECOMPRESS_SETTING)
                .fetch_optional(&main)
                .await
                .unwrap();
        assert!(cursor.is_none(), "waiting must not advance the cursor");

        covers.set_copy_cursor(cover_store::CopyCursor::done());
        assert_eq!(run_batch(&main, &covers, 10).await.unwrap().rewritten, 1);
    }

    #[tokio::test]
    async fn recompressing_an_upload_keeps_its_backup_equal_to_the_master() {
        let main = main_db().await;
        let dir = TestDir::new();
        let covers = test_support::store(&dir);
        sqlx::query("INSERT INTO work (id, created_at, updated_at) VALUES ('w_a', now(), now())")
            .execute(&main)
            .await
            .unwrap();
        let legacy = legacy_lossless_master(600, 900);
        assert!(
            cover::put_uploaded_work_cover(&main, &covers, "w_a", &legacy)
                .await
                .unwrap()
        );

        assert_eq!(run_batch(&main, &covers, 10).await.unwrap().rewritten, 1);
        let master = stored(&main, &covers, "w_a").await;
        assert!(cover::webp_is_lossy(&master.bytes));
        assert_ne!(master.bytes, legacy);
        let (bytes, sha): (Vec<u8>, Vec<u8>) =
            sqlx::query_as("SELECT bytes, sha256 FROM cover_upload WHERE work_id = 'w_a'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert_eq!(bytes, master.bytes);
        assert_eq!(sha.as_slice(), master.sha.as_slice());
        assert_eq!(cover_store::sha256(&bytes), master.sha);
    }
}
