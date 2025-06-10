//! DB-backed cover cache for canonical works.
//!
//! The web reader normally routes every cover through the Cloudflare image Worker
//! (CORS). To take covers off the Worker, we fetch each work's MangaDex cover
//! once, re-encode it to a bounded lossless WebP, and store the bytes in
//! `work_cover_blob` (Litestream-replicated, like `user_avatars`). The reader
//! then loads covers from our own origin at `/covers/{work_id}.webp` and the
//! Worker only ever sees chapter PAGES.
//!
//! Presence is signalled by `work.cover_cached_version` (NULL = not cached), a
//! column that rides on the `work` row already loaded on hot paths, so cover-URL
//! resolution costs no extra query.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use image::{imageops, GenericImageView};
use sqlx::SqlitePool;

use crate::avatar::{decode_limited, encode_webp_lossy};
use crate::mangadex::MangaDexClient;

/// Reject raw source images larger than this before decoding (decode-bomb guard).
/// Raised from 8 MB: some source thumbnails (a handful of Suwayomi scanlator
/// covers) ship 8–20 MB originals that decode fine and downscale to a tiny WebP —
/// the old cap rejected them outright. The real bomb guard is `decode_limited`'s
/// pixel-dimension + allocation ceiling, which still applies after this gate.
pub const MAX_SOURCE_BYTES: usize = 24 * 1024 * 1024;

/// Per-cover size budget for the stored WebP — the "100–150 KB" target. Lossy
/// encoding (below) keeps detailed art sharp at 512px while landing under this,
/// so the DB blob store + its Litestream replication stay cheap. Down from the old
/// 200 KB lossless budget.
pub const MAX_COVER_BYTES: usize = 150 * 1024;

/// Candidate longest-edge lengths (px), largest first — the portrait-preserving
/// dimension bound. With LOSSY encoding the quality ladder does most of the size
/// control, so the top edge (512) almost always wins; the smaller edges are a
/// fallback for pathological sources that won't fit even at low quality.
const CANDIDATE_EDGES: [u32; 3] = [512, 448, 384];

/// Lossy WebP quality ladder (libwebp scale, 0–100), highest first. The first
/// (edge, quality) pair encoding within [`MAX_COVER_BYTES`] wins, so sharpness is
/// preserved by trying high quality before stepping down.
const COVER_QUALITY_LADDER: [f32; 5] = [82.0, 74.0, 66.0, 58.0, 50.0];

/// Decode an upstream cover, downscale (aspect-preserving, never upscaling) to a
/// bounded longest edge, and re-encode as LOSSY WebP within [`MAX_COVER_BYTES`].
/// Tries each edge at descending quality and returns the first result under budget.
pub fn process_cover(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.is_empty() {
        bail!("empty cover source");
    }
    if bytes.len() > MAX_SOURCE_BYTES {
        bail!(
            "cover source too large (max {} MB)",
            MAX_SOURCE_BYTES / (1024 * 1024)
        );
    }
    // Truncation gate, BEFORE the decode. `zune-jpeg` returns `Ok` for a JPEG that was
    // cut off mid-scan and fills the missing rows with a flat decoder value, so a
    // partial download would otherwise be re-encoded and frozen into the cache behind a
    // one-year immutable TTL. `decode_limited` calls this too; the explicit call here
    // keeps the failure attributable to the cover pipeline (and to
    // `classify_cover_error`'s `truncated` reason) for every `process_cover` caller.
    crate::avatar::ensure_complete(bytes)?;
    let img = decode_limited(bytes)?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        bail!("cover has a zero dimension");
    }
    let long = w.max(h).max(1);
    let rgba = img.to_rgba8();

    let mut smallest: Option<Vec<u8>> = None;
    for edge in CANDIDATE_EDGES {
        // Downscale only — a source already smaller than the candidate keeps its
        // size (scale clamped to 1.0) instead of being blurrily enlarged.
        let scale = (edge as f32 / long as f32).min(1.0);
        let resized = if scale >= 1.0 {
            rgba.clone()
        } else {
            let tw = ((w as f32 * scale).round() as u32).max(1);
            let th = ((h as f32 * scale).round() as u32).max(1);
            imageops::resize(&rgba, tw, th, imageops::FilterType::Lanczos3)
        };
        for &quality in &COVER_QUALITY_LADDER {
            let encoded = encode_webp_lossy(&resized, quality)?;
            if encoded.len() <= MAX_COVER_BYTES {
                return Ok(encoded);
            }
            // Keep the smallest produced (smallest edge, lowest quality) as the
            // last-resort fallback if nothing fits the budget.
            smallest = Some(encoded);
        }
    }
    smallest.ok_or_else(|| anyhow!("no candidate cover size produced"))
}

/// Read a resized, origin-cached Suwayomi source cover (keyed by numeric manga id),
/// or `None` if it hasn't been materialized yet. Errors are swallowed to `None` so a
/// transient DB hiccup falls back to a live resize rather than failing the request.
pub async fn get_suwayomi_cover(covers: &SqlitePool, manga_id: i64) -> Option<Vec<u8>> {
    sqlx::query_scalar("SELECT webp FROM suwayomi_cover_blob WHERE manga_id = ?")
        .bind(manga_id)
        .fetch_optional(covers)
        .await
        .ok()
        .flatten()
}

/// Store (or replace) a resized Suwayomi source cover in the un-replicated covers DB.
pub async fn put_suwayomi_cover(covers: &SqlitePool, manga_id: i64, webp: &[u8]) -> Result<()> {
    let ts = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO suwayomi_cover_blob (manga_id, webp, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(manga_id) DO UPDATE SET webp = excluded.webp, updated_at = excluded.updated_at",
    )
    .bind(manga_id)
    .bind(webp)
    .bind(&ts)
    .execute(covers)
    .await?;
    Ok(())
}

/// The public path stored/served for a cached work cover, cache-busted with
/// `?v=<version>` so the browser + edge refetch after a re-save. The
/// `/covers/{file}` route reads the bytes from `work_cover_blob` keyed by
/// `<work_id>`.
pub fn cover_path(work_id: &str, version: i64) -> String {
    format!("/covers/{work_id}.webp?v={version}")
}

/// Resolve the cover URL for a work. Cached ⇒ the versioned VPS blob path. Uncached
/// but cover-able (has a MangaDex anchor) ⇒ our own `/covers/{id}.webp` route, which
/// lazily fetches + caches the MangaDex cover on first request (and 302-falls back to
/// the CDN if that fails) — so covers land on our origin AS THEY'RE VIEWED, not only
/// via the slow background drainer. No anchor ⇒ empty. This is the single seam every
/// cover-URL site uses so the web/VPS split lives in one place.
pub fn work_cover_url(
    work_id: &str,
    cached_version: Option<i64>,
    mangadex_id: Option<&str>,
    cover_file_name: Option<&str>,
) -> String {
    if let Some(v) = cached_version {
        return cover_path(work_id, v);
    }
    match (mangadex_id, cover_file_name) {
        (Some(mid), Some(fname)) if !mid.is_empty() && !fname.is_empty() => {
            format!("/covers/{work_id}.webp")
        }
        _ => String::new(),
    }
}

/// The MangaDex anchor (`(source_key, cover_file_name)`) for a work — the oldest
/// MangaDex `source_series` (the same anchor the reader/drainer picks) plus the work's
/// cover file name. `None` if the work has no MangaDex source or no cover file name, in
/// which case there's nothing to lazily fetch. Used by `serve_cover`'s lazy cache.
pub async fn mangadex_cover_anchor(main: &SqlitePool, work_id: &str) -> Option<(String, String)> {
    let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT ss.source_key, w.cover_file_name \
         FROM work w \
         JOIN source_series ss ON ss.work_id = w.id AND ss.source_type = 'mangadex' \
         WHERE w.id = ? \
         ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1",
    )
    .bind(work_id)
    .fetch_optional(main)
    .await
    .ok()
    .flatten()?;
    match row {
        (Some(mid), Some(fname)) if !mid.is_empty() && !fname.is_empty() => Some((mid, fname)),
        _ => None,
    }
}

