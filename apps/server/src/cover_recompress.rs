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
//! to produce the lossy master are already on our own disk: decode the stored blob and
//! re-encode it. No network, no rate limiter, no upstream politeness budget — just CPU.
//!
//! # Shape of the pass
//!
//! A cursor walk over `work_cover_blob` by primary key, a bounded batch per tick:
//!
//!  * The batch is read as HEADERS ONLY (`substr(webp, 1, 16)`), which lets SQLite skip
//!    the overflow pages holding the actual image; a row already encoded `VP8 ` is
//!    dismissed without its ~170 KB ever leaving the disk.
//!  * Everything else is re-encoded through `process_cover`, which is a no-op resize
//!    here (the stored master is already inside the edge ladder's top rung) and so only
//!    changes the ENCODING, never the dimensions.
//!  * The result is kept only when it is meaningfully smaller ([`MIN_SHRINK_PCT`]), so a
//!    cover that does not benefit is left exactly as it was.
//!
//! Writes go through `cover::put_work_cover`, which drops the stale derivatives and
//! bumps `work.cover_cached_version`. That version bump is what makes the saving VISIBLE:
//! `/covers/{id}.webp` is served `immutable` for a year, so without a fresh `?v=` the
//! edge would keep handing out the old fat bytes.
//!
//! # Why a durable cursor rather than a predicate
//!
//! "Still lossless" is not cheaply expressible in SQL — it needs the blob's 13th byte,
//! and `substr()` over a 20 GB store would re-read every candidate on every tick. A
//! monotonic cursor visits each row exactly once, survives restarts, and lets the pass
//! declare itself finished and go quiet.
//!
//! # Scope
//!
//! `work_cover_blob` only. `suwayomi_cover_blob` is deliberately untouched: those masters
//! are already lossy (the warmer that writes them postdates the switch), AND their public
//! URL — `/api/v1/manga/{id}/thumbnail` — carries no version token, so a rewrite there
//! could not bust the year-long edge entry anyway. Rewriting them would spend CPU for a
//! saving no client would see for a year.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use sqlx::SqlitePool;

use crate::cover;

/// `cover_maintenance.key` for this pass.
const CURSOR_KEY: &str = "work_cover_recompress";

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

/// Read the stored cursor. A missing row means "never run" — start from before every
/// possible `work_id`, i.e. the empty string.
async fn load_cursor(covers: &SqlitePool) -> Result<Option<String>> {
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT cursor, done FROM cover_maintenance WHERE key = ?")
            .bind(CURSOR_KEY)
            .fetch_optional(covers)
            .await?;
    Ok(match row {
        // `done` is terminal: the walk reached the end, so there is nothing left to do.
        Some((_, done)) if done != 0 => None,
        Some((cursor, _)) => Some(cursor),
        None => Some(String::new()),
    })
}

/// Persist the cursor (and, at the end of the walk, the terminal `done` flag).
async fn save_cursor(covers: &SqlitePool, cursor: &str, done: bool) -> Result<()> {
    sqlx::query(
        "INSERT INTO cover_maintenance (key, cursor, done, updated_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET cursor = excluded.cursor, done = excluded.done, \
         updated_at = excluded.updated_at",
    )
    .bind(CURSOR_KEY)
    .bind(cursor)
    .bind(i64::from(done))
    .bind(Utc::now().to_rfc3339())
    .execute(covers)
    .await?;
    Ok(())
}