/// Store (or replace) a work's cover blob (in the separate `covers` DB) and flip
/// `work.cover_cached_version` (in the `main` DB) to the new version. The version
/// is a wall-clock timestamp — monotonic enough to bust caches on every re-save.
///
/// The blob and its version pointer live in DIFFERENT databases (covers is
/// un-replicated; see `db::init_covers`), so a single cross-DB transaction is
/// impossible. We preserve the invariant "`cover_cached_version` set ⇒ blob exists"
/// by ORDERING: write the blob first, set the pointer second. A crash in between
/// leaves a blob with no pointer — harmless: the resolver falls back to the proxy
/// URL, and the drainer re-runs (it selects `cover_cached_version IS NULL`) and
/// re-sets the pointer. The reverse order could point at a missing blob → 404.
pub async fn put_work_cover(
    main: &SqlitePool,
    covers: &SqlitePool,
    work_id: &str,
    webp: &[u8],
) -> Result<()> {
    let now = Utc::now();
    let version = now.timestamp();
    let ts = now.to_rfc3339();
    // 1) Blob first, into the un-replicated covers DB.
    sqlx::query(
        "INSERT INTO work_cover_blob (work_id, webp, version, updated_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(work_id) DO UPDATE SET \
           webp = excluded.webp, version = excluded.version, updated_at = excluded.updated_at",
    )
    .bind(work_id)
    .bind(webp)
    .bind(version)
    .bind(&ts)
    .execute(covers)
    .await?;
    // 2) Version pointer second, into the main (replicated) DB.
    sqlx::query("UPDATE work SET cover_cached_version = ? WHERE id = ?")
        .bind(version)
        .bind(work_id)
        .execute(main)
        .await?;
    Ok(())
}

/// A work still missing a cached cover, with the MangaDex anchor needed to fetch
/// one.
struct PendingCover {
    work_id: String,
    mangadex_id: String,
    file_name: String,
}

/// How many canonical works still need a cover materialized (cacheable = has a
/// MangaDex anchor + cover fileName, no blob yet). Drives the admin button's
/// "queued N" feedback.
pub async fn pending_cover_count(pool: &SqlitePool) -> Result<i64> {
    // Uncached works we can materialize a cover for: a MangaDex-anchored work with a
    // cover file name, OR a Suwayomi-anchored work (cover comes from its thumbnail).
    //
    // The `work_cover_issue` exclusion mirrors BOTH crawl SELECTs (see
    // `crawl_uncached_covers`' `BASE` and `SUW_BASE`). Without it this counted works the
    // crawl will never select: measured live, 27 "pending" against 1 actually selectable,
    // because 26 works carry a recorded issue. That made the drainer's `Ok(0) => nothing
    // to do` short-circuit unreachable — it launched a full crawl pass every 300 s to
    // process a single row — and overstated the operator-facing backlog by 27x.
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work w \
         WHERE w.cover_cached_version IS NULL \
           AND w.id NOT IN (SELECT work_id FROM work_cover_issue) AND ( \
             (w.cover_file_name IS NOT NULL \
              AND EXISTS (SELECT 1 FROM source_series ss \
                          WHERE ss.work_id = w.id AND ss.source_type = 'mangadex')) \
             OR EXISTS (SELECT 1 FROM source_series ss \
                        WHERE ss.work_id = w.id AND ss.source_type = 'suwayomi'))",
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Map a `process_cover` / store error to a stable machine reason code for
/// `work_cover_issue.reason` (surfaced in the admin panel).
pub fn classify_cover_error(err: &anyhow::Error) -> &'static str {
    let s = err.to_string().to_ascii_lowercase();
    if s.contains("source too large") {
        "too_large"
    } else if s.contains("truncated") {
        // A partial download, not a property of the source — see `is_transient`.
        "truncated"
    } else if s.contains("unsupported") || s.contains("corrupt") || s.contains("zero dimension") {
        "unsupported"
    } else if s.contains("empty") {
        "empty"
    } else {
        "encode"
    }
}

/// Whether a `process_cover` failure is TRANSIENT — i.e. re-fetching the same work later
/// could plausibly succeed, so it must NOT be written to `work_cover_issue` (which
/// permanently excludes the work from every crawl SELECT).
///
/// `truncated` is the case that matters: the source image is fine, the *download* was
/// cut short. Recording it would turn one flaky CDN response into a permanently
/// coverless work — exactly the failure mode this whole change set exists to remove.
pub fn is_transient_cover_error(err: &anyhow::Error) -> bool {
    classify_cover_error(err) == "truncated"
}

/// Record (or refresh) a cover-processing failure so the crawl stops re-attempting
/// a deterministically-unprocessable work every tick. Best-effort — a failure to
/// write the marker only means we retry next tick, never breaks the crawl.
pub async fn record_cover_issue(pool: &SqlitePool, work_id: &str, reason: &str, detail: &str) {
    let now = Utc::now().to_rfc3339();
    // Truncate an over-long error so a pathological message can't bloat the row.
    let detail: String = detail.chars().take(500).collect();
    if let Err(e) = sqlx::query(
        "INSERT INTO work_cover_issue (work_id, reason, detail, attempts, first_seen, last_seen) \
         VALUES (?, ?, ?, 1, ?, ?) \
         ON CONFLICT(work_id) DO UPDATE SET \
           reason = excluded.reason, detail = excluded.detail, \
           attempts = work_cover_issue.attempts + 1, last_seen = excluded.last_seen",
    )
    .bind(work_id)
    .bind(reason)
    .bind(&detail)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await
    {
        tracing::warn!(work_id, error = %e, "cover issue: failed to record");
    }
}

/// Clear a work's recorded cover issue (a later attempt — auto or manual — succeeded,
/// so it should re-enter the normal cached/uncached flow). Best-effort.
pub async fn clear_cover_issue(pool: &SqlitePool, work_id: &str) {
    if let Err(e) = sqlx::query("DELETE FROM work_cover_issue WHERE work_id = ?")
        .bind(work_id)
        .execute(pool)
        .await
    {
        tracing::warn!(work_id, error = %e, "cover issue: failed to clear");
    }
}

/// Attempt to materialize ONE work's cover on demand (admin "retry" action). Tries
/// the MangaDex anchor first, then the Suwayomi thumbnail. Clears the issue on
/// success; re-records it (refreshed reason) on a deterministic failure; leaves it
/// untouched on a transient upstream-fetch failure. Returns Ok(true) when a cover
/// was stored, Ok(false) when neither source yielded processable bytes.
pub async fn retry_one_cover(
    main: &SqlitePool,
    covers: &SqlitePool,
    mangadex: &MangaDexClient,
    suwayomi: &crate::suwayomi::SuwayomiClient,
    work_id: &str,
) -> Result<bool> {
    // MangaDex anchor (oldest source_series) + cover file name, mirroring the crawl.
    let md = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT (SELECT ss.source_key FROM source_series ss \
                 WHERE ss.work_id = w.id AND ss.source_type = 'mangadex' \
                 ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1), \
                w.cover_file_name \
         FROM work w WHERE w.id = ?",
    )
    .bind(work_id)
    .fetch_optional(main)
    .await?;

    // Collect candidate source byte-fetches in priority order.
    let mut source_bytes: Option<Vec<u8>> = None;
    if let Some((Some(mangadex_id), Some(file_name))) = &md {
        if !mangadex_id.is_empty() {
            source_bytes = mangadex.cover_thumb_bytes(mangadex_id, file_name).await;
        }
    }
    if source_bytes.is_none() {
        // Fall back to the Suwayomi thumbnail anchor.
        // The cast is on `ss.source_key` (TEXT, no usable index for this join), NOT on
        // `s.id`. Casting the INDEXED side — `CAST(s.id AS TEXT)` — hides
        // `suwayomi_series`' INTEGER PRIMARY KEY from the planner, which then has no
        // choice but `SCAN s` across all 13,802 rows for every call. Same fix and same
        // reasoning as `catalog::work_effective_genres`. Measured on a copy of
        // production: 2.00 ms -> 0.01 ms, and 5.99 ms -> 0.01 ms once 0058 drops
        // `idx_suwayomi_series_library` and the fallback scan target widens.
        let thumb = sqlx::query_scalar::<_, Option<String>>(
            "SELECT s.thumbnail_url FROM source_series ss \
             JOIN suwayomi_series s ON s.id = CAST(ss.source_key AS INTEGER) \
             WHERE ss.work_id = ? AND ss.source_type = 'suwayomi' \
             ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1",
        )
        .bind(work_id)
        .fetch_optional(main)
        .await?
        .flatten();
        if let Some(t) = thumb.filter(|t| !t.is_empty()) {
            source_bytes = suwayomi.cover_bytes(Some(&t)).await;
        }
    }

    let Some(bytes) = source_bytes else {
        // Transient — couldn't fetch a source. Leave any existing issue as-is.
        return Ok(false);
    };

    // `bytes` is moved into the blocking task; `work_id` (a &str) stays borrowable
    // across the await for the DB writes below.
    match tokio::task::spawn_blocking(move || process_cover(&bytes)).await {
        Ok(Ok(webp)) => {
            put_work_cover(main, covers, work_id, &webp).await?;
            clear_cover_issue(main, work_id).await;
            Ok(true)
        }
        Ok(Err(e)) => {
            // A truncated download is transient — don't freeze it into work_cover_issue.
            if !is_transient_cover_error(&e) {
                record_cover_issue(main, work_id, classify_cover_error(&e), &e.to_string()).await;
            }
            Err(anyhow!("cover processing failed: {e}"))
        }
        // A spawn_blocking JoinError (panic) is not necessarily deterministic — surface
        // it as an error but do NOT record an issue, so a later attempt can retry.
        Err(e) => Err(anyhow!("cover processing task failed: {e}")),
    }
}

/// Fetch + store covers for EVERY canonical work that still lacks one (bounded by
/// the MangaDex rate limiter inside `cover_thumb_bytes`, so this is a polite,
/// slow background crawl). Idempotent and resumable: it only ever selects works
/// with `cover_cached_version IS NULL` (and no recorded `work_cover_issue`), so a
/// re-run drains whatever remains (e.g. works whose upstream fetch failed last
/// time). Best-effort per work — one failure never aborts the crawl.
pub async fn crawl_uncached_covers(
    main: &SqlitePool,
    covers: &SqlitePool,
    mangadex: &MangaDexClient,
    suwayomi: &crate::suwayomi::SuwayomiClient,
    limit: Option<i64>,
) {
    // The mangadex anchor lives on `source_series`, not `work`; pick the same
    // deterministic anchor the reader uses (oldest source_series) so the cover
    // matches the pages. `limit` bounds a single (auto-drainer) tick; None = drain
    // everything still uncached (the manual admin crawl).
    const BASE: &str = "SELECT w.id, \
                (SELECT ss.source_key FROM source_series ss \
                 WHERE ss.work_id = w.id AND ss.source_type = 'mangadex' \
                 ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1) AS mangadex_id, \
                w.cover_file_name \
         FROM work w \
         WHERE w.cover_cached_version IS NULL AND w.cover_file_name IS NOT NULL \
           AND w.id NOT IN (SELECT work_id FROM work_cover_issue)";
    let loaded = match limit {
        Some(n) => {
            sqlx::query_as::<_, (String, Option<String>, Option<String>)>(&format!(
                "{BASE} LIMIT ?"
            ))
            .bind(n)
            .fetch_all(main)
            .await
        }
        None => {
            sqlx::query_as::<_, (String, Option<String>, Option<String>)>(BASE)
                .fetch_all(main)
                .await
        }
    };
    let pending = match loaded {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "cover crawl: failed to load pending works");
            return;
        }
    };

    let jobs: Vec<PendingCover> = pending
        .into_iter()
        .filter_map(|(work_id, mid, fname)| match (mid, fname) {
            (Some(mangadex_id), Some(file_name)) if !mangadex_id.is_empty() => Some(PendingCover {
                work_id,
                mangadex_id,
                file_name,
            }),
            _ => None,
        })
        .collect();

    let total = jobs.len();
    tracing::info!(total, "cover crawl: starting");
    let mut saved = 0usize;
    let mut failed = 0usize;
    for job in jobs {
        match mangadex
            .cover_thumb_bytes(&job.mangadex_id, &job.file_name)
            .await
        {
            Some(bytes) => match process_cover(&bytes) {
                Ok(webp) => match put_work_cover(main, covers, &job.work_id, &webp).await {
                    Ok(()) => {
                        saved += 1;
                        clear_cover_issue(main, &job.work_id).await;
                    }
                    Err(e) => {
                        failed += 1;
                        tracing::warn!(work_id = %job.work_id, error = %e, "cover crawl: store failed");
                        record_cover_issue(main, &job.work_id, "store", &e.to_string()).await;
                    }
                },
                // Deterministic decode/encode/size failure — record it so the crawl
                // stops re-attempting this work every tick (it would fail identically).
                Err(e) => {
                    failed += 1;
                    tracing::warn!(work_id = %job.work_id, error = %e, "cover crawl: encode failed");
                    // …unless it's a truncated download, which a later tick may fetch whole.
                    if !is_transient_cover_error(&e) {
                        record_cover_issue(
                            main,
                            &job.work_id,
                            classify_cover_error(&e),
                            &e.to_string(),
                        )
                        .await;
                    }
                }
            },
            // Upstream fetch failure is TRANSIENT (rate limit / network) — do NOT
            // record an issue, so the next tick retries it.
            None => {
                failed += 1;
                tracing::warn!(work_id = %job.work_id, "cover crawl: upstream fetch failed");
            }
        }
    }
    tracing::info!(saved, failed, total, "cover crawl: complete");

    // Second pass: works with no MangaDex cover but anchored to a Suwayomi source
    // (the bulk of the scanlator/MangaDex-via-Suwayomi catalogue). Materialize their
    // cover from the Suwayomi thumbnail into the SAME work_cover_blob, so they too
    // serve from our own `/covers/{work_id}.webp` (immutable, CDN-cacheable) instead
    // of a live proxy to the Suwayomi engine on every request. Deterministic anchor:
    // the oldest suwayomi source_series (matches the reader's source pick).
    //
    // The thumbnail lookup is a CORRELATED scalar subquery — it runs once per candidate
    // work — so the join direction matters far more here than in `retry_one_cover`. The
    // cast goes on `ss.source_key` (TEXT, unindexed for this join), never on `s.id`:
    // `CAST(s.id AS TEXT)` hides the INTEGER PRIMARY KEY and forces `SCAN s` over all
    // 13,802 `suwayomi_series` rows PER WORK. Harmless today (the crawl is drained to a
    // single pending work) but quadratic on a fresh seed or after a
    // `cover_cached_version` bump invalidates the ~13.8k Suwayomi-anchored works: ~6 ms
    // x 13.8k = ~80 s of pure scan before a single byte is fetched. Post-fix the plan is
    // `SEARCH s USING INTEGER PRIMARY KEY (rowid=?)`.
    const SUW_BASE: &str = "SELECT w.id, \
                (SELECT s.thumbnail_url FROM source_series ss \
                 JOIN suwayomi_series s ON s.id = CAST(ss.source_key AS INTEGER) \
                 WHERE ss.work_id = w.id AND ss.source_type = 'suwayomi' \
                 ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1) AS thumbnail_url \
         FROM work w \
         WHERE w.cover_cached_version IS NULL \
           AND w.id NOT IN (SELECT work_id FROM work_cover_issue) \
           AND EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id \
                       AND ss.source_type = 'suwayomi')";
    let suw_loaded = match limit {
        Some(n) => {
            sqlx::query_as::<_, (String, Option<String>)>(&format!("{SUW_BASE} LIMIT ?"))
                .bind(n)
                .fetch_all(main)
                .await
        }
        None => {
            sqlx::query_as::<_, (String, Option<String>)>(SUW_BASE)
                .fetch_all(main)
                .await
        }
    };
    let suw_jobs: Vec<(String, String)> = match suw_loaded {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|(work_id, thumb)| thumb.filter(|t| !t.is_empty()).map(|t| (work_id, t)))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "cover crawl (suwayomi): failed to load pending works");
            return;
        }
    };
    let suw_total = suw_jobs.len();
    tracing::info!(total = suw_total, "cover crawl (suwayomi): starting");
    // OPT-9: run this pass with bounded concurrency instead of one-at-a-time, and move
    // the CPU-bound `process_cover` (decode + WebP re-encode) off the reactor via
    // spawn_blocking. A small fan-out overlaps the network waits without flooding the
    // Suwayomi engine or contending hard with the DB writer during a seed. Each item
    // yields (saved, failed) which we sum after the stream drains.
    use futures::StreamExt as _;
    // Bounded to the background share of the shared cover-fetch pool
    // (`suwayomi::COVER_FETCH_CONCURRENCY`), so this crawl can never occupy enough
    // permits to make an on-demand `<img>` request fail fast. Was a private 5, which is
    // what the headroom arithmetic is computed against — the semaphore alone does not
    // give on-demand requests headroom, only a cap on the background holders' share does
    // (they wait patiently, on-demand does not queue at all).
    const SUW_CRAWL_CONCURRENCY: usize = crate::suwayomi::WARM_COVER_CONCURRENCY;
    let (suw_saved, suw_failed) = futures::stream::iter(suw_jobs)
        .map(|(work_id, thumb)| async move {
            let bytes = match suwayomi.cover_bytes(Some(&thumb)).await {
                Some(b) => b,
                None => {
                    tracing::warn!(work_id = %work_id, "cover crawl (suwayomi): upstream fetch failed");
                    return (0usize, 1usize);
                }
            };
            match tokio::task::spawn_blocking(move || process_cover(&bytes)).await {
                Ok(Ok(webp)) => match put_work_cover(main, covers, &work_id, &webp).await {
                    Ok(()) => {
                        clear_cover_issue(main, &work_id).await;
                        (1, 0)
                    }
                    Err(e) => {
                        tracing::warn!(work_id = %work_id, error = %e, "cover crawl (suwayomi): store failed");
                        record_cover_issue(main, &work_id, "store", &e.to_string()).await;
                        (0, 1)
                    }
                },
                // Deterministic — record so we stop re-attempting an unprocessable source.
                Ok(Err(e)) => {
                    tracing::warn!(work_id = %work_id, error = %e, "cover crawl (suwayomi): encode failed");
                    if !is_transient_cover_error(&e) {
                        record_cover_issue(main, &work_id, classify_cover_error(&e), &e.to_string()).await;
                    }
                    (0, 1)
                }
                Err(e) => {
                    // A spawn_blocking JoinError (panic) is not necessarily deterministic;
                    // don't permanently skip on it — let a later tick retry.
                    tracing::warn!(work_id = %work_id, error = %e, "cover crawl (suwayomi): encode task panicked");
                    (0, 1)
                }
            }
        })
        .buffer_unordered(SUW_CRAWL_CONCURRENCY)
        .fold((0usize, 0usize), |(s, f), (ds, df)| async move { (s + ds, f + df) })
        .await;
    tracing::info!(
        saved = suw_saved,
        failed = suw_failed,
        total = suw_total,
        "cover crawl (suwayomi): complete"
    );
}