/// Walk one batch of masters forward from the stored cursor.
///
/// Returns `Progress::finished` once the table is exhausted; the caller stops ticking.
/// Never returns an error for a single bad row — a master that will not decode is left
/// alone and the cursor moves past it, because a cover we cannot re-encode is still a
/// cover we can serve.
pub async fn run_batch(main: &SqlitePool, covers: &SqlitePool, batch: i64) -> Result<Progress> {
    let Some(start) = load_cursor(covers).await? else {
        return Ok(Progress {
            finished: true,
            ..Default::default()
        });
    };

    // Headers only. `substr(blob, 1, N)` lets SQLite answer from the row's first page
    // without walking the overflow chain, so a tick over already-converted rows costs
    // kilobytes instead of tens of megabytes — which is what makes a large `batch` cheap
    // and a re-run of a finished region nearly free.
    let headers: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT work_id, substr(webp, 1, 16) FROM work_cover_blob \
         WHERE work_id > ? ORDER BY work_id LIMIT ?",
    )
    .bind(&start)
    .bind(batch)
    .fetch_all(covers)
    .await?;

    if headers.is_empty() {
        save_cursor(covers, &start, true).await?;
        return Ok(Progress {
            finished: true,
            ..Default::default()
        });
    }

    let mut progress = Progress {
        scanned: headers.len(),
        ..Default::default()
    };
    let mut cursor = start;
    for (work_id, header) in headers {
        cursor = work_id.clone();
        if cover::webp_is_lossy(&header) {
            continue;
        }
        // Only now is the full master worth reading. A row that vanished since the header
        // scan (the GC reclaiming an orphan) simply yields nothing and is stepped over.
        let Some(master): Option<Vec<u8>> =
            sqlx::query_scalar("SELECT webp FROM work_cover_blob WHERE work_id = ?")
                .bind(&work_id)
                .fetch_optional(covers)
                .await?
        else {
            continue;
        };
        let original_len = master.len();
        // Decode + re-encode is CPU-bound; keep it off the async runtime exactly like
        // every other cover resize. The ladder's top edge (512) is >= the stored master's
        // longest edge, so the scale clamps to 1.0 and this only re-encodes.
        let encoded = match tokio::task::spawn_blocking(move || cover::process_cover(&master)).await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => {
                // Leave the row alone and move on. Most likely a master this build's
                // decoder cannot read; it still serves fine as stored bytes.
                tracing::warn!(error = %e, work_id, "cover recompress: re-encode failed");
                continue;
            }
            Err(e) => {
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
        if let Err(e) = cover::put_work_cover(main, covers, &work_id, &encoded).await {
            tracing::warn!(error = %e, work_id, "cover recompress: store failed");
            continue;
        }
        progress.rewritten += 1;
        progress.bytes_saved += shrink as u64;
    }

    // Cursor last: a crash mid-batch simply replays the batch, and every step in it is
    // idempotent (a row already rewritten now reads as `VP8 ` and is skipped).
    save_cursor(covers, &cursor, false).await?;
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
/// Unlike the cover drainer this touches NO external service, so it is safe on every
/// replica: each one owns its own `covers.sqlite3` and has its own leftovers to fix. The
/// only shared write is `work.cover_cached_version`, where two replicas agreeing on
/// "some fresh version" is all the correctness the `?v=` bust needs.
pub fn spawn(
    main: SqlitePool,
    covers: SqlitePool,
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
    main: SqlitePool,
    covers: SqlitePool,
    interval_secs: u64,
    batch: i64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Start 210s in, behind the scanner (0s), the cover drainer (90s) and the GC (150s),
    // so the boot burst stays spread across the background loops rather than landing in
    // one instant on SQLite's single writer.
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
                    Err(e) => tracing::warn!(error = %e, "cover recompress tick failed"),
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
    async fn main_db() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE work (id TEXT PRIMARY KEY, cover_cached_version INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
            sqlx::query(&format!(
                "CREATE TABLE {table} (work_id TEXT PRIMARY KEY, cover_url TEXT)"
            ))
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    /// Covers DB with the two tables this pass reads and writes.
    async fn covers_db() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        for ddl in [
            "CREATE TABLE work_cover_blob (work_id TEXT PRIMARY KEY, webp BLOB NOT NULL, \
             version INTEGER NOT NULL, updated_at TEXT NOT NULL)",
            "CREATE TABLE cover_variant (asset_key TEXT NOT NULL, width INTEGER NOT NULL, \
             webp BLOB NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (asset_key, width))",
            "CREATE TABLE cover_maintenance (key TEXT PRIMARY KEY, cursor TEXT NOT NULL, \
             done INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL)",
        ] {
            sqlx::query(ddl).execute(&pool).await.unwrap();
        }
        pool
    }

    async fn insert_master(main: &SqlitePool, covers: &SqlitePool, id: &str, webp: &[u8]) {
        sqlx::query("INSERT INTO work (id, cover_cached_version) VALUES (?, 1)")
            .bind(id)
            .execute(main)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO work_cover_blob (work_id, webp, version, updated_at) \
             VALUES (?, ?, 1, '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(webp)
        .execute(covers)
        .await
        .unwrap();
    }

    async fn stored(covers: &SqlitePool, id: &str) -> Vec<u8> {
        sqlx::query_scalar("SELECT webp FROM work_cover_blob WHERE work_id = ?")
            .bind(id)
            .fetch_one(covers)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_lossless_master_is_re_encoded_and_gets_a_fresh_version() {
        let (main, covers) = (main_db().await, covers_db().await);
        let legacy = legacy_lossless_master(600, 900);
        insert_master(&main, &covers, "w_a", &legacy).await;
        // A derivative of the OLD master, which must not survive the rewrite.
        sqlx::query(
            "INSERT INTO cover_variant (asset_key, width, webp, updated_at) \
             VALUES ('w_a', 192, x'00', '2026-01-01T00:00:00Z')",
        )
        .execute(&covers)
        .await
        .unwrap();

        let p = run_batch(&main, &covers, 10).await.unwrap();
        assert_eq!(p.rewritten, 1, "the lossless master must be rewritten");

        let now = stored(&covers, "w_a").await;
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
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id = 'w_a'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert!(version > 1, "cover_cached_version must be bumped");

        let variants: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM cover_variant WHERE asset_key = 'w_a'")
                .fetch_one(&covers)
                .await
                .unwrap();
        assert_eq!(variants, 0, "derivatives of the old master must be dropped");
    }

    #[tokio::test]
    async fn an_already_lossy_master_is_left_alone() {
        let (main, covers) = (main_db().await, covers_db().await);
        let modern = cover::process_cover(&noisy_png(600, 900)).unwrap();
        insert_master(&main, &covers, "w_a", &modern).await;

        let p = run_batch(&main, &covers, 10).await.unwrap();
        assert_eq!(p.scanned, 1);
        assert_eq!(p.rewritten, 0, "a lossy master needs no work");
        assert_eq!(stored(&covers, "w_a").await, modern, "bytes untouched");
        let version: i64 =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id = 'w_a'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert_eq!(version, 1, "no needless cache bust");
    }

    #[tokio::test]
    async fn the_walk_resumes_then_terminates() {
        let (main, covers) = (main_db().await, covers_db().await);
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
            let cursor: String =
                sqlx::query_scalar("SELECT cursor FROM cover_maintenance WHERE key = ?")
                    .bind(CURSOR_KEY)
                    .fetch_one(&covers)
                    .await
                    .unwrap();
            assert_eq!(cursor, expected, "cursor advances one row at a time");
        }

        let p = run_batch(&main, &covers, 1).await.unwrap();
        assert!(p.finished, "the walk must declare itself finished");
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
        let (main, covers) = (main_db().await, covers_db().await);
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

        let p = run_batch(&main, &covers, 10).await.unwrap();
        assert_eq!(p.scanned, 1);
        assert_eq!(p.rewritten, 0, "not worth a cache bust");
        assert_eq!(stored(&covers, "w_a").await, flat, "bytes untouched");
    }
}