/// Cursor value meaning "start from the newest id". See [`warm_suwayomi_covers`].
pub const WARM_CURSOR_START: i64 = i64::MAX;

/// The manga-id set that already has a `suwayomi_cover_blob` row.
///
/// NOTE this is NOT the same as "rows in the table": `serve_suwayomi_cover` caches a
/// blob for ANY manga id the browser asks for, including Browse results that were never
/// enrolled into `suwayomi_series`. So the table is a superset of the candidate set and
/// `COUNT(*)` on it is not a coverage figure — see [`pending_suwayomi_cover_count`].
async fn cached_suwayomi_cover_ids(covers: &SqlitePool) -> Result<std::collections::HashSet<i64>> {
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT manga_id FROM suwayomi_cover_blob")
            .fetch_all(covers)
            .await?
            .into_iter()
            .collect(),
    )
}

/// Cached series that have a usable thumbnail, newest id first, restricted to
/// `id < before_id` (the warmer's rotating window; [`WARM_CURSOR_START`] = no restriction).
async fn thumbnailed_series_ids(main: &SqlitePool, before_id: i64) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM suwayomi_series \
         WHERE thumbnail_url IS NOT NULL AND thumbnail_url <> '' AND id < ? \
         ORDER BY id DESC",
    )
    .bind(before_id)
    .fetch_all(main)
    .await?)
}

/// Suwayomi manga ids below `before_id` that still have NO row in `suwayomi_cover_blob`,
/// newest-first, capped at `limit`.
///
/// `suwayomi_series` lives in the main (replicated) DB and `suwayomi_cover_blob` in the
/// separate covers DB, so SQLite cannot join them — the "LEFT JOIN" is done in memory:
/// pull the cached id set (a few i64 per row, ~110 KB at full coverage) and difference
/// it against the candidate list. Both queries are index-only-ish scans of small tables.
///
/// Ordering is by id DESC so a fresh deploy warms the most recently added series first
/// (those are what the "Latest updates" / discovery feeds surface). `before_id` is what
/// makes successive ticks make progress even when the newest batch *fails* — see
/// [`warm_suwayomi_covers`].
async fn uncached_suwayomi_cover_ids(
    main: &SqlitePool,
    covers: &SqlitePool,
    limit: i64,
    before_id: i64,
) -> Result<Vec<i64>> {
    let cached = cached_suwayomi_cover_ids(covers).await?;
    Ok(thumbnailed_series_ids(main, before_id)
        .await?
        .into_iter()
        .filter(|id| !cached.contains(id))
        .take(limit.max(0) as usize)
        .collect())
}

/// How many Suwayomi source covers are still un-warmed. Coverage gauge for the drainer's
/// "nothing to do" short-circuit and for logging.
///
/// This is the SET DIFFERENCE, not `series - COUNT(*)`. The subtraction is wrong because
/// `suwayomi_cover_blob` also holds blobs for manga that are not in `suwayomi_series` at
/// all (every Browse cover the request path materializes, see
/// [`cached_suwayomi_cover_ids`] — a live audit found 78 such rows). Those inflate the
/// cached count, understate the gap, and once they outnumber the real gap this reports 0
/// and the drainer concludes "fully warm" while genuine candidates remain — the warmer
/// would then never run again.
pub async fn pending_suwayomi_cover_count(main: &SqlitePool, covers: &SqlitePool) -> Result<i64> {
    let cached = cached_suwayomi_cover_ids(covers).await?;
    Ok(thumbnailed_series_ids(main, WARM_CURSOR_START)
        .await?
        .into_iter()
        .filter(|id| !cached.contains(id))
        .count() as i64)
}

/// Warm `suwayomi_cover_blob` — the cache the reader's home/discovery feeds ACTUALLY
/// read from.
///
/// Discovery hands the browser `/api/v1/manga/{manga_id}/thumbnail`, which
/// `main::serve_suwayomi_cover` serves out of `suwayomi_cover_blob` keyed by the
/// **Suwayomi manga id**. Nothing populated that table: `crawl_uncached_covers`' second
/// pass filters on `work.cover_cached_version IS NULL` and writes `work_cover_blob` keyed
/// by **work id** — a different table, a different key, a different consumer. The two
/// never met, so coverage sat at ~9% and every cold cover was a synchronous fetch through
/// Suwayomi to the source CDN (measured p50 5.6 s / p90 22.0 s / max 29.0 s at grid
/// concurrency, 502 past the timeout).
///
/// This pass closes exactly that gap: same batching/budget/shutdown shape as the crawl
/// above, fetching through [`crate::suwayomi::SuwayomiClient::fetch_cover_background`] so
/// it shares — and is capped inside — the bounded cover-fetch pool that on-demand
/// requests pre-empt.
///
/// # Progress guarantee
///
/// `cursor` is what makes the scan finish. Taking the newest `limit` uncached ids every
/// tick only progresses while those ids SUCCEED — a warmed id drops out of the candidate
/// set, an unwarmed one does not. A batch that persistently fails (a dead source
/// extension, and ids are assigned sequentially so one bulk enrolment IS a contiguous
/// newest block) would therefore be retried forever and the remaining ~12k candidates
/// below it would never be reached. So the window slides: each tick starts strictly below
/// the previous tick's lowest id and resets to [`WARM_CURSOR_START`] on reaching the
/// bottom. Failures now cost one tick each instead of every tick, and a full rotation
/// still retries them.
///
/// ARCHITECTURAL FOLLOW-UP (not done here — needs files this change set doesn't own):
/// collapse the two cover caches into one by having `series_cache.rs` / `graphql/mod.rs`
/// emit `/covers/{work_id}.webp` for Suwayomi-anchored works too. Then
/// `work_cover_blob` is the single cache, this warmer disappears, and the
/// `cover_cached_version` pointer covers every surface.
pub async fn warm_suwayomi_covers(
    covers: &SqlitePool,
    main: &SqlitePool,
    suwayomi: &crate::suwayomi::SuwayomiClient,
    limit: i64,
    cursor: &std::sync::atomic::AtomicI64,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> (usize, usize) {
    use std::sync::atomic::Ordering as AtomicOrdering;
    let start = cursor.load(AtomicOrdering::Relaxed);
    let load = |before| uncached_suwayomi_cover_ids(main, covers, limit, before);
    let ids = match load(start).await {
        // Bottom of the scan: wrap and take the newest window again, so a tick that
        // lands on the tail is not wasted.
        Ok(ids) if ids.is_empty() && start != WARM_CURSOR_START => {
            match load(WARM_CURSOR_START).await {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(error = %e, "suwayomi cover warmer: failed to load candidates");
                    return (0, 0);
                }
            }
        }
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "suwayomi cover warmer: failed to load candidates");
            return (0, 0);
        }
    };
    // Advance BEFORE doing the work: the point is that this tick's ids are not re-taken
    // next tick whatever happens to them (including a shutdown mid-batch). A short batch
    // means we reached the bottom, so restart from the newest.
    let next = match ids.last() {
        Some(&lowest) if (ids.len() as i64) >= limit => lowest,
        _ => WARM_CURSOR_START,
    };
    cursor.store(next, AtomicOrdering::Relaxed);
    if ids.is_empty() {
        return (0, 0);
    }
    let total = ids.len();
    tracing::info!(
        total,
        from_id = start,
        next_from_id = next,
        "suwayomi cover warmer: starting"
    );

    use futures::StreamExt as _;
    let (saved, failed) = futures::stream::iter(ids)
        .map(|manga_id| async move {
            // Cooperative shutdown: stop taking new work the moment the signal flips,
            // rather than draining a 500-item batch through a shutting-down process.
            if *shutdown.borrow() {
                return (0usize, 0usize);
            }
            let path = format!("/api/v1/manga/{manga_id}/thumbnail");
            let bytes = match suwayomi.fetch_cover_background(&path).await {
                Ok((b, _ct)) => b,
                Err(e) => {
                    tracing::debug!(manga_id, error = %e, "suwayomi cover warmer: fetch failed");
                    return (0, 1);
                }
            };
            match tokio::task::spawn_blocking(move || process_cover(&bytes)).await {
                Ok(Ok(webp)) => match put_suwayomi_cover(covers, manga_id, &webp).await {
                    Ok(()) => (1, 0),
                    Err(e) => {
                        tracing::warn!(manga_id, error = %e, "suwayomi cover warmer: store failed");
                        (0, 1)
                    }
                },
                Ok(Err(e)) => {
                    // No per-id issue table exists for Suwayomi covers, so an
                    // unprocessable source is simply retried on a later tick. That is
                    // acceptable: the candidate list is small and bounded, and the
                    // alternative (a second issue table) buys little.
                    tracing::debug!(manga_id, error = %e, "suwayomi cover warmer: encode failed");
                    (0, 1)
                }
                Err(e) => {
                    tracing::warn!(manga_id, error = %e, "suwayomi cover warmer: encode task panicked");
                    (0, 1)
                }
            }
        })
        // Never more than the background share of the shared cover-fetch pool, so an
        // on-demand `<img>` request always finds free permits and never fails fast
        // because of the warmer.
        .buffer_unordered(crate::suwayomi::WARM_COVER_CONCURRENCY)
        .fold((0usize, 0usize), |(s, f), (ds, df)| async move {
            (s + ds, f + df)
        })
        .await;
    tracing::info!(saved, failed, total, "suwayomi cover warmer: complete");
    (saved, failed)
}

/// Recurring background drainer that keeps the cover cache full with NO manual
/// trigger: every `interval_secs` it fetches + stores up to `batch` still-uncached
/// covers, so works added by catalogue sync / ingest get their cover cached
/// automatically within an interval or two. The first tick fires immediately on
/// startup. Batches are bounded so a tick stays short (polite + responsive to
/// shutdown), and it shares `inflight` with the manual admin crawl so the two
/// never hammer MangaDex at once. Mirrors `graphql::spawn_metadata_backfill`.
///
/// FLEET CONSTRAINT: like the catalogue sync / metadata backfill, this hits
/// MangaDex under the in-process rate limiter, so run it on exactly ONE replica.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    main: SqlitePool,
    covers: SqlitePool,
    mangadex: Arc<MangaDexClient>,
    suwayomi: crate::suwayomi::SuwayomiClient,
    inflight: Arc<AtomicBool>,
    interval_secs: u64,
    batch: i64,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Panic-supervised (crate::task::supervise): an unhandled panic in a crawl used to
    // end cover caching silently for the process lifetime. The factory re-clones the
    // loop's state on each restart. All captured handles are cheap to clone (pools are
    // Arc-backed, the client is Arc, inflight is Arc).
    tokio::spawn(crate::task::supervise(
        "cover-drainer",
        Duration::from_secs(30),
        shutdown.clone(),
        move || {
            run_loop(
                main.clone(),
                covers.clone(),
                mangadex.clone(),
                suwayomi.clone(),
                inflight.clone(),
                interval_secs,
                batch,
                shutdown.clone(),
            )
        },
    ));
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    main: SqlitePool,
    covers: SqlitePool,
    mangadex: Arc<MangaDexClient>,
    suwayomi: crate::suwayomi::SuwayomiClient,
    inflight: Arc<AtomicBool>,
    interval_secs: u64,
    batch: i64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Start 90s behind the scanner. This loop and the scan scheduler BOTH default
    // to a 300s period, so with both starting at t=0 they collided on every single
    // tick, forever — two of the six writers contending for SQLite's single writer
    // in lockstep. A phase offset that is not a divisor of the shared period keeps
    // them permanently out of step.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(90),
        Duration::from_secs(interval_secs),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // A second handle on the same channel: `shutdown` itself is mutably borrowed by the
    // `select!` below for the whole expression, so the tick body can't also read it.
    let warm_shutdown = shutdown.clone();
    // Rotating window for the Suwayomi warmer, so a persistently-failing newest batch
    // can't monopolise every tick — see `warm_suwayomi_covers`. Loop-local: a supervised
    // restart simply begins the rotation again from the newest id.
    let warm_cursor = std::sync::atomic::AtomicI64::new(WARM_CURSOR_START);
    tracing::info!(interval_secs, batch, "cover cache drainer started");
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Yield to a manual full crawl if one is in flight (don't stack
                // two MangaDex crawls); we'll pick up the remainder next tick.
                if inflight
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    continue;
                }
                match pending_cover_count(&main).await {
                    Ok(0) => { /* nothing to do this tick */ }
                    Ok(_) => crawl_uncached_covers(&main, &covers, &mangadex, &suwayomi, Some(batch)).await,
                    Err(e) => tracing::warn!(error = %e, "cover drainer: count failed"),
                }
                // Second cache, second pass. `work_cover_blob` (above) is what
                // `/covers/{work_id}.webp` serves; `suwayomi_cover_blob` (here) is what
                // the reader's home/discovery feeds actually request via
                // `/api/v1/manga/{id}/thumbnail`. Nothing warmed the latter, so 91% of
                // home covers were a live source-CDN fetch on the request path. Reuses
                // this loop's existing interval/batch budget rather than adding a knob;
                // its fan-out is capped at the background share of the shared cover
                // semaphore so it cannot saturate the pool it exists to relieve.
                if !*warm_shutdown.borrow() {
                    match pending_suwayomi_cover_count(&main, &covers).await {
                        Ok(0) => { /* fully warm */ }
                        Ok(pending) => {
                            tracing::debug!(pending, "suwayomi cover warmer: pending");
                            warm_suwayomi_covers(
                                &covers,
                                &main,
                                &suwayomi,
                                batch,
                                &warm_cursor,
                                &warm_shutdown,
                            )
                            .await;
                        }
                        Err(e) => tracing::warn!(error = %e, "suwayomi cover warmer: count failed"),
                    }
                }
                inflight.store(false, Ordering::SeqCst);
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("cover cache drainer stopping");
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
    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

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
            .unwrap();
        bytes
    }

    /// P0.5 opt-level bench (not part of the 191-test suite — `#[ignore]`d).
    /// Times the two dominant CPU hot paths on a realistic high-entropy cover
    /// (forces the full 5-edge resize+encode search — the worst case). Run under
    /// the release profile so it reflects the shipped binary; toggle
    /// `[profile.release] opt-level` between runs to compare:
    ///   cargo test --release --bin komika-server opt_level_hot_path_bench \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore]
    fn opt_level_hot_path_bench() {
        use std::time::Instant;

        fn stats(label: &str, mut ns: Vec<u128>) {
            ns.sort_unstable();
            let n = ns.len();
            let median = ns[n / 2] as f64 / 1e6;
            let min = ns[0] as f64 / 1e6;
            let mean = ns.iter().sum::<u128>() as f64 / n as f64 / 1e6;
            eprintln!(
                "  {label:<28} n={n:>4}  min={min:8.3}ms  median={median:8.3}ms  mean={mean:8.3}ms"
            );
        }

        // Realistic portrait cover source (~700x1000), high-entropy so no early
        // candidate edge fits the byte budget → full resize+encode loop runs.
        let cover_src = noisy_png(700, 1000);
        let hash_src = noisy_png(512, 728);

        // Warm up (allocator, code paths, page cache) before timing.
        for _ in 0..3 {
            let _ = process_cover(&cover_src).expect("warmup process_cover");
            let _ = crate::phash::dhash(&hash_src);
        }

        const N_COVER: usize = 20;
        const N_HASH: usize = 100;

        let mut cover_ns = Vec::with_capacity(N_COVER);
        for _ in 0..N_COVER {
            let t = Instant::now();
            let out = process_cover(&cover_src).expect("process_cover");
            cover_ns.push(t.elapsed().as_nanos());
            assert!(out.len() <= MAX_COVER_BYTES);
        }

        let mut hash_ns = Vec::with_capacity(N_HASH);
        for _ in 0..N_HASH {
            let t = Instant::now();
            let out = crate::phash::dhash(&hash_src);
            hash_ns.push(t.elapsed().as_nanos());
            assert!(out.is_some());
        }

        eprintln!("\n=== opt-level hot-path bench (release profile) ===");
        stats("process_cover(700x1000)", cover_ns);
        stats("dhash(512x728)", hash_ns);
        eprintln!("=================================================\n");
    }

    #[test]
    fn preserves_aspect_ratio_within_budget() {
        // A portrait, noisy cover: stays portrait (not squared) and fits budget.
        let src = noisy_png(700, 1000);
        let webp = process_cover(&src).expect("processes");
        assert!(
            webp.len() <= MAX_COVER_BYTES,
            "cover {} bytes exceeds budget {}",
            webp.len(),
            MAX_COVER_BYTES
        );
        let (w, h) = image::load_from_memory(&webp).unwrap().dimensions();
        assert!(h > w, "portrait aspect ratio must be preserved");
        assert!(w.max(h) <= CANDIDATE_EDGES[0], "longest edge within cap");
    }

    #[test]
    fn never_upscales_small_source() {
        let src = noisy_png(120, 180);
        let webp = process_cover(&src).unwrap();
        let (w, h) = image::load_from_memory(&webp).unwrap().dimensions();
        assert_eq!((w, h), (120, 180), "small source kept at native size");
    }

    #[test]
    fn rejects_empty_and_non_image() {
        assert!(process_cover(&[]).is_err());
        assert!(process_cover(b"not an image").is_err());
    }

    #[test]
    fn lossy_cover_is_sharp_and_well_under_budget() {
        // A realistic portrait cover already within the 512 longest-edge cap (360x512),
        // smooth gradient — what real art resembles, vs. adversarial pure noise. Lossy
        // must keep its FULL native dimensions (not shrink) AND land under the 150 KB
        // budget, decoding back to a valid image. (Pure noise legitimately shrinks
        // instead — covered by preserves_aspect_ratio_within_budget.)
        let mut img = RgbImage::new(360, 512);
        for (x, y, px) in img.enumerate_pixels_mut() {
            // Smooth full-range gradient (no wrap discontinuity) — compresses well
            // with lossy, so it stays at native size under budget.
            let r = (x * 255 / 359) as u8;
            let g = (y * 255 / 511) as u8;
            let b = ((x + y) * 255 / (359 + 511)) as u8;
            *px = image::Rgb([r, g, b]);
        }
        let mut src = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut src), image::ImageFormat::Png)
            .unwrap();
        let webp = process_cover(&src).expect("processes");
        assert!(
            webp.len() <= MAX_COVER_BYTES,
            "lossy cover {} bytes over budget {}",
            webp.len(),
            MAX_COVER_BYTES
        );
        let (w, h) = image::load_from_memory(&webp).unwrap().dimensions();
        assert_eq!(
            (w, h),
            (360, 512),
            "kept native dimensions via lossy, not shrunk"
        );
    }

    #[test]
    fn gif_source_decodes_and_processes() {
        // A GIF source (previously "unsupported") must now decode + process to WebP.
        let img = RgbImage::from_pixel(300, 420, image::Rgb([180, 40, 90]));
        let mut gif = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut gif), image::ImageFormat::Gif)
            .expect("encode gif");
        let webp = process_cover(&gif).expect("gif cover processes");
        image::load_from_memory(&webp).expect("valid webp from gif source");
    }

    #[test]
    fn classify_cover_error_maps_reasons() {
        assert_eq!(
            classify_cover_error(&anyhow!("cover source too large (max 24 MB)")),
            "too_large"
        );
        assert_eq!(
            classify_cover_error(&anyhow!("image too large or unsupported")),
            "unsupported"
        );
        assert_eq!(
            classify_cover_error(&anyhow!("empty cover source")),
            "empty"
        );
        assert_eq!(classify_cover_error(&anyhow!("libwebp exploded")), "encode");
    }

    #[tokio::test]
    async fn cover_issue_record_increments_then_clears() {
        use sqlx::sqlite::SqlitePoolOptions;
        let main = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&main).await.unwrap();
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('w_iss', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&main)
        .await
        .unwrap();

        record_cover_issue(&main, "w_iss", "unsupported", "boom").await;
        record_cover_issue(&main, "w_iss", "too_large", "again").await;
        let (reason, attempts): (String, i64) =
            sqlx::query_as("SELECT reason, attempts FROM work_cover_issue WHERE work_id = ?")
                .bind("w_iss")
                .fetch_one(&main)
                .await
                .unwrap();
        assert_eq!(attempts, 2, "second record increments attempts");
        assert_eq!(reason, "too_large", "reason refreshed to the latest");

        clear_cover_issue(&main, "w_iss").await;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_cover_issue WHERE work_id = ?")
            .bind("w_iss")
            .fetch_one(&main)
            .await
            .unwrap();
        assert_eq!(n, 0, "clear removes the issue row");
    }

    #[test]
    fn cover_path_is_versioned() {
        assert_eq!(cover_path("w_abc", 99), "/covers/w_abc.webp?v=99");
    }

    #[tokio::test]
    async fn put_work_cover_stores_blob_and_flips_version() {
        use sqlx::sqlite::SqlitePoolOptions;
        // Main (replicated) DB: `work` + the `cover_cached_version` pointer. After
        // migration 0040 it has NO `work_cover_blob` table.
        let main = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&main).await.unwrap();
        // Covers (un-replicated) DB: just the blob table, as `db::init_covers` builds.
        let covers = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE work_cover_blob (work_id TEXT PRIMARY KEY, webp BLOB NOT NULL, \
             version INTEGER NOT NULL, updated_at TEXT NOT NULL)",
        )
        .execute(&covers)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('w_test', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&main)
        .await
        .unwrap();

        // No cache yet → resolver falls back to the MangaDex thumbnail URL.
        let ver0: Option<i64> =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id = 'w_test'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert!(ver0.is_none());

        let webp = process_cover(&noisy_png(400, 600)).unwrap();
        put_work_cover(&main, &covers, "w_test", &webp)
            .await
            .unwrap();

        // Blob is stored in the covers DB and the work's version pointer (main DB)
        // matches it (invariant: no version pointer without the bytes behind it).
        let (blob, blob_ver): (Vec<u8>, i64) =
            sqlx::query_as("SELECT webp, version FROM work_cover_blob WHERE work_id = 'w_test'")
                .fetch_one(&covers)
                .await
                .unwrap();
        let work_ver: Option<i64> =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id = 'w_test'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert_eq!(blob, webp, "stored bytes round-trip");
        assert_eq!(
            Some(blob_ver),
            work_ver,
            "version pointer matches stored blob"
        );
        assert_eq!(
            work_cover_url("w_test", work_ver, Some("md"), Some("f.jpg")),
            cover_path("w_test", blob_ver),
            "resolver now prefers the cached VPS path"
        );

        // pending_cover_count excludes the now-cached work.
        assert_eq!(pending_cover_count(&main).await.unwrap(), 0);
    }

    /// The drainer's "nothing to do" short-circuit is only meaningful if the count agrees
    /// with what the crawl SELECTs can actually pick up. A work with a recorded
    /// `work_cover_issue` is excluded by BOTH crawl passes, so counting it made the
    /// short-circuit unreachable — measured live as 27 "pending" against 1 selectable,
    /// i.e. a full crawl pass every 300 s to process a single row.
    #[tokio::test]
    async fn pending_cover_count_excludes_works_with_a_recorded_issue() {
        use sqlx::sqlite::SqlitePoolOptions;
        let main = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&main).await.unwrap();
        for id in ["w_ok", "w_broken"] {
            sqlx::query(
                "INSERT INTO work (id, cover_file_name, created_at, updated_at) \
                 VALUES (?, 'c.jpg', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(id)
            .execute(&main)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
                 VALUES (?, ?, 'mangadex', ?, '2026-01-01T00:00:00Z')",
            )
            .bind(format!("ss_{id}"))
            .bind(id)
            .bind(format!("md-{id}"))
            .execute(&main)
            .await
            .unwrap();
        }
        assert_eq!(pending_cover_count(&main).await.unwrap(), 2);

        record_cover_issue(&main, "w_broken", "unsupported", "nope").await;
        assert_eq!(
            pending_cover_count(&main).await.unwrap(),
            1,
            "a work the crawl will never select must not be counted as pending"
        );
    }

    #[test]
    fn work_cover_url_prefers_cached_then_own_lazy_route() {
        // Cached → versioned VPS blob path.
        assert_eq!(
            work_cover_url("w_1", Some(7), Some("md-uuid"), Some("f.jpg")),
            "/covers/w_1.webp?v=7"
        );
        // Uncached but cover-able → our own lazy /covers/ route (serve_cover fetches +
        // caches the MangaDex cover on first hit), NOT the raw CDN URL.
        assert_eq!(
            work_cover_url("w_1", None, Some("md-uuid"), Some("f.jpg")),
            "/covers/w_1.webp"
        );
        // No anchor → empty.
        assert_eq!(work_cover_url("w_1", None, None, None), "");
        assert_eq!(work_cover_url("w_1", None, Some("md-uuid"), None), "");
    }

    // ---------------------------------------------------------------------------
    // Truncation gate (P3)
    // ---------------------------------------------------------------------------

    fn real_jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([
                ((x * 73 + y * 151) % 256) as u8,
                ((x * 199 + y * 37) % 256) as u8,
                ((x ^ (y.wrapping_mul(101))) % 256) as u8,
            ]);
        }
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut out),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        out
    }

    /// The corruption regression: a truncated JPEG must be REJECTED by `process_cover`,
    /// not silently re-encoded into a cover with a flat decoder-fill tail and frozen
    /// behind `cover_cached_version` + a one-year immutable edge TTL.
    #[test]
    fn process_cover_rejects_truncated_jpeg() {
        let whole = real_jpeg(400, 600);
        process_cover(&whole).expect("whole JPEG still processes");

        let mut decoder_would_have_accepted = 0;
        for pct in [25, 40, 55, 70, 85, 95] {
            let part = &whole[..whole.len() * pct / 100];
            if image::load_from_memory(part).is_ok() {
                decoder_would_have_accepted += 1;
            }
            let e = process_cover(part).expect_err("truncated source must not be stored");
            assert_eq!(
                classify_cover_error(&e),
                "truncated",
                "reason must be attributable in the admin Bugs panel: {e}"
            );
            assert!(
                is_transient_cover_error(&e),
                "a truncated DOWNLOAD is transient — recording it in work_cover_issue \
                 would permanently strand a work that has a perfectly good source"
            );
        }
        assert!(
            decoder_would_have_accepted > 0,
            "precondition: `image`/zune-jpeg must accept at least one of these \
             truncations, otherwise the gate is redundant"
        );
    }

    #[test]
    fn deterministic_cover_errors_stay_non_transient() {
        // Everything except `truncated` must still be recorded, or the crawl will
        // re-attempt an unprocessable source on every single tick forever.
        for msg in [
            "cover source too large (max 24 MB)",
            "image too large or unsupported",
            "empty cover source",
            "libwebp exploded",
        ] {
            assert!(
                !is_transient_cover_error(&anyhow!("{msg}")),
                "{msg} must remain recordable"
            );
        }
        assert!(is_transient_cover_error(&anyhow!(
            "truncated JPEG (no EOI marker)"
        )));
    }

    // ---------------------------------------------------------------------------
    // suwayomi_cover_blob warmer (P1)
    // ---------------------------------------------------------------------------

    /// Build the two-DB pair the warmer works across: main (migrated, holds
    /// `suwayomi_series`) and covers (holds `suwayomi_cover_blob`), exactly as
    /// `db::init_covers` shapes it.
    async fn warm_test_pools() -> (SqlitePool, SqlitePool) {
        use sqlx::sqlite::SqlitePoolOptions;
        let main = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&main).await.unwrap();
        let covers = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE suwayomi_cover_blob (manga_id INTEGER PRIMARY KEY, \
             webp BLOB NOT NULL, updated_at TEXT NOT NULL)",
        )
        .execute(&covers)
        .await
        .unwrap();
        (main, covers)
    }

    async fn insert_series(main: &SqlitePool, id: i64, thumb: Option<&str>) {
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, status, source_id, in_library, \
             thumbnail_url, updated_at) VALUES (?, ?, 'ONGOING', '1', 1, ?, '2026-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(format!("s{id}"))
        .bind(thumb)
        .execute(main)
        .await
        .unwrap();
    }

    /// The candidate query is the "LEFT JOIN across two SQLite files" that the missing
    /// warmer never had: only series with a usable thumbnail and NO blob yet, newest
    /// first, capped.
    #[tokio::test]
    async fn uncached_ids_excludes_cached_and_thumbless_series() {
        let (main, covers) = warm_test_pools().await;
        insert_series(&main, 10, Some("/api/v1/manga/10/thumbnail")).await;
        insert_series(&main, 11, Some("/api/v1/manga/11/thumbnail")).await;
        insert_series(&main, 12, None).await; // no thumbnail → not a candidate
        insert_series(&main, 13, Some("")).await; // empty thumbnail → not a candidate
        insert_series(&main, 14, Some("/api/v1/manga/14/thumbnail")).await;
        put_suwayomi_cover(&covers, 11, b"already-warm")
            .await
            .unwrap();

        let ids = uncached_suwayomi_cover_ids(&main, &covers, 100, WARM_CURSOR_START)
            .await
            .unwrap();
        assert_eq!(
            ids,
            vec![14, 10],
            "newest-first, cached + thumbless excluded"
        );
        assert_eq!(
            pending_suwayomi_cover_count(&main, &covers).await.unwrap(),
            2
        );

        // The limit bounds a single tick.
        let one = uncached_suwayomi_cover_ids(&main, &covers, 1, WARM_CURSOR_START)
            .await
            .unwrap();
        assert_eq!(one, vec![14]);
    }

    /// THE regression test for the outage: the warmer must populate
    /// `suwayomi_cover_blob` — the table the reader's home/discovery feeds actually read
    /// — for an uncached in-library series. Before this pass nothing wrote that table at
    /// all (the drainer's Suwayomi pass writes `work_cover_blob` keyed by work id), so
    /// coverage sat at 1,253/13,847 and every cold home cover was a live source-CDN
    /// fetch on the request path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn warmer_populates_the_cache_the_reader_reads() {
        let (main, covers) = warm_test_pools().await;
        for id in [21, 22, 23] {
            insert_series(&main, id, Some(&format!("/api/v1/manga/{id}/thumbnail"))).await;
        }
        // 22 is already warm; the warmer must not refetch it.
        put_suwayomi_cover(&covers, 22, b"already-warm")
            .await
            .unwrap();

        let origin = crate::suwayomi::testsrv::spawn(
            crate::suwayomi::testsrv::jpeg(300, 450),
            std::time::Duration::ZERO,
        )
        .await;
        let client =
            crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, Some("1".into()));
        let (_tx, rx) = tokio::sync::watch::channel(false);

        let cursor = AtomicI64::new(WARM_CURSOR_START);
        let (saved, failed) =
            warm_suwayomi_covers(&covers, &main, &client, 500, &cursor, &rx).await;
        assert_eq!((saved, failed), (2, 0), "both uncached series warmed");

        // Exactly what `serve_suwayomi_cover` reads on the request path.
        for id in [21, 23] {
            let blob = get_suwayomi_cover(&covers, id)
                .await
                .unwrap_or_else(|| panic!("manga {id} must now be cached"));
            assert!(
                blob.len() <= MAX_COVER_BYTES,
                "warmed cover {id} is {} bytes, over the {MAX_COVER_BYTES} budget",
                blob.len()
            );
            image::load_from_memory(&blob).expect("warmed blob is a decodable image");
        }
        // The already-warm row is untouched (no wasted upstream fetch, no re-encode).
        assert_eq!(
            get_suwayomi_cover(&covers, 22).await.as_deref(),
            Some(&b"already-warm"[..])
        );
        assert_eq!(
            origin.hits.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one fetch per uncached series, none for the cached one"
        );
        // Coverage is now complete, so a second tick is a no-op.
        assert_eq!(
            pending_suwayomi_cover_count(&main, &covers).await.unwrap(),
            0
        );
        assert_eq!(
            warm_suwayomi_covers(&covers, &main, &client, 500, &cursor, &rx).await,
            (0, 0),
            "a fully warm cache does no work"
        );
    }

    /// The warmer must never occupy enough of the shared cover-fetch pool to make an
    /// on-demand request fail fast — that would trade the outage for a different one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn warmer_leaves_headroom_for_on_demand_requests() {
        let (main, covers) = warm_test_pools().await;
        for id in 1..=40 {
            insert_series(&main, id, Some(&format!("/api/v1/manga/{id}/thumbnail"))).await;
        }
        // Slow origin so the warmer is provably mid-flight while we probe the pool.
        let origin = crate::suwayomi::testsrv::spawn(
            crate::suwayomi::testsrv::jpeg(64, 96),
            std::time::Duration::from_millis(120),
        )
        .await;
        let client =
            crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, Some("1".into()));
        let (_tx, rx) = tokio::sync::watch::channel(false);

        let warm = {
            let (covers, main, client, rx) =
                (covers.clone(), main.clone(), client.clone(), rx.clone());
            tokio::spawn(async move {
                let cursor = AtomicI64::new(WARM_CURSOR_START);
                warm_suwayomi_covers(&covers, &main, &client, 40, &cursor, &rx).await
            })
        };

        // Sample the pool while the warmer runs: it must never hold more than its share.
        let mut min_free = usize::MAX;
        for _ in 0..40 {
            min_free = min_free.min(client.cover_permits_available());
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            if warm.is_finished() {
                break;
            }
        }
        let (saved, _) = warm.await.unwrap();
        assert_eq!(saved, 40, "warmer completed its batch");
        assert!(
            min_free
                >= crate::suwayomi::COVER_FETCH_CONCURRENCY
                    - crate::suwayomi::WARM_COVER_CONCURRENCY,
            "warmer left only {min_free} permits free; on-demand needs at least {}",
            crate::suwayomi::COVER_FETCH_CONCURRENCY - crate::suwayomi::WARM_COVER_CONCURRENCY
        );
        assert!(
            origin
                .peak_concurrent
                .load(std::sync::atomic::Ordering::SeqCst)
                <= crate::suwayomi::WARM_COVER_CONCURRENCY,
            "warmer exceeded its fan-out bound at the origin"
        );
    }

    /// Shutdown must stop the warmer taking new work rather than draining a full batch
    /// through a shutting-down process.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn warmer_honours_shutdown() {
        let (main, covers) = warm_test_pools().await;
        for id in 1..=30 {
            insert_series(&main, id, Some(&format!("/api/v1/manga/{id}/thumbnail"))).await;
        }
        let origin = crate::suwayomi::testsrv::spawn(
            crate::suwayomi::testsrv::jpeg(64, 96),
            std::time::Duration::ZERO,
        )
        .await;
        let client =
            crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, Some("1".into()));
        let (tx, rx) = tokio::sync::watch::channel(true); // already shutting down
        let _ = tx;
        let cursor = AtomicI64::new(WARM_CURSOR_START);
        let (saved, failed) = warm_suwayomi_covers(&covers, &main, &client, 30, &cursor, &rx).await;
        assert_eq!((saved, failed), (0, 0), "no work started after shutdown");
        assert_eq!(origin.hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// LIVELOCK regression. Taking the newest `limit` uncached ids every tick only makes
    /// progress while they SUCCEED — a failing id stays in the candidate set and is
    /// re-selected forever, so a dead source occupying the newest ids (they are assigned
    /// sequentially, so one bulk enrolment is a contiguous block) would starve every
    /// other candidate indefinitely. The rotating cursor must slide the window down
    /// regardless of outcome, and wrap at the bottom.
    #[tokio::test]
    async fn warmer_rotates_past_a_persistently_failing_batch() {
        let (main, covers) = warm_test_pools().await;
        for id in 1..=6 {
            insert_series(&main, id, Some(&format!("/api/v1/manga/{id}/thumbnail"))).await;
        }
        // Nothing listening: every fetch fails immediately (connection refused), so no id
        // ever leaves the candidate set.
        let client = crate::suwayomi::SuwayomiClient::new(
            "http://127.0.0.1:1".into(),
            None,
            Some("1".into()),
        );
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let cursor = AtomicI64::new(WARM_CURSOR_START);

        // Three ticks of two must cover all six ids, not retry {6,5} three times.
        for expect_cursor in [5, 3, 1] {
            let (saved, failed) =
                warm_suwayomi_covers(&covers, &main, &client, 2, &cursor, &rx).await;
            assert_eq!(
                (saved, failed),
                (0, 2),
                "every fetch fails against a dead origin"
            );
            assert_eq!(
                cursor.load(AtomicOrdering::Relaxed),
                expect_cursor,
                "the window must slide down even though nothing succeeded"
            );
        }
        // Bottom reached: the next tick wraps to the newest window instead of idling.
        let (_, failed) = warm_suwayomi_covers(&covers, &main, &client, 2, &cursor, &rx).await;
        assert_eq!(failed, 2, "wrapped and retried the newest ids");
        assert_eq!(cursor.load(AtomicOrdering::Relaxed), 5);
    }

    /// The coverage gauge must be a SET DIFFERENCE, not `series - COUNT(*)`.
    /// `suwayomi_cover_blob` also holds blobs the request path materialized for Browse
    /// manga that were never enrolled in `suwayomi_series`; counting those as coverage
    /// drives `pending` to 0 while real candidates remain, and the drainer then skips the
    /// warmer forever.
    #[tokio::test]
    async fn pending_count_ignores_blobs_for_unenrolled_manga() {
        let (main, covers) = warm_test_pools().await;
        for id in [31, 32] {
            insert_series(&main, id, Some(&format!("/api/v1/manga/{id}/thumbnail"))).await;
        }
        // Three Browse covers cached on the request path; none is an enrolled series.
        for id in [900, 901, 902] {
            put_suwayomi_cover(&covers, id, b"browse").await.unwrap();
        }
        assert_eq!(
            pending_suwayomi_cover_count(&main, &covers).await.unwrap(),
            2,
            "both enrolled series are still un-warmed"
        );
        assert_eq!(
            uncached_suwayomi_cover_ids(&main, &covers, 10, WARM_CURSOR_START)
                .await
                .unwrap(),
            vec![32, 31]
        );
    }
}
