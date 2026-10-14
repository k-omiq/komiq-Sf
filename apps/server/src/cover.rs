//! DB-backed cover cache for canonical works.
//!
//! The web reader normally routes every cover through the Cloudflare image Worker
//! (CORS). To take covers off the Worker, we fetch each work's MangaDex cover
//! once, re-encode it to a bounded lossless WebP, and store the bytes in
//! `work_cover_blob` (in the separate, unreplicated covers file). The reader
//! then loads covers from our own origin at `/covers/{work_id}.webp` and the
//! Worker only ever sees chapter PAGES.
//!
//! Presence is signalled by `work.cover_cached_version` (NULL = not cached), a
//! column that rides on the `work` row already loaded on hot paths, so cover-URL
//! resolution costs no extra query.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::database::SqliteDatabase;
use crate::db::Database;
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use image::{imageops, GenericImageView};

use crate::avatar::{decode_limited, encode_webp_lossy};
use crate::mangadex::MangaDexClient;

/// Reject raw source images larger than this before decoding (decode-bomb guard).
/// Raised from 8 MB: some source thumbnails (a handful of Suwayomi scanlator
/// covers) ship 8–20 MB originals that decode fine and downscale to a tiny WebP —
/// the old cap rejected them outright. The real bomb guard is `decode_limited`'s
/// pixel-dimension + allocation ceiling, which still applies after this gate.
pub const MAX_SOURCE_BYTES: usize = 24 * 1024 * 1024;

/// Capacity for uncached master covers, independent of GraphQL and variant work.
/// Acquire before the download: at most eight source buffers can exist while
/// waiting for one of two memory-heavy decoders. Cached covers bypass this lane.
static MASTER_COVER_LIMITS: std::sync::LazyLock<MasterCoverLimits> =
    std::sync::LazyLock::new(|| MasterCoverLimits::new(8, 2));

struct MasterCoverLimits {
    sources: Arc<tokio::sync::Semaphore>,
    decoders: Arc<tokio::sync::Semaphore>,
}

impl MasterCoverLimits {
    fn new(sources: usize, decoders: usize) -> Self {
        Self {
            sources: Arc::new(tokio::sync::Semaphore::new(sources)),
            decoders: Arc::new(tokio::sync::Semaphore::new(decoders)),
        }
    }

    async fn acquire(&self) -> MasterCoverWork {
        MasterCoverWork {
            _source: self
                .sources
                .clone()
                .acquire_owned()
                .await
                .expect("master cover source semaphore is never closed"),
            decoders: self.decoders.clone(),
        }
    }
}

/// Owns capacity from before a source fetch until its decoder really exits.
/// Dropping the HTTP future cannot release either permit from a blocking worker.
pub struct MasterCoverWork {
    _source: tokio::sync::OwnedSemaphorePermit,
    decoders: Arc<tokio::sync::Semaphore>,
}

pub async fn master_cover_work() -> MasterCoverWork {
    MASTER_COVER_LIMITS.acquire().await
}

impl MasterCoverWork {
    pub async fn process<T: Send + 'static>(
        self,
        process: impl FnOnce() -> T + Send + 'static,
    ) -> std::result::Result<T, tokio::task::JoinError> {
        let decoder = self
            .decoders
            .clone()
            .acquire_owned()
            .await
            .expect("master cover decoder semaphore is never closed");
        tokio::task::spawn_blocking(move || {
            let (_work, _decoder) = (self, decoder);
            process()
        })
        .await
    }
}

/// Per-cover size budget for the stored WebP — the "100–150 KB" target. Lossy
/// encoding (below) keeps detailed art sharp at 512px while landing under this,
/// so the covers blob store stays cheap. Down from the old
/// 200 KB lossless budget.
pub const MAX_COVER_BYTES: usize = 150 * 1024;

/// Candidate longest-edge lengths (px), largest first — the portrait-preserving
/// dimension bound. With LOSSY encoding the quality ladder does most of the size
/// control, so the top edge (512) almost always wins; the smaller edges are a
/// fallback for pathological sources that won't fit even at low quality.
///
/// Tried largest first; `process_cover` keeps the first encoding
/// that fits [`MAX_COVER_BYTES`]. The 320/256 rungs exist for the noisy minority whose
/// 384px encode still misses the budget at the lowest quality — before they were added
/// those covers fell through to the "smallest produced" fallback and shipped OVER the
/// cap (one live cover measured 203 KB at 316x448). A cover that fits at 512 never
/// reaches these rungs, so nothing that was already in budget gets smaller.
const CANDIDATE_EDGES: [u32; 5] = [512, 448, 384, 320, 256];

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

/// Whether a stored WebP is in the LOSSY (VP8) encoding this pipeline produces today.
///
/// Covers written before the lossy switch are `VP8L` (lossless, from the pure-Rust
/// `image-webp` encoder) and run 150–200 KB for the same 512 px art that lossy encodes in
/// 20–40 KB — the single biggest chunk of the "Improve image delivery" bytes Lighthouse
/// reports. `cover_recompress` uses this as its cheap, header-only "does this row still
/// need work?" test: no decode, and a row it rewrites can never match again, so the pass
/// is self-terminating.
///
/// A `VP8X` (extended container: alpha, ICC, EXIF) master reads as NOT lossy. That is
/// deliberate — the payload inside may be either, and re-encoding one is harmless (the
/// recompress pass keeps the result only when it is meaningfully smaller).
pub fn webp_is_lossy(bytes: &[u8]) -> bool {
    bytes.len() >= 16
        && &bytes[0..4] == b"RIFF"
        && &bytes[8..12] == b"WEBP"
        && &bytes[12..16] == b"VP8 "
}

/// Widths (px) the `?w=` cover variant is allowed to take. An ALLOWLIST, not a range:
/// each accepted value mints a stored derivative, so an open parameter would let one
/// crafted page fill the covers DB with thousands of near-identical encodings.
///
/// Two entries. Cards render at 150–200 CSS px ([`crate::graphql`] feeds them to
/// `MangaCard`, whose default width is 150; the widest auto-fill grid column reaches
/// ~200), so a 1x viewer needs ~150–200 px and gets 192, and a 2x viewer needs
/// ~300–400 px and gets 320. Only a 3x viewer still gets the master, which at 240–364
/// px wide is the largest cover we hold — there is nothing sharper to hand it.
///
/// The 2x rung is not cosmetic: before it existed, EVERY device above 1x downloaded the
/// master, which is what Lighthouse flagged (a 360x510 master for a card that can show
/// 263x373 device px at DPR 1.75). Adding a width here is all a new call site needs;
/// nothing is generated until a request actually asks for it.
pub const VARIANT_WIDTHS: [u32; 2] = [CARD_VARIANT_WIDTH, CARD_VARIANT_WIDTH_2X];

/// The variant width for a card-sized cover at 1x — what the reader puts in the `1x`
/// slot of its `srcset` (mirrored there as `CARD_COVER_WIDTH`). Defined ahead of
/// [`VARIANT_WIDTHS`] and used to build it, so "the width the reader asks for" and "the
/// widths we agree to serve" cannot drift apart on this side of the wire.
pub const CARD_VARIANT_WIDTH: u32 = 192;

/// The variant width for a card-sized cover at 2x (`CARD_COVER_WIDTH_2X` on the reader
/// side). A 150 CSS px card needs 300 device px at DPR 2 and 263 at DPR 1.75; 320 covers
/// both and stays well under every master, so no viewer at 2x pulls a master any more.
pub const CARD_VARIANT_WIDTH_2X: u32 = 320;

/// The 2x rung must be WIDER than the 1x rung: `downscale_variant` reads the ordering to
/// pick an encode quality, and the reader hands the two out as `1x`/`2x` candidates, so a
/// swap would serve the smaller image to the denser screen. Checked at compile time
/// because there is no runtime path that could ever report it.
const _: () = assert!(CARD_VARIANT_WIDTH_2X > CARD_VARIANT_WIDTH);

/// Encode quality for a HiDPI derivative — every rung above the 1x one.
///
/// Lower than the master's ladder top (82) on purpose, and the reasoning is about where
/// the pixels land, not about "small images matter less". A 1x derivative is painted
/// roughly 1:1, so an artifact is an artifact; a 2x derivative is DOWNSAMPLED by the
/// device before the viewer sees it (320 px of image into a 263–300 device-px box), which
/// is exactly the operation that hides compression noise. Measured on a live cover, the
/// 320 rung goes 44.1 KB → 35.7 KB for no difference visible at card size — the other
/// half of what Lighthouse's "increasing the image compression factor" line is asking for.
const HIDPI_VARIANT_QUALITY: f32 = 75.0;

/// Concurrency bound on variant generation.
///
/// Uncached masters use [`master_cover_work`]. Variants read an already-stored,
/// small master and retain their separate two-worker lane, so a slow source fetch
/// cannot delay generation of a cached cover's card-sized derivative.
///
/// Two permits, not more: the resize is short (a card-sized derivative is a fraction of
/// `process_cover`'s work, which is itself ~130 ms), so a queued request waits
/// milliseconds, while half the box stays free to answer GraphQL. Callers WAIT for a
/// permit rather than falling back to the master — under load the master is the more
/// expensive answer, so shedding here would make a busy server send MORE bytes.
static VARIANT_RESIZE_LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Acquire a variant-resize permit, held for the lifetime of the returned guard.
pub async fn variant_resize_permit() -> tokio::sync::SemaphorePermit<'static> {
    VARIANT_RESIZE_LIMIT
        .acquire()
        .await
        .expect("VARIANT_RESIZE_LIMIT is never closed")
}

/// Parse a `?w=` query value into an allowed variant width. `None` for absent,
/// unparseable, or non-allowlisted values — every one of which means "serve the
/// master", never an error: a stale or hand-edited URL must still render a cover.
pub fn variant_width(raw: Option<&str>) -> Option<u32> {
    let n: u32 = raw?.parse().ok()?;
    VARIANT_WIDTHS.contains(&n).then_some(n)
}

/// Downscale an already-processed cover to `width` px wide and re-encode as lossy WebP.
///
/// Takes the STORED master rather than the upstream original on purpose: the master is
/// local (no refetch, no rate limiter) and already passed the decode-bomb and truncation
/// gates, so this stays a pure CPU step.
///
/// `Ok(None)` means "the master is ALREADY at or under `width`" — upscaling would cost
/// bytes to add no detail, so the caller serves the master and, importantly, stores
/// nothing. That distinction matters now that the allowlist reaches 320: masters measure
/// 240–364 px wide, so a byte-identical copy of the master would otherwise be written
/// into `cover_variant` for a large share of the catalogue.
pub fn downscale_variant(master: &[u8], width: u32) -> Result<Option<Vec<u8>>> {
    let img = decode_limited(master)?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        bail!("cover master has a zero dimension");
    }
    if w <= width {
        return Ok(None);
    }
    let th = ((h as f32 * (width as f32 / w as f32)).round() as u32).max(1);
    let resized = imageops::resize(&img.to_rgba8(), width, th, imageops::FilterType::Lanczos3);
    // A card-sized derivative is a fraction of the master's pixels, so the master's
    // quality LADDER is unnecessary here — one encode already lands far under
    // MAX_COVER_BYTES. The quality itself is per-rung: see `HIDPI_VARIANT_QUALITY`.
    let quality = if width <= CARD_VARIANT_WIDTH {
        COVER_QUALITY_LADDER[0]
    } else {
        HIDPI_VARIANT_QUALITY
    };
    encode_webp_lossy(&resized, quality).map(Some)
}

/// Read a stored cover derivative, or `None` when it has not been generated yet.
/// `asset_key` namespaces the two cover families that share this table — canonical
/// works (`w_…`) and Suwayomi source covers (`s:<manga_id>`) — so one lazy-generate
/// path serves both.
pub async fn get_cover_variant(
    covers: &SqliteDatabase,
    asset_key: &str,
    width: u32,
) -> Option<Vec<u8>> {
    sqlx::query_scalar("SELECT webp FROM cover_variant WHERE asset_key = $1 AND width = $2")
        .bind(asset_key)
        .bind(width)
        .fetch_optional(covers)
        .await
        .inspect_err(crate::db::record_read_database_error)
        .ok()
        .flatten()
}

/// Store (or replace) a cover derivative in the un-replicated covers DB.
pub async fn put_cover_variant<'e, E>(
    covers: E,
    asset_key: &str,
    width: u32,
    webp: &[u8],
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let ts = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO cover_variant (asset_key, width, webp, updated_at) VALUES ($1, $2, $3, $4) \
         ON CONFLICT(asset_key, width) DO UPDATE SET webp = excluded.webp, \
         updated_at = excluded.updated_at",
    )
    .bind(asset_key)
    .bind(width)
    .bind(webp)
    .bind(&ts)
    .execute(covers)
    .await?;
    Ok(())
}

/// Drop every derivative of one cover. Called whenever a master is (re)written, so a
/// re-saved cover cannot keep serving a derivative of the OLD art. The public URL is
/// `?v=`-busted for works, but the Suwayomi cover path has no version token at all —
/// there, this delete is the only thing keeping the two in step.
#[allow(dead_code)]
pub async fn clear_cover_variants(covers: &SqliteDatabase, asset_key: &str) {
    if let Err(e) = sqlx::query("DELETE FROM cover_variant WHERE asset_key = $1")
        .bind(asset_key)
        .execute(covers)
        .await
    {
        let e = crate::db::observed_database_error(e.into());
        tracing::warn!(error = %e, asset_key, "cover variant invalidation failed");
    }
}

/// The `cover_variant.asset_key` for a Suwayomi source cover.
pub fn suwayomi_variant_key(manga_id: i64) -> String {
    format!("s:{manga_id}")
}

/// Distinguish a cold cover from a failed cache store. Public serving paths must
/// not turn a storage outage into a new source fetch.
pub async fn read_suwayomi_cover(
    covers: &SqliteDatabase,
    manga_id: i64,
) -> std::result::Result<Option<Vec<u8>>, sqlx::Error> {
    sqlx::query_scalar("SELECT webp FROM suwayomi_cover_blob WHERE manga_id = $1")
        .bind(manga_id)
        .fetch_optional(covers)
        .await
}

/// Best-effort fixture lookup. Serving uses the error-preserving helper above.
#[cfg(test)]
pub async fn get_suwayomi_cover(covers: &SqliteDatabase, manga_id: i64) -> Option<Vec<u8>> {
    read_suwayomi_cover(covers, manga_id)
        .await
        .inspect_err(crate::db::record_read_database_error)
        .ok()
        .flatten()
}

/// Store (or replace) a resized Suwayomi source cover in the un-replicated covers DB.
///
/// Test fixtures only. It takes the process-local asset mutex but not the PostgreSQL
/// Cover key and overwrites whatever master is there, so no production path may use it:
/// they publish through [`publish_suwayomi_cover`] or
/// [`publish_suwayomi_cover_if_missing`] (T080).
#[cfg(test)]
pub async fn put_suwayomi_cover(covers: &SqliteDatabase, manga_id: i64, webp: &[u8]) -> Result<()> {
    let _asset = asset_mutex(&suwayomi_variant_key(manga_id)).await;
    put_suwayomi_cover_on(covers, manga_id, webp).await
}

async fn put_suwayomi_cover_on(covers: &SqliteDatabase, manga_id: i64, webp: &[u8]) -> Result<()> {
    let mut cache = covers.begin().await?;
    write_suwayomi_cover_on(&mut cache, manga_id, webp).await?;
    cache.commit().await?;
    Ok(())
}

async fn write_suwayomi_cover_on(
    cache: &mut sqlx::SqliteConnection,
    manga_id: i64,
    webp: &[u8],
) -> Result<()> {
    let ts = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO suwayomi_cover_blob (manga_id, webp, updated_at) VALUES ($1, $2, $3) \
         ON CONFLICT(manga_id) DO UPDATE SET webp = excluded.webp, updated_at = excluded.updated_at",
    )
    .bind(manga_id)
    .bind(webp)
    .bind(&ts)
    .execute(&mut *cache)
    .await?;
    // This path has no `?v=` token in its public URL, so a stale derivative would
    // outlive its master indefinitely. Dropping them here is the only invalidation.
    sqlx::query("DELETE FROM cover_variant WHERE asset_key = $1")
        .bind(suwayomi_variant_key(manga_id))
        .execute(&mut *cache)
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
pub async fn mangadex_cover_anchor(main: &Database, work_id: &str) -> Option<(String, String)> {
    let row = crate::db_types::query_tuple::<(Option<String>, Option<String>)>(
        "SELECT ss.source_key, w.cover_file_name \
         FROM work w \
         JOIN source_series ss ON ss.work_id = w.id AND ss.source_type = 'mangadex' \
         WHERE w.id = $1 \
         ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1",
    )
    .bind(work_id)
    .fetch_optional(main)
    .await
    .inspect_err(crate::db::record_read_database_error)
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
pub async fn asset_mutex(asset: &str) -> tokio::sync::OwnedMutexGuard<()> {
    type Assets = std::collections::HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>;
    static ASSETS: std::sync::LazyLock<std::sync::Mutex<Assets>> =
        std::sync::LazyLock::new(Default::default);
    let mutex = {
        let mut assets = ASSETS.lock().unwrap_or_else(|p| p.into_inner());
        assets.retain(|_, mutex| mutex.strong_count() > 0);
        match assets.get(asset).and_then(std::sync::Weak::upgrade) {
            Some(mutex) => mutex,
            None => {
                let mutex = std::sync::Arc::new(tokio::sync::Mutex::new(()));
                assets.insert(asset.to_owned(), std::sync::Arc::downgrade(&mutex));
                mutex
            }
        }
    };
    mutex.lock_owned().await
}

pub async fn lock_asset(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    asset: &str,
) -> Result<tokio::sync::OwnedMutexGuard<()>> {
    crate::locks::advisory(
        tx,
        &[crate::locks::AdvisoryKey::new(
            crate::locks::KeyClass::Cover,
            &[asset],
        )],
    )
    .await?;
    Ok(asset_mutex(asset).await)
}

/// [`lock_asset`] for several assets of one transaction: every Cover key in ONE sorted
/// advisory call, then the in-process guards in sorted order.
///
/// A single-asset holder takes its advisory key before its in-process guard as well, so
/// a holder of one in-process guard never waits for a key this call already holds.
pub async fn lock_assets(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    assets: &[&str],
) -> Result<Vec<tokio::sync::OwnedMutexGuard<()>>> {
    let mut assets = assets.to_vec();
    assets.sort_unstable();
    assets.dedup();
    let keys: Vec<_> = assets
        .iter()
        .map(|asset| crate::locks::AdvisoryKey::new(crate::locks::KeyClass::Cover, &[*asset]))
        .collect();
    crate::locks::advisory(tx, &keys).await?;
    let mut guards = Vec::with_capacity(assets.len());
    for asset in assets {
        guards.push(asset_mutex(asset).await);
    }
    Ok(guards)
}

pub async fn put_current_variant(
    main: &Database,
    covers: &SqliteDatabase,
    asset: &str,
    width: u32,
    master: &[u8],
    derivative: &[u8],
) -> Result<()> {
    let mut tx = main.begin().await?;
    let _asset = lock_asset(&mut tx, asset).await?;
    let mut cache = covers.begin().await?;
    let current: Option<Vec<u8>> = if let Some(id) = asset.strip_prefix("s:") {
        sqlx::query_scalar("SELECT webp FROM suwayomi_cover_blob WHERE manga_id = $1")
            .bind(id.parse::<i64>()?)
            .fetch_optional(&mut *cache)
            .await?
    } else {
        sqlx::query_scalar("SELECT webp FROM work_cover_blob WHERE work_id = $1")
            .bind(asset)
            .fetch_optional(&mut *cache)
            .await?
    };
    if current.as_deref() == Some(master) {
        put_cover_variant(&mut *cache, asset, width, derivative).await?;
    }
    cache.commit().await?;
    tx.commit().await?;
    Ok(())
}

pub async fn publish_suwayomi_cover(
    main: &Database,
    covers: &SqliteDatabase,
    manga_id: i64,
    webp: &[u8],
) -> Result<()> {
    let mut tx = main.begin().await?;
    let _asset = lock_asset(&mut tx, &suwayomi_variant_key(manga_id)).await?;
    put_suwayomi_cover_on(covers, manga_id, webp).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn publish_suwayomi_cover_if_missing(
    main: &Database,
    covers: &SqliteDatabase,
    manga_id: i64,
    webp: &[u8],
) -> Result<bool> {
    let mut tx = main.begin().await?;
    let _asset = lock_asset(&mut tx, &suwayomi_variant_key(manga_id)).await?;
    let mut cache = covers.begin().await?;
    let present: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM suwayomi_cover_blob WHERE manga_id = $1)")
            .bind(manga_id)
            .fetch_one(&mut *cache)
            .await?;
    if present {
        cache.rollback().await?;
        tx.rollback().await?;
        return Ok(false);
    }
    write_suwayomi_cover_on(&mut cache, manga_id, webp).await?;
    cache.commit().await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
tokio::task_local! { pub(crate) static COVER_PUBLICATION_PAUSE: crate::locks::pause_points::PausePoints; }

pub async fn put_work_cover(
    main: &Database,
    covers: &SqliteDatabase,
    work_id: &str,
    webp: &[u8],
) -> Result<()> {
    publish_work_cover(main, covers, work_id, webp, None, None).await?;
    Ok(())
}

pub async fn put_work_cover_if_current(
    main: &Database,
    covers: &SqliteDatabase,
    work_id: &str,
    webp: &[u8],
    expected: Option<&[u8]>,
) -> Result<bool> {
    publish_work_cover(main, covers, work_id, webp, Some(expected), None).await
}

pub async fn put_work_cover_for_missing_anchor(
    main: &Database,
    covers: &SqliteDatabase,
    work_id: &str,
    webp: &[u8],
    mangadex_id: &str,
    file_name: &str,
) -> Result<bool> {
    publish_work_cover(
        main,
        covers,
        work_id,
        webp,
        Some(None),
        Some((mangadex_id, file_name)),
    )
    .await
}

async fn publish_work_cover(
    main: &Database,
    covers: &SqliteDatabase,
    work_id: &str,
    webp: &[u8],
    expected: Option<Option<&[u8]>>,
    expected_anchor: Option<(&str, &str)>,
) -> Result<bool> {
    let now = Utc::now();

    let ts = now.to_rfc3339();
    let mut tx = main.begin().await?;
    let _asset = lock_asset(&mut tx, work_id).await?;
    if !crate::locks::lock_works(&mut tx, &[work_id])
        .await?
        .missing
        .is_empty()
    {
        tx.rollback().await?;
        return Ok(false);
    }
    if let Some((mangadex_id, file_name)) = expected_anchor {
        let current: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT ss.source_key, w.cover_file_name FROM work w JOIN source_series ss ON ss.work_id = w.id AND ss.source_type = 'mangadex' WHERE w.id = $1 ORDER BY ss.created_at, ss.id LIMIT 1"
        ).bind(work_id).fetch_optional(&mut *tx).await?;
        if current
            .as_ref()
            .map(|(id, file)| (id.as_str(), file.as_deref()))
            != Some((mangadex_id, Some(file_name)))
        {
            tx.rollback().await?;
            return Ok(false);
        }
    }
    if let Some(expected) = expected {
        let current: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT webp FROM work_cover_blob WHERE work_id = $1")
                .bind(work_id)
                .fetch_optional(covers)
                .await?;
        if current.as_deref() != expected {
            tx.rollback().await?;
            return Ok(false);
        }
    }
    let previous: Option<i64> =
        sqlx::query_scalar("SELECT version FROM work_cover_blob WHERE work_id = $1")
            .bind(work_id)
            .fetch_optional(covers)
            .await?;
    let version = now.timestamp_millis().max(
        previous
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("cover version exhausted"))?,
    );
    #[cfg(test)]
    if let Ok(points) = COVER_PUBLICATION_PAUSE.try_with(Clone::clone) {
        points.hit("cover-locked").await;
    }
    let mut cache = covers.begin().await?;
    // 1) Blob first, into the un-replicated covers DB.
    sqlx::query(
        "INSERT INTO work_cover_blob (work_id, webp, version, updated_at) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT(work_id) DO UPDATE SET \
           webp = excluded.webp, version = excluded.version, updated_at = excluded.updated_at",
    )
    .bind(work_id)
    .bind(webp)
    .bind(version)
    .bind(&ts)
    .execute(&mut *cache)
    .await?;
    // Derivatives of the PREVIOUS master are now wrong art. Drop them here rather than
    // relying on the `?v=` bust: that only stops browsers and the edge from reusing the
    // old bytes, it does not touch what we would hand the next cold request.
    sqlx::query("DELETE FROM cover_variant WHERE asset_key = $1")
        .bind(work_id)
        .execute(&mut *cache)
        .await?;
    cache.commit().await?;
    // 2) Publish the version and every materialized card URL atomically. Updating
    // only work left library/browse cards pointing at immutable damaged thumbnails
    // even after an admin had successfully fetched a replacement.
    sqlx::query("UPDATE work SET cover_cached_version = $1 WHERE id = $2")
        .bind(version)
        .bind(work_id)
        .execute(&mut *tx)
        .await?;
    let url = cover_path(work_id, version);
    for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
        sqlx::query(&format!(
            "UPDATE {table} SET cover_url = $1 WHERE work_id = $2"
        ))
        .bind(&url)
        .bind(work_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(true)
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
pub async fn pending_cover_count(pool: &Database) -> Result<i64> {
    // Uncached works we can materialize a cover for: a MangaDex-anchored work with a
    // cover file name, OR a Suwayomi-anchored work (cover comes from its thumbnail).
    //
    // The `work_cover_issue` exclusion mirrors BOTH crawl SELECTs (see
    // `crawl_uncached_covers`' `BASE` and `SUW_BASE`). Without it this counted works the
    // crawl will never select: measured live, 27 "pending" against 1 actually selectable,
    // because 26 works carry a recorded issue. That made the drainer's `Ok(0) => nothing
    // to do` short-circuit unreachable — it launched a full crawl pass every 300 s to
    // process a single row — and overstated the operator-facing backlog by 27x.
    let n: i64 = crate::db_types::query_scalar(
        "SELECT COUNT(*) FROM work w \
         WHERE w.cover_cached_version IS NULL \
           AND NOT EXISTS (SELECT 1 FROM work_cover_issue i WHERE i.work_id = w.id \
                           AND (i.retry_after IS NULL OR i.retry_after > $1)) AND ( \
             (w.cover_file_name IS NOT NULL \
              AND EXISTS (SELECT 1 FROM source_series ss \
                          WHERE ss.work_id = w.id AND ss.source_type = 'mangadex')) \
             OR EXISTS (SELECT 1 FROM source_series ss \
                        WHERE ss.work_id = w.id AND ss.source_type = 'suwayomi'))",
    )
    .bind(Utc::now())
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
pub async fn record_cover_issue(
    pool: &Database,
    work_id: &str,
    reason: &'static str,
    detail: &str,
) {
    // The historical callers of this function all record DETERMINISTIC failures, so the
    // marker gets the long (permanent) window. Transient failures go through
    // `record_cover_issue_backoff` instead, which is what stops the crawl re-fetching a
    // 403ing source every 5 minutes for ever.
    record_cover_issue_backoff(pool, work_id, CoverFailure::Permanent(reason), detail).await;
}

#[cfg(test)]
tokio::task_local! {
    static COVER_BACKOFF_PAUSE: crate::locks::pause_points::PausePoints;
    static COVER_BACKOFF_RESULTS: tokio::sync::mpsc::UnboundedSender<std::result::Result<(), String>>;
}

/// Record (or refresh) a work cover failure with a `retry_after` computed from the
/// failure class and the attempt count — the backoff half of the 0043 skip-marker.
///
/// Permanent: one long window (the marker still expires, so a decoder/cap improvement is
/// picked up automatically). Transient: 30 min doubling to a 24 h ceiling, so a 403/429
/// costs at most one probe a day instead of 288.
pub async fn record_cover_issue_backoff(
    pool: &Database,
    work_id: &str,
    failure: CoverFailure,
    detail: &str,
) {
    let result: Result<()> = async {
        let mut tx = pool.begin().await?;
        let _asset = lock_asset(&mut tx, work_id).await?;
        #[cfg(test)]
        if let Ok(points) = COVER_BACKOFF_PAUSE.try_with(Clone::clone) {
            points.hit("backoff-locked").await;
        }
        let now = Utc::now();
        // Truncate an over-long error so a pathological message can't bloat the row.
        let detail: String = detail.chars().take(500).collect();
        let prev: i64 = crate::db_types::query_scalar(
            "SELECT attempts FROM work_cover_issue WHERE work_id = $1",
        )
        .bind(work_id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(0);
        let retry_after = failure.retry_after(now, prev + 1);
        sqlx::query(
            "INSERT INTO work_cover_issue \
           (work_id, reason, detail, attempts, first_seen, last_seen, retry_after) \
         VALUES ($1, $2, $3, 1, $4, $5, $6) \
         ON CONFLICT(work_id) DO UPDATE SET \
           reason = excluded.reason, detail = excluded.detail, \
           attempts = work_cover_issue.attempts + 1, last_seen = excluded.last_seen, \
           retry_after = excluded.retry_after",
        )
        .bind(work_id)
        .bind(failure.reason())
        .bind(&detail)
        .bind(now)
        .bind(now)
        .bind(crate::db_types::date(&retry_after).expect("internally generated UTC timestamp"))
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }
    .await;
    #[cfg(test)]
    let _ = COVER_BACKOFF_RESULTS
        .try_with(|results| results.send(result.as_ref().map(|_| ()).map_err(ToString::to_string)));
    if let Err(e) = result {
        crate::db::observe_database_error(&e);
        tracing::warn!(error = %e, "cover backoff write failed");
    }
}

/// Clear a work's recorded cover issue (a later attempt — auto or manual — succeeded,
/// so it should re-enter the normal cached/uncached flow). Best-effort.
pub async fn clear_cover_issue(pool: &Database, work_id: &str) {
    if let Err(e) = sqlx::query("DELETE FROM work_cover_issue WHERE work_id = $1")
        .bind(work_id)
        .execute(pool)
        .await
    {
        let e = crate::db::observed_database_error(e.into());
        tracing::warn!(work_id, error = %e, "cover issue: failed to clear");
    }
}

// ---------------------------------------------------------------------------
// Retry backoff (migration 0100)
// ---------------------------------------------------------------------------

/// First transient retry window. A source-side failure (403/429/5xx/network) may lapse
/// on its own, so it must be retried — but not on the next 5-minute tick, which is what
/// turned 92 permanently-failing covers into ~26k pointless outbound fetches a day.
pub const TRANSIENT_BACKOFF_BASE_SECS: i64 = 30 * 60;

/// Ceiling for the transient ladder. A cover is never abandoned: at worst it is probed
/// once a day until it works or its source comes back.
pub const TRANSIENT_BACKOFF_MAX_SECS: i64 = 24 * 60 * 60;

/// Window for a DETERMINISTIC failure (a `data:` thumbnail, an over-cap GIF, an AVIF we
/// have no decoder for). Long enough that retrying costs nothing measurable, finite so a
/// decoder/cap change (or a source that quietly fixed its asset) is picked up without an
/// operator DELETE.
pub const PERMANENT_BACKOFF_SECS: i64 = 30 * 24 * 60 * 60;

/// Transient failures from ONE source within a single tick before the whole source is
/// parked. Per-cover backoff alone is not enough: 28 individually-backed-off covers at
/// one host still means 28 requests per tick at a host that is banning us.
pub const SOURCE_BACKOFF_THRESHOLD: usize = 3;

/// First park window for a source, doubled per consecutive park up to
/// [`SOURCE_BACKOFF_MAX_SECS`].
pub const SOURCE_BACKOFF_BASE_SECS: i64 = 60 * 60;

/// Ceiling for the per-source park (a WAF ban is measured in days, but we still probe
/// daily so an unblock is noticed by itself).
pub const SOURCE_BACKOFF_MAX_SECS: i64 = 24 * 60 * 60;

/// A cover failure and whether it can EVER succeed again unprompted.
///
/// `Permanent` = deterministic given the same input bytes/URL (the retry is pure waste
/// until something changes); `Transient` = the source or the network might behave
/// differently later. The distinction is the whole point of the backoff: permanent gets
/// one long window, transient gets an exponential ladder with a ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverFailure {
    Permanent(&'static str),
    Transient(&'static str),
}

impl CoverFailure {
    /// Stable machine code stored in `*_cover_issue.reason` (admin Bugs panel).
    pub fn reason(self) -> &'static str {
        match self {
            Self::Permanent(r) | Self::Transient(r) => r,
        }
    }

    pub fn is_permanent(self) -> bool {
        matches!(self, Self::Permanent(_))
    }

    /// Seconds to wait after the `attempts`-th consecutive failure.
    pub fn backoff_secs(self, attempts: i64) -> i64 {
        match self {
            Self::Permanent(_) => PERMANENT_BACKOFF_SECS,
            Self::Transient(_) => {
                // 30m, 1h, 2h, 4h, … capped at 24h. The shift is clamped so a marker
                // that has been failing for months cannot overflow the shift.
                let shift = attempts.clamp(1, 20) - 1;
                TRANSIENT_BACKOFF_BASE_SECS
                    .saturating_mul(1i64 << shift.min(20))
                    .min(TRANSIENT_BACKOFF_MAX_SECS)
            }
        }
    }

    fn retry_after(self, now: chrono::DateTime<Utc>, attempts: i64) -> String {
        (now + chrono::Duration::seconds(self.backoff_secs(attempts))).to_rfc3339()
    }
}

/// The HTTP status carried by our own `Suwayomi cover error {status}` message, if any.
///
/// Deliberately anchored on that literal prefix rather than "first 3-digit number in the
/// string": a reqwest transport error Displays the whole URL, and
/// `/api/v1/manga/404/thumbnail` would otherwise be read as a 404 and freeze a network
/// blip into a permanent marker.
fn cover_error_status(msg: &str) -> Option<u16> {
    let rest = msg.split("cover error ").nth(1)?;
    rest.split_whitespace()
        .next()?
        .parse::<u16>()
        .ok()
        .filter(|c| (100..=599).contains(c))
}

/// Classify an upstream cover FETCH failure (as opposed to a decode/encode failure —
/// see [`classify_process_failure`]).
///
/// Permanent here means "this URL cannot produce an image", not "this work has no
/// cover": the marker still expires, and a Suwayomi marker is additionally voided the
/// moment the extension re-fetches the manga.
pub fn classify_fetch_error(err: &anyhow::Error) -> CoverFailure {
    let s = err.to_string().to_ascii_lowercase();
    // Suwayomi answers 400 with this body when the extension stored a `data:` URI as the
    // manga thumbnail (46 of the 92 permanently-failing covers). Nothing about a later
    // fetch can change that — only a re-fetch of the manga can.
    if s.contains("but was 'data'") || s.contains("expected url scheme") {
        return CoverFailure::Permanent("data_uri");
    }
    if s.contains("exceeds cap") || s.contains("source too large") {
        return CoverFailure::Permanent("too_large");
    }
    if s.contains("truncated") {
        // The image is fine, the download was cut short (see `is_transient_cover_error`).
        return CoverFailure::Transient("truncated");
    }
    match cover_error_status(&s) {
        // Deterministic for this URL: a malformed/absent upstream reference.
        Some(400) => CoverFailure::Permanent("bad_request"),
        Some(404) | Some(410) => CoverFailure::Permanent("not_found"),
        Some(415) => CoverFailure::Permanent("unsupported_media"),
        Some(451) => CoverFailure::Permanent("unavailable_legal"),
        // WAF / auth walls and rate limits DO lapse — Philia's block is expected to.
        Some(401) | Some(403) => CoverFailure::Transient("blocked"),
        Some(429) => CoverFailure::Transient("rate_limited"),
        Some(c) if (500..=599).contains(&c) => CoverFailure::Transient("upstream_5xx"),
        // Anything else (other 4xx, no status at all: timeouts, DNS, connection refused).
        _ => CoverFailure::Transient("fetch"),
    }
}

/// Classify a `process_cover` (decode/encode) failure, reusing the existing reason codes.
/// Everything except a truncated download is deterministic for the same bytes.
pub fn classify_process_failure(err: &anyhow::Error) -> CoverFailure {
    let reason = classify_cover_error(err);
    if is_transient_cover_error(err) {
        CoverFailure::Transient(reason)
    } else {
        CoverFailure::Permanent(reason)
    }
}

/// Record (or refresh) a Suwayomi-manga cover failure so the warmer stops re-fetching it
/// every tick.
///
/// Keyed by the Suwayomi manga id, NOT by work id: `work_cover_issue` is the precedent
/// for this whole mechanism, but 10,494 of 16,646 `suwayomi_series` have no `work` row
/// (Browse / unenrolled), and its `work_id` column is a foreign key into `work` — the
/// bulk of the failing covers simply cannot be represented there.
///
/// `fetched_at` is `suwayomi_series.last_fetched_at` at the time of the failure; storing
/// it is what makes even a PERMANENT marker self-healing (see
/// [`warm_candidates`]). Best-effort: failing to write a marker only means we retry.
pub async fn record_suwayomi_cover_issue(
    main: &Database,
    manga_id: i64,
    source_id: Option<&str>,
    fetched_at: Option<&str>,
    failure: CoverFailure,
    detail: &str,
) {
    let result: Result<()> = async {
        let mut tx = main.begin().await?;
        let _asset = lock_asset(&mut tx, &suwayomi_variant_key(manga_id)).await?;
        #[cfg(test)]
        if let Ok(points) = COVER_BACKOFF_PAUSE.try_with(Clone::clone) {
            points.hit("backoff-locked").await;
        }
        let now = Utc::now();
        let detail: String = detail.chars().take(500).collect();
        let prev: Option<(i64, Option<String>)> = crate::db_types::query_tuple(
            "SELECT attempts, source_fetched_at FROM suwayomi_cover_issue WHERE manga_id = $1",
        )
        .bind(manga_id)
        .fetch_optional(&mut *tx)
        .await?;
        // A manga the extension has re-fetched since the last failure is a NEW input: start
        // the ladder again rather than inheriting a 24h step earned by the old thumbnail.
        let attempts = match &prev {
            Some((n, seen)) if seen.as_deref() == fetched_at => n + 1,
            _ => 1,
        };
        let retry_after = failure.retry_after(now, attempts);
        sqlx::query(
            "INSERT INTO suwayomi_cover_issue \
           (manga_id, source_id, reason, detail, permanent, attempts, first_seen, \
            last_seen, retry_after, source_fetched_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT(manga_id) DO UPDATE SET \
           source_id = excluded.source_id, reason = excluded.reason, \
           detail = excluded.detail, permanent = excluded.permanent, \
           attempts = excluded.attempts, last_seen = excluded.last_seen, \
           retry_after = excluded.retry_after, \
           source_fetched_at = excluded.source_fetched_at",
        )
        .bind(manga_id)
        .bind(source_id)
        .bind(failure.reason())
        .bind(&detail)
        .bind(failure.is_permanent())
        .bind(attempts)
        .bind(now)
        .bind(now)
        .bind(crate::db_types::date(&retry_after).expect("internally generated UTC timestamp"))
        .bind(fetched_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }
    .await;
    #[cfg(test)]
    let _ = COVER_BACKOFF_RESULTS
        .try_with(|results| results.send(result.as_ref().map(|_| ()).map_err(ToString::to_string)));
    if let Err(e) = result {
        crate::db::observe_database_error(&e);
        tracing::warn!(error = %e, "cover backoff write failed");
    }
}

/// Clear a Suwayomi cover marker — a fetch succeeded, so the cover is back in the normal
/// cached/uncached flow. Best-effort.
pub async fn clear_suwayomi_cover_issue(main: &Database, manga_id: i64) {
    if let Err(e) = sqlx::query("DELETE FROM suwayomi_cover_issue WHERE manga_id = $1")
        .bind(manga_id)
        .execute(main)
        .await
    {
        let e = crate::db::observed_database_error(e.into());
        tracing::warn!(manga_id, error = %e, "suwayomi cover issue: failed to clear");
    }
}

/// Park a whole SOURCE after a tick in which several of its covers failed transiently and
/// none succeeded.
///
/// This is the per-host half of the mechanism: a Cloudflare WAF ban is a property of the
/// host, so backing off cover-by-cover still sends one request per cover per tick at a
/// site that is actively blocking us (28/tick for philiascans.org, ~8k/day). Logged at
/// WARN — a parked source is exactly the "every cover for a source is failing" signal
/// that must NOT be silently swallowed by the markers.
pub async fn record_source_cover_backoff(
    main: &Database,
    source_id: &str,
    failure: CoverFailure,
    failed_covers: usize,
    detail: &str,
) {
    let result: Result<()> = async {
    let mut tx = main.begin().await?;
    let _asset = lock_asset(&mut tx, source_id).await?;
    #[cfg(test)]
    if let Ok(points) = COVER_BACKOFF_PAUSE.try_with(Clone::clone) { points.hit("backoff-locked").await; }
    let now = Utc::now();
    let detail: String = detail.chars().take(500).collect();
    let prev: i64 = crate::db_types::query_scalar(
        "SELECT failures FROM cover_source_backoff WHERE source_id = $1",
    )
    .bind(source_id)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(0);
    let failures = prev + 1;
    let secs = SOURCE_BACKOFF_BASE_SECS
        .saturating_mul(1i64 << (failures.clamp(1, 20) - 1).min(20))
        .min(SOURCE_BACKOFF_MAX_SECS);
    let retry_after = (now + chrono::Duration::seconds(secs)).to_rfc3339();
    sqlx::query(
        "INSERT INTO cover_source_backoff \
           (source_id, reason, detail, failures, failed_covers, first_seen, last_seen, retry_after) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT(source_id) DO UPDATE SET \
           reason = excluded.reason, detail = excluded.detail, failures = excluded.failures, \
           failed_covers = excluded.failed_covers, last_seen = excluded.last_seen, \
           retry_after = excluded.retry_after",
    )
    .bind(source_id)
    .bind(failure.reason())
    .bind(&detail)
    .bind(failures)
    .bind(failed_covers as i64)
    .bind(now)
    .bind(now)
    .bind(crate::db_types::date(&retry_after).expect("internally generated UTC timestamp"))
    .execute(&mut *tx)
    .await?;

    tracing::warn!(
        source_id,
        reason = failure.reason(),
        failed_covers,
        consecutive_parks = failures,
        retry_after = %retry_after,
        "cover warmer: parking a whole source (every cover attempted this tick failed)"
    );
    tx.commit().await?;
    Ok(())
    }.await;
    #[cfg(test)]
    let _ = COVER_BACKOFF_RESULTS
        .try_with(|results| results.send(result.as_ref().map(|_| ()).map_err(ToString::to_string)));
    if let Err(e) = result {
        crate::db::observe_database_error(&e);
        tracing::warn!(error = %e, "cover backoff write failed");
    }
}

/// Un-park a source: one of its covers succeeded, so whatever blocked it has lifted.
/// Also resets the escalation counter, so the next park starts at the base window again.
pub async fn clear_source_cover_backoff(main: &Database, source_id: &str) {
    if let Err(e) = sqlx::query("DELETE FROM cover_source_backoff WHERE source_id = $1")
        .bind(source_id)
        .execute(main)
        .await
    {
        let e = crate::db::observed_database_error(e.into());
        tracing::warn!(source_id, error = %e, "cover source backoff: failed to clear");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RepairCoverSource {
    MangaDex { manga_id: String, file_name: String },
    Suwayomi { path: String },
}

async fn repair_cover_sources(main: &Database, work_id: &str) -> Result<Vec<RepairCoverSource>> {
    let md = crate::db_types::query_tuple::<(Option<String>, Option<String>)>(
        "SELECT (SELECT ss.source_key FROM source_series ss \
                 WHERE ss.work_id = w.id AND ss.source_type = 'mangadex' \
                 ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1), \
                w.cover_file_name FROM work w WHERE w.id = $1",
    )
    .bind(work_id)
    .fetch_optional(main)
    .await?;
    let mut sources = Vec::new();
    if let Some((Some(manga_id), Some(file_name))) = md {
        if !manga_id.is_empty() && !file_name.is_empty() {
            sources.push(RepairCoverSource::MangaDex {
                manga_id,
                file_name,
            });
        }
    }
    // Every linked engine manga can supply a cover, even before its metadata is
    // mirrored locally. Use the internal thumbnail endpoint rather than requiring
    // a cached thumbnail_url or following a stale public/CDN URL.
    let keys: Vec<String> = crate::db_types::query_scalar(
        "SELECT source_key FROM source_series WHERE work_id = $1 \
         AND source_type = 'suwayomi' ORDER BY created_at ASC, id ASC",
    )
    .bind(work_id)
    .fetch_all(main)
    .await?;
    for key in keys {
        let Ok(id) = key.parse::<i64>() else { continue };
        if id < 0 {
            continue;
        }
        let candidate = RepairCoverSource::Suwayomi {
            path: format!("/api/v1/manga/{id}/thumbnail"),
        };
        if !sources.contains(&candidate) {
            sources.push(candidate);
        }
    }
    Ok(sources)
}

async fn process_repair_sources<F, Fut>(
    sources: Vec<RepairCoverSource>,
    mut fetch: F,
) -> Result<Option<Vec<u8>>>
where
    F: FnMut(RepairCoverSource) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<u8>>>,
{
    let mut last_error = None;
    for source in sources {
        let work = master_cover_work().await;
        let Some(bytes) = fetch(source).await else {
            continue;
        };
        // A download is only usable after it has decoded and processed. A corrupt
        // MangaDex image must not prevent trying Suwayomi, nor may a bad first
        // Suwayomi source prevent trying the remaining linked sources.
        match work.process(move || process_cover(&bytes)).await? {
            Ok(webp) => return Ok(Some(webp)),
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

/// Admin cover repair: try MangaDex and every linked Suwayomi source until one
/// supplies a complete, processable image. Works without MangaDex use Suwayomi
/// directly. Only store a successful replacement; leave the current cover intact
/// if every source fails.
pub async fn retry_one_cover(
    main: &Database,
    covers: &SqliteDatabase,
    mangadex: &MangaDexClient,
    suwayomi: &crate::suwayomi::SuwayomiClient,
    work_id: &str,
) -> Result<bool> {
    let mut sources = repair_cover_sources(main, work_id).await?;
    // With MangaDex switched off the repair simply uses the linked Suwayomi sources;
    // offering the MangaDex one would only collect the client's refusal.
    if !mangadex.is_enabled() {
        sources.retain(|s| !matches!(s, RepairCoverSource::MangaDex { .. }));
    }
    let processed = process_repair_sources(sources, |source| async move {
        match source {
            RepairCoverSource::MangaDex {
                manga_id,
                file_name,
            } => mangadex.cover_thumb_bytes(&manga_id, &file_name).await,
            RepairCoverSource::Suwayomi { path } => suwayomi.cover_bytes(Some(&path)).await,
        }
    })
    .await;
    match processed {
        Ok(Some(webp)) => {
            put_work_cover(main, covers, work_id, &webp).await?;
            clear_cover_issue(main, work_id).await;
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(error) => {
            // Do not freeze truncated downloads or blocking-task failures into the
            // issues table. A deterministic image failure is recorded only after
            // all alternative sources have had a chance to repair it.
            if !is_transient_cover_error(&error)
                && error.downcast_ref::<tokio::task::JoinError>().is_none()
            {
                record_cover_issue(
                    main,
                    work_id,
                    classify_cover_error(&error),
                    &error.to_string(),
                )
                .await;
            }
            Err(error.context("cover processing failed"))
        }
    }
}

/// Fetch + store covers for EVERY canonical work that still lacks one (bounded by
/// the MangaDex rate limiter inside `cover_thumb_bytes`, so this is a polite,
/// slow background crawl). Idempotent and resumable: it only ever selects works
/// with `cover_cached_version IS NULL` (and no recorded `work_cover_issue`), so a
/// re-run drains whatever remains (e.g. works whose upstream fetch failed last
/// time). Best-effort per work — one failure never aborts the crawl.
pub async fn crawl_uncached_covers(
    main: &Database,
    covers: &SqliteDatabase,
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
           AND NOT EXISTS (SELECT 1 FROM work_cover_issue i WHERE i.work_id = w.id \
                           AND (i.retry_after IS NULL OR i.retry_after > $1))";
    // A marker only hides a work while its backoff is unexpired (0100), so `now` is
    // bound into every crawl SELECT rather than the marker being an absolute exclusion.
    let now = Utc::now().to_rfc3339();
    let loaded = match limit {
        Some(n) => {
            crate::db_types::query_tuple::<(String, Option<String>, Option<String>)>(&format!(
                "{BASE} LIMIT $2"
            ))
            .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
            .bind(n)
            .fetch_all(main)
            .await
        }
        None => {
            crate::db_types::query_tuple::<(String, Option<String>, Option<String>)>(BASE)
                .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
                .fetch_all(main)
                .await
        }
    };
    let pending = match loaded {
        Ok(rows) => rows,
        Err(e) => {
            let e = crate::db::observed_database_error(e.into());
            tracing::warn!(error = %e, "cover crawl: failed to load pending works");
            return;
        }
    };

    // THE SPLIT. This first pass is the only MangaDex part of the cover pipeline; the
    // Suwayomi pass below and the warmer in `run_loop` never touch MangaDex. With the
    // switch off the pass is skipped outright — not attempted — because every fetch
    // would come back refused and be recorded as a failed cover with a backoff,
    // burying the works the moment MangaDex is turned back on.
    let pending = if mangadex.is_enabled() {
        pending
    } else {
        if !pending.is_empty() {
            tracing::debug!("cover crawl: MangaDex is off; skipping the MangaDex pass");
        }
        Vec::new()
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
        let work = master_cover_work().await;
        match mangadex
            .cover_thumb_bytes(&job.mangadex_id, &job.file_name)
            .await
        {
            Some(bytes) => match work.process(move || process_cover(&bytes)).await {
                Ok(Ok(webp)) => match put_work_cover(main, covers, &job.work_id, &webp).await {
                    Ok(()) => {
                        saved += 1;
                        clear_cover_issue(main, &job.work_id).await;
                    }
                    Err(e) => {
                        failed += 1;
                        let e = crate::db::observed_database_error(e);
                        tracing::warn!(work_id = %job.work_id, error = %e, "cover crawl: store failed");
                        record_cover_issue(main, &job.work_id, "store", &e.to_string()).await;
                    }
                },
                // Decode/encode/size failure — record it so the crawl stops re-attempting
                // this work every tick. A deterministic one gets the long window; a
                // truncated DOWNLOAD (the image is fine, the transfer was cut short) gets
                // the transient ladder, so it is retried soon but not 288 times a day.
                Ok(Err(e)) => {
                    failed += 1;
                    let e = crate::db::observed_database_error(e);
                    tracing::warn!(work_id = %job.work_id, error = %e, "cover crawl: encode failed");
                    record_cover_issue_backoff(
                        main,
                        &job.work_id,
                        classify_process_failure(&e),
                        &e.to_string(),
                    )
                    .await;
                }
                Err(e) => {
                    failed += 1;
                    let e = crate::db::observed_database_error(e.into());
                    tracing::warn!(work_id = %job.work_id, error = %e, "cover crawl: encode task panicked");
                }
            },
            // Upstream fetch failure is TRANSIENT (rate limit / network). It IS recorded
            // now — with a 30 min → 24 h backoff, not an exclusion — because "don't
            // record, just retry next tick" is what had the drainer re-fetching the same
            // permanently-failing covers every 5 minutes for ever.
            None => {
                failed += 1;
                tracing::warn!(work_id = %job.work_id, "cover crawl: upstream fetch failed");
                record_cover_issue_backoff(
                    main,
                    &job.work_id,
                    CoverFailure::Transient("fetch"),
                    "upstream cover fetch failed",
                )
                .await;
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
           AND NOT EXISTS (SELECT 1 FROM work_cover_issue i WHERE i.work_id = w.id \
                           AND (i.retry_after IS NULL OR i.retry_after > $1)) \
           AND EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id \
                       AND ss.source_type = 'suwayomi')";
    let suw_loaded = match limit {
        Some(n) => {
            crate::db_types::query_tuple::<(String, Option<String>)>(&format!(
                "{SUW_BASE} LIMIT $2"
            ))
            .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
            .bind(n)
            .fetch_all(main)
            .await
        }
        None => {
            crate::db_types::query_tuple::<(String, Option<String>)>(SUW_BASE)
                .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
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
            let e = crate::db::observed_database_error(e.into());
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
            let work = master_cover_work().await;
            let bytes = match suwayomi.cover_bytes(Some(&thumb)).await {
                Some(b) => b,
                None => {
                    tracing::warn!(work_id = %work_id, "cover crawl (suwayomi): upstream fetch failed");
                    // Backed off, not abandoned: these are the same 403ing scanlator
                    // hosts the warmer hits, and an every-tick retry here is the same
                    // wasted fetch (and the same ban risk) from a second code path.
                    record_cover_issue_backoff(
                        main,
                        &work_id,
                        CoverFailure::Transient("fetch"),
                        "upstream cover fetch failed",
                    )
                    .await;
                    return (0usize, 1usize);
                }
            };
            match work.process(move || process_cover(&bytes)).await {
                Ok(Ok(webp)) => match put_work_cover(main, covers, &work_id, &webp).await {
                    Ok(()) => {
                        clear_cover_issue(main, &work_id).await;
                        (1, 0)
                    }
                    Err(e) => {
                        let e = crate::db::observed_database_error(e);
                        tracing::warn!(work_id = %work_id, error = %e, "cover crawl (suwayomi): store failed");
                        record_cover_issue(main, &work_id, "store", &e.to_string()).await;
                        (0, 1)
                    }
                },
                // Record so we stop re-attempting an unprocessable source every tick;
                // the failure CLASS picks the window (see `CoverFailure`).
                Ok(Err(e)) => {
                    let e = crate::db::observed_database_error(e);
                    tracing::warn!(work_id = %work_id, error = %e, "cover crawl (suwayomi): encode failed");
                    record_cover_issue_backoff(main, &work_id, classify_process_failure(&e), &e.to_string()).await;
                    (0, 1)
                }
                Err(e) => {
                    // A spawn_blocking JoinError (panic) is not necessarily deterministic;
                    // don't permanently skip on it — let a later tick retry.
                    let e = crate::db::observed_database_error(e.into());
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

/// How much the warmer is currently DEFERRING: (covers held by an un-expired marker,
/// sources parked). The gauge exists so a backoff can never masquerade as "fully warm" —
/// [`pending_suwayomi_cover_count`] deliberately excludes deferred covers (that is what
/// stops the drainer launching a pass for work it will skip), and without this the
/// operator-facing signal for "an entire source is failing" would be a silent zero.
pub async fn suwayomi_cover_backoff_stats(main: &Database) -> Result<(i64, i64)> {
    let now = Utc::now().to_rfc3339();
    let covers: i64 = crate::db_types::query_scalar(
        "SELECT COUNT(*) FROM suwayomi_cover_issue WHERE retry_after > $1",
    )
    .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
    .fetch_one(main)
    .await?;
    let sources: i64 = crate::db_types::query_scalar(
        "SELECT COUNT(*) FROM cover_source_backoff WHERE retry_after > $1",
    )
    .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
    .fetch_one(main)
    .await?;
    Ok((covers, sources))
}

/// Cursor value meaning "start from the newest id". See [`warm_suwayomi_covers`].
pub const WARM_CURSOR_START: i64 = i64::MAX;

/// The manga-id set that already has a `suwayomi_cover_blob` row.
///
/// NOTE this is NOT the same as "rows in the table": `serve_suwayomi_cover` caches a
/// blob for ANY manga id the browser asks for, including Browse results that were never
/// enrolled into `suwayomi_series`. So the table is a superset of the candidate set and
/// `COUNT(*)` on it is not a coverage figure — see [`pending_suwayomi_cover_count`].
async fn cached_suwayomi_cover_ids(
    covers: &SqliteDatabase,
) -> Result<std::collections::HashSet<i64>> {
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
async fn thumbnailed_series_ids(main: &Database, before_id: i64) -> Result<Vec<i64>> {
    Ok(warm_candidates(main, before_id)
        .await?
        .into_iter()
        .map(|c| c.manga_id)
        .collect())
}

/// A warmer candidate: the manga id plus everything the backoff needs — the source it
/// belongs to (per-host park) and the `last_fetched_at` a marker is fingerprinted
/// against (self-healing).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WarmCandidate {
    #[sqlx(try_from = "crate::db_types::DbI64")]
    pub manga_id: i64,
    #[sqlx(try_from = "crate::db_types::DbString")]
    pub source_id: String,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub fetched_at: Option<String>,
    /// A marker row exists (expired or voided — it is not blocking this candidate, or it
    /// would not be one). Lets a success pay for a DELETE only when there is a row.
    pub had_issue: bool,
}

/// Cached series that have a usable thumbnail and are NOT currently backed off, newest
/// id first, restricted to `id < before_id`.
///
/// Three exclusions, in the SELECT rather than in Rust so the "nothing to do" gauge
/// ([`pending_suwayomi_cover_count`]) sees exactly what the warmer sees:
///
/// 1. no per-cover marker whose `retry_after` is still in the future;
/// 2. …unless the series has been re-fetched since (`source_fetched_at IS
///    s.last_fetched_at` — SQLite `IS` is null-safe). The extension re-fetching the
///    manga is precisely the event that can replace a `data:` thumbnail with a real
///    URL, so the marker is void from that moment, permanent or not;
/// 3. no un-expired park on the whole source (`cover_source_backoff`).
async fn warm_candidates(main: &Database, before_id: i64) -> Result<Vec<WarmCandidate>> {
    let now = Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, WarmCandidate>(
        "SELECT s.id AS manga_id, s.source_id AS source_id, \
                s.last_fetched_at AS fetched_at, \
                EXISTS (SELECT 1 FROM suwayomi_cover_issue i WHERE i.manga_id = s.id) AS had_issue \
           FROM suwayomi_series s \
          WHERE s.thumbnail_url IS NOT NULL AND s.thumbnail_url <> '' AND s.id < $1 \
            AND NOT EXISTS (SELECT 1 FROM suwayomi_cover_issue i \
                             WHERE i.manga_id = s.id AND i.retry_after > $2 \
                               AND i.source_fetched_at IS NOT DISTINCT FROM s.last_fetched_at) \
            AND NOT EXISTS (SELECT 1 FROM cover_source_backoff b \
                             WHERE b.source_id = s.source_id AND b.retry_after > $3) \
          ORDER BY s.id DESC",
    )
    .bind(before_id)
    .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
    .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
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
#[cfg(test)]
async fn uncached_suwayomi_cover_ids(
    main: &Database,
    covers: &SqliteDatabase,
    limit: i64,
    before_id: i64,
) -> Result<Vec<i64>> {
    Ok(
        uncached_suwayomi_cover_candidates(main, covers, limit, before_id)
            .await?
            .into_iter()
            .map(|c| c.manga_id)
            .collect(),
    )
}

/// Same set as [`uncached_suwayomi_cover_ids`], carrying the backoff metadata the warmer
/// needs (source id, fetched-at fingerprint, "has a marker row").
async fn uncached_suwayomi_cover_candidates(
    main: &Database,
    covers: &SqliteDatabase,
    limit: i64,
    before_id: i64,
) -> Result<Vec<WarmCandidate>> {
    let cached = cached_suwayomi_cover_ids(covers).await?;
    Ok(warm_candidates(main, before_id)
        .await?
        .into_iter()
        .filter(|c| !cached.contains(&c.manga_id))
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
pub async fn pending_suwayomi_cover_count(main: &Database, covers: &SqliteDatabase) -> Result<i64> {
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
    covers: &SqliteDatabase,
    main: &Database,
    suwayomi: &crate::suwayomi::SuwayomiClient,
    limit: i64,
    cursor: &std::sync::atomic::AtomicI64,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> (usize, usize) {
    use std::sync::atomic::Ordering as AtomicOrdering;
    let start = cursor.load(AtomicOrdering::Relaxed);
    let load = |before| uncached_suwayomi_cover_candidates(main, covers, limit, before);
    let ids = match load(start).await {
        // Bottom of the scan: wrap and take the newest window again, so a tick that
        // lands on the tail is not wasted.
        Ok(ids) if ids.is_empty() && start != WARM_CURSOR_START => {
            match load(WARM_CURSOR_START).await {
                Ok(ids) => ids,
                Err(e) => {
                    let e = crate::db::observed_database_error(e);
                    tracing::warn!(error = %e, "suwayomi cover warmer: failed to load candidates");
                    return (0, 0);
                }
            }
        }
        Ok(ids) => ids,
        Err(e) => {
            let e = crate::db::observed_database_error(e);
            tracing::warn!(error = %e, "suwayomi cover warmer: failed to load candidates");
            return (0, 0);
        }
    };
    // Advance BEFORE doing the work: the point is that this tick's ids are not re-taken
    // next tick whatever happens to them (including a shutdown mid-batch). A short batch
    // means we reached the bottom, so restart from the newest.
    let next = match ids.last() {
        Some(last) if (ids.len() as i64) >= limit => last.manga_id,
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
    let outcomes: Vec<(String, WarmOutcome)> = futures::stream::iter(ids)
        .map(|cand| async move {
            let WarmCandidate {
                manga_id,
                source_id,
                fetched_at,
                had_issue,
            } = cand;
            let fetched = fetched_at.as_deref();
            // Cooperative shutdown: stop taking new work the moment the signal flips,
            // rather than draining a 500-item batch through a shutting-down process.
            if *shutdown.borrow() {
                return (source_id, WarmOutcome::Skipped);
            }
            let work = master_cover_work().await;
            if *shutdown.borrow() {
                return (source_id, WarmOutcome::Skipped);
            }
            let path = format!("/api/v1/manga/{manga_id}/thumbnail");
            let bytes = match suwayomi.fetch_cover_background(&path).await {
                Ok((b, _ct)) => b,
                Err(e) => {
                    // The 92-covers-a-tick case: a `data:` thumbnail (400) is permanent,
                    // a WAF 403 / 429 / 5xx is transient. Both stop being re-fetched
                    // immediately; only the window differs.
                    let failure = classify_fetch_error(&e);
                    tracing::debug!(manga_id, reason = failure.reason(), error = %e, "suwayomi cover warmer: fetch failed");
                    record_suwayomi_cover_issue(
                        main,
                        manga_id,
                        Some(&source_id),
                        fetched,
                        failure,
                        &e.to_string(),
                    )
                    .await;
                    return (source_id, WarmOutcome::from(failure));
                }
            };
            match work.process(move || process_cover(&bytes)).await {
                Ok(Ok(webp)) => match publish_suwayomi_cover(main, covers, manga_id, &webp).await {
                    Ok(()) => {
                        if had_issue {
                            clear_suwayomi_cover_issue(main, manga_id).await;
                        }
                        (source_id, WarmOutcome::Saved)
                    }
                    Err(e) => {
                        // A local write failure says nothing about the source, so it is
                        // transient — backed off, never permanent.
                        let e = crate::db::observed_database_error(e);
                        tracing::warn!(manga_id, error = %e, "suwayomi cover warmer: store failed");
                        record_suwayomi_cover_issue(
                            main,
                            manga_id,
                            Some(&source_id),
                            fetched,
                            CoverFailure::Transient("store"),
                            &e.to_string(),
                        )
                        .await;
                        (source_id, WarmOutcome::Transient)
                    }
                },
                Ok(Err(e)) => {
                    // Undecodable (AVIF with no feature compiled in) or over-cap GIF:
                    // deterministic for these bytes. Recorded in `suwayomi_cover_issue`
                    // — the sibling of `work_cover_issue` this warmer never had, which
                    // is why it re-decoded the same broken source every 5 minutes.
                    let failure = classify_process_failure(&e);
                    tracing::debug!(manga_id, reason = failure.reason(), error = %e, "suwayomi cover warmer: encode failed");
                    record_suwayomi_cover_issue(
                        main,
                        manga_id,
                        Some(&source_id),
                        fetched,
                        failure,
                        &e.to_string(),
                    )
                    .await;
                    (source_id, WarmOutcome::from(failure))
                }
                Err(e) => {
                    // A panic in the blocking pool is not attributable to the cover; no
                    // marker, let a later tick retry.
                    let e = crate::db::observed_database_error(e.into());
                    tracing::warn!(manga_id, error = %e, "suwayomi cover warmer: encode task panicked");
                    (source_id, WarmOutcome::Transient)
                }
            }
        })
        // Never more than the background share of the shared cover-fetch pool, so an
        // on-demand `<img>` request always finds free permits and never fails fast
        // because of the warmer.
        .buffer_unordered(crate::suwayomi::WARM_COVER_CONCURRENCY)
        .collect()
        .await;

    let mut saved = 0usize;
    let mut failed = 0usize;
    // Per-source tallies drive the host-level park: per-cover backoff alone would still
    // send one request per cover per tick at a source that is banning us.
    let mut by_source: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();
    for (source_id, outcome) in outcomes {
        let entry = by_source.entry(source_id).or_insert((0, 0));
        match outcome {
            WarmOutcome::Saved => {
                saved += 1;
                entry.0 += 1;
            }
            WarmOutcome::Transient => {
                failed += 1;
                entry.1 += 1;
            }
            WarmOutcome::Permanent => failed += 1,
            WarmOutcome::Skipped => {}
        }
    }
    update_source_backoffs(main, &by_source).await;
    tracing::info!(saved, failed, total, "suwayomi cover warmer: complete");
    (saved, failed)
}

/// What one candidate did this tick. `Permanent` failures deliberately do NOT count
/// toward the per-source park: a `data:` thumbnail or an AVIF is our problem, not
/// evidence that the host is refusing us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarmOutcome {
    Saved,
    Permanent,
    Transient,
    Skipped,
}

impl From<CoverFailure> for WarmOutcome {
    fn from(f: CoverFailure) -> Self {
        if f.is_permanent() {
            Self::Permanent
        } else {
            Self::Transient
        }
    }
}

/// Apply one tick's per-source tallies: any success un-parks (and resets the escalation
/// counter for) a source; [`SOURCE_BACKOFF_THRESHOLD`] transient failures with NO success
/// parks it.
async fn update_source_backoffs(
    main: &Database,
    by_source: &std::collections::HashMap<String, (usize, usize)>,
) {
    // Which sources actually have a row, so a healthy tick doesn't fire a no-op DELETE
    // (and a write lock) per source. Tiny table — 22 sources in production.
    let parked: std::collections::HashSet<String> =
        crate::db_types::query_scalar::<String>("SELECT source_id FROM cover_source_backoff")
            .fetch_all(main)
            .await
            .inspect_err(crate::db::record_read_database_error)
            .unwrap_or_default()
            .into_iter()
            .collect();
    for (source_id, (ok, transient)) in by_source {
        if *ok > 0 {
            if parked.contains(source_id) {
                clear_source_cover_backoff(main, source_id).await;
            }
        } else if *transient >= SOURCE_BACKOFF_THRESHOLD {
            record_source_cover_backoff(
                main,
                source_id,
                CoverFailure::Transient("source_failing"),
                *transient,
                "every cover attempted for this source failed this tick",
            )
            .await;
        }
    }
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
///
/// MangaDex OFF: only the MangaDex pass of `crawl_uncached_covers` stops (it reads the
/// switch every tick). Suwayomi covers keep flowing into `work_cover_blob` and the
/// Suwayomi warmer keeps filling `suwayomi_cover_blob`, so a MangaDex-free server still
/// serves every cover from its own origin. `COVER_CACHE=off` still stops the whole loop.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    main: Database,
    covers: SqliteDatabase,
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
    main: Database,
    covers: SqliteDatabase,
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
                    Err(e) => { let e = crate::db::observed_read_database_error(e); tracing::warn!(error = %e, "cover drainer: count failed"); },
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
                        Ok(0) => {
                            // "Nothing to do" now has two causes — genuinely warm, or
                            // everything left is backed off. Say which, so a source-wide
                            // failure is visible instead of looking like full coverage.
                            match suwayomi_cover_backoff_stats(&main).await {
                                Ok((0, 0)) => { /* genuinely fully warm */ }
                                Ok((deferred, parked)) => tracing::warn!(
                                    deferred,
                                    parked_sources = parked,
                                    "suwayomi cover warmer: nothing runnable — covers/sources are backed off"
                                ),
                                Err(e) => { let e = crate::db::observed_read_database_error(e); tracing::warn!(error = %e, "suwayomi cover warmer: backoff stats failed"); },
                            }
                        }
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
                        Err(e) => { let e = crate::db::observed_read_database_error(e); tracing::warn!(error = %e, "suwayomi cover warmer: count failed"); },
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
pub(crate) async fn wait_for_writer_lock(pool: &Database) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid() AND wait_event_type = 'Lock')")
                .fetch_one(pool).await.expect("observe overlapping writer lock wait");
            if waiting { break; }
            // A poll interval, not synchronisation: the loop ends on the observed wait.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.expect("second foreground writer never waited on the guard");
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};
    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

    async fn publication_pools() -> (Database, SqliteDatabase) {
        let (main, covers) = warm_test_pools().await;
        sqlx::query("CREATE TABLE work_cover_blob(work_id TEXT PRIMARY KEY, webp BLOB NOT NULL, version INTEGER NOT NULL, updated_at TEXT NOT NULL)").execute(&covers).await.unwrap();
        sqlx::query(
            "INSERT INTO work(id,created_at,updated_at) VALUES ('w_publication',now(),now())",
        )
        .execute(&main)
        .await
        .unwrap();
        (main.foreground(), covers)
    }

    #[tokio::test]
    async fn delayed_cover_misses_recheck_the_current_asset_and_work_anchor() {
        let (main, covers) = publication_pools().await;
        sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_key,created_at,last_seen) VALUES ('rest-anchor','w_publication','mangadex','md-old',NOW(),NOW())").execute(&main).await.unwrap();
        sqlx::query("UPDATE work SET cover_file_name='old.jpg' WHERE id='w_publication'")
            .execute(&main)
            .await
            .unwrap();
        let mut first = main.begin().await.unwrap();
        crate::locks::lock_works(&mut first, &["w_publication"])
            .await
            .unwrap();
        sqlx::query("UPDATE work SET cover_file_name='new.jpg' WHERE id='w_publication'")
            .execute(&mut *first)
            .await
            .unwrap();
        let writer = main.clone();
        let cache = covers.clone();
        let second = tokio::spawn(async move {
            put_work_cover_for_missing_anchor(
                &writer,
                &cache,
                "w_publication",
                b"old-fetch",
                "md-old",
                "old.jpg",
            )
            .await
        });
        wait_for_writer_lock(&main).await;
        first.commit().await.unwrap();
        assert!(!second.await.unwrap().unwrap());
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM work_cover_blob WHERE work_id='w_publication')",
        )
        .fetch_one(&covers)
        .await
        .unwrap();
        assert!(!exists);
        let mut first = main.begin().await.unwrap();
        let _asset = lock_asset(&mut first, "s:7").await.unwrap();
        put_suwayomi_cover_on(&covers, 7, b"fresh-source")
            .await
            .unwrap();
        let writer = main.clone();
        let cache = covers.clone();
        let second = tokio::spawn(async move {
            publish_suwayomi_cover_if_missing(&writer, &cache, 7, b"old-fetch").await
        });
        wait_for_writer_lock(&main).await;
        first.commit().await.unwrap();
        drop(_asset);
        assert!(!second.await.unwrap().unwrap());
        assert_eq!(
            get_suwayomi_cover(&covers, 7).await.unwrap(),
            b"fresh-source"
        );
    }

    #[tokio::test]
    async fn overlapping_cover_backoffs_recompute_all_three_ladders_under_the_guard() {
        for kind in 0..3 {
            let (main, _) = publication_pools().await;
            insert_series(&main, 7, None).await;
            let points = crate::locks::pause_points::PausePoints::default();
            let armed = points.arm("backoff-locked");
            let (results, mut received) = tokio::sync::mpsc::unbounded_channel();
            let writer = main.clone();
            let first_results = results.clone();
            let operation = |writer: Database| async move {
                match kind {
                    0 => {
                        record_cover_issue_backoff(
                            &writer,
                            "w_publication",
                            CoverFailure::Transient("http_502"),
                            "failed",
                        )
                        .await
                    }
                    1 => {
                        record_suwayomi_cover_issue(
                            &writer,
                            7,
                            Some("1"),
                            Some("2026-01-01T00:00:00Z"),
                            CoverFailure::Transient("http_502"),
                            "failed",
                        )
                        .await
                    }
                    _ => {
                        record_source_cover_backoff(
                            &writer,
                            "1",
                            CoverFailure::Transient("http_502"),
                            3,
                            "failed",
                        )
                        .await
                    }
                }
            };
            let first = tokio::spawn(COVER_BACKOFF_RESULTS.scope(
                first_results,
                COVER_BACKOFF_PAUSE.scope(points, operation(writer)),
            ));
            tokio::time::timeout(std::time::Duration::from_secs(30), armed.reached())
                .await
                .unwrap();
            let second =
                tokio::spawn(COVER_BACKOFF_RESULTS.scope(results, operation(main.clone())));
            wait_for_writer_lock(&main).await;
            armed.release();
            first.await.unwrap();
            second.await.unwrap();
            received.recv().await.unwrap().unwrap();
            received.recv().await.unwrap().unwrap();
            let sql = match kind {
                0 => "SELECT attempts, EXTRACT(EPOCH FROM (retry_after-last_seen))::bigint FROM work_cover_issue WHERE work_id='w_publication'",
                1 => "SELECT attempts, EXTRACT(EPOCH FROM (retry_after-last_seen))::bigint FROM suwayomi_cover_issue WHERE manga_id=7",
                _ => "SELECT failures, EXTRACT(EPOCH FROM (retry_after-last_seen))::bigint FROM cover_source_backoff WHERE source_id='1'",
            };
            let (count, seconds): (i64, i64) = sqlx::query_as(sql).fetch_one(&main).await.unwrap();
            assert_eq!(count, 2);
            assert_eq!(
                seconds,
                if kind == 2 {
                    SOURCE_BACKOFF_BASE_SECS * 2
                } else {
                    TRANSIENT_BACKOFF_BASE_SECS * 2
                }
            );
        }
    }
    #[tokio::test]
    async fn two_foreground_cover_saves_publish_matching_monotonic_versions() {
        let (main, covers) = publication_pools().await;
        sqlx::query("INSERT INTO work_cover_blob VALUES ('w_publication', x'00', 4102444800000, '2026-01-01')").execute(&covers).await.unwrap();
        let points = crate::locks::pause_points::PausePoints::default();
        let armed = points.arm("cover-locked");
        let writer = main.clone();
        let cache = covers.clone();
        let first = tokio::spawn(COVER_PUBLICATION_PAUSE.scope(points, async move {
            put_work_cover(&writer, &cache, "w_publication", b"first").await
        }));
        tokio::time::timeout(std::time::Duration::from_secs(30), armed.reached())
            .await
            .unwrap();
        let writer = main.clone();
        let cache = covers.clone();
        let second = tokio::spawn(async move {
            put_work_cover(&writer, &cache, "w_publication", b"second").await
        });
        wait_for_writer_lock(&main).await;
        armed.release();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        let (bytes, version): (Vec<u8>, i64) = sqlx::query_as(
            "SELECT webp,version FROM work_cover_blob WHERE work_id='w_publication'",
        )
        .fetch_one(&covers)
        .await
        .unwrap();
        let pointer: i64 =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id='w_publication'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert_eq!(bytes, b"second");
        assert_eq!(version, 4102444800002);
        assert_eq!(pointer, version);
    }

    #[tokio::test]
    async fn a_delayed_derivative_cannot_publish_after_master_replacement() {
        let (main, covers) = publication_pools().await;
        put_work_cover(&main, &covers, "w_publication", b"old")
            .await
            .unwrap();
        let points = crate::locks::pause_points::PausePoints::default();
        let armed = points.arm("cover-locked");
        let writer = main.clone();
        let cache = covers.clone();
        let first = tokio::spawn(COVER_PUBLICATION_PAUSE.scope(points, async move {
            put_work_cover(&writer, &cache, "w_publication", b"new").await
        }));
        tokio::time::timeout(std::time::Duration::from_secs(30), armed.reached())
            .await
            .unwrap();
        let writer = main.clone();
        let cache = covers.clone();
        let second = tokio::spawn(async move {
            put_current_variant(
                &writer,
                &cache,
                "w_publication",
                CARD_VARIANT_WIDTH,
                b"old",
                b"old-small",
            )
            .await
        });
        wait_for_writer_lock(&main).await;
        armed.release();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert!(
            get_cover_variant(&covers, "w_publication", CARD_VARIANT_WIDTH)
                .await
                .is_none()
        );
    }

    /// The conditional master writers publish only over the master they captured:
    /// a lazy miss expects no master, recompression expects the bytes it re-encoded.
    #[tokio::test]
    async fn a_stale_fetch_or_recompression_cannot_replace_a_newer_master() {
        let (main, covers) = publication_pools().await;
        assert!(
            put_work_cover_if_current(&main, &covers, "w_publication", b"old", None)
                .await
                .unwrap(),
            "a miss with no master yet publishes"
        );
        put_work_cover(&main, &covers, "w_publication", b"new")
            .await
            .unwrap();
        let published = || async {
            let blob: (Vec<u8>, i64) = sqlx::query_as(
                "SELECT webp,version FROM work_cover_blob WHERE work_id='w_publication'",
            )
            .fetch_one(&covers)
            .await
            .unwrap();
            let pointer: i64 = sqlx::query_scalar(
                "SELECT cover_cached_version FROM work WHERE id='w_publication'",
            )
            .fetch_one(&main)
            .await
            .unwrap();
            assert_eq!(pointer, blob.1, "pointer and blob version differ");
            blob
        };
        let (_, version) = published().await;
        assert!(
            !put_work_cover_if_current(&main, &covers, "w_publication", b"stale-fetch", None)
                .await
                .unwrap()
        );
        assert!(!put_work_cover_if_current(
            &main,
            &covers,
            "w_publication",
            b"stale-recompression",
            Some(b"old")
        )
        .await
        .unwrap());
        assert_eq!(published().await, (b"new".to_vec(), version));
        assert!(put_work_cover_if_current(
            &main,
            &covers,
            "w_publication",
            b"recompressed",
            Some(b"new")
        )
        .await
        .unwrap());
        let (bytes, newer) = published().await;
        assert_eq!(bytes, b"recompressed");
        assert!(newer > version);
    }
    #[tokio::test]
    async fn cancelled_master_cover_keeps_capacity_until_decoder_exits() {
        let limits = MasterCoverLimits::new(1, 1);
        let work = limits.acquire().await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(async move {
            work.process(move || {
                let _ = started_tx.send(());
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test releases the blocking decoder");
            })
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("decoder starts")
            .expect("start signal");
        waiter.abort();
        assert!(waiter
            .await
            .expect_err("HTTP waiter cancelled")
            .is_cancelled());

        // A timed-out HTTP request cannot admit another source image or decoder
        // while its non-cancellable blocking worker still owns the image.
        assert!(limits.sources.clone().try_acquire_owned().is_err());
        assert!(limits.decoders.clone().try_acquire_owned().is_err());
        release_tx.send(()).expect("release decoder");
        let next = tokio::time::timeout(Duration::from_secs(5), limits.acquire())
            .await
            .expect("capacity returns when the real decoder exits");
        next.process(|| ()).await.expect("next decoder runs");
        assert_eq!(limits.sources.available_permits(), 1);
        assert_eq!(limits.decoders.available_permits(), 1);
    }

    #[tokio::test]
    async fn master_cover_pipeline_bounds_source_buffers_and_decoder_workers() {
        use std::sync::atomic::AtomicUsize;

        let limits = Arc::new(MasterCoverLimits::new(3, 2));
        let live_sources = Arc::new(AtomicUsize::new(0));
        let peak_sources = Arc::new(AtomicUsize::new(0));
        let live_decoders = Arc::new(AtomicUsize::new(0));
        let peak_decoders = Arc::new(AtomicUsize::new(0));
        let mut requests = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let limits = limits.clone();
            let live_sources = live_sources.clone();
            let peak_sources = peak_sources.clone();
            let live_decoders = live_decoders.clone();
            let peak_decoders = peak_decoders.clone();
            requests.spawn(async move {
                // Live routes and background work acquire before downloading,
                // so each fake fetch below represents a retained source buffer.
                let work = limits.acquire().await;
                let sources = live_sources.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                peak_sources.fetch_max(sources, AtomicOrdering::SeqCst);
                tokio::task::yield_now().await;
                work.process(move || {
                    let decoders = live_decoders.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                    peak_decoders.fetch_max(decoders, AtomicOrdering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                    live_decoders.fetch_sub(1, AtomicOrdering::SeqCst);
                    live_sources.fetch_sub(1, AtomicOrdering::SeqCst);
                })
                .await
                .expect("decoder task completes");
            });
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(request) = requests.join_next().await {
                request.expect("request completes");
            }
        })
        .await
        .expect("bounded pipeline drains");
        assert_eq!(peak_sources.load(AtomicOrdering::SeqCst), 3);
        assert!((1..=2).contains(&peak_decoders.load(AtomicOrdering::SeqCst)));
        assert_eq!(live_sources.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(live_decoders.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(limits.sources.available_permits(), 3);
        assert_eq!(limits.decoders.available_permits(), 2);
    }

    #[test]
    fn the_card_variant_width_is_actually_servable() {
        // `VARIANT_WIDTHS` is built FROM `CARD_VARIANT_WIDTH`, so containment is structural
        // rather than asserted here. What is still worth pinning is the round trip: the
        // reader sends this width as a decimal string, and `variant_width` has to parse
        // that exact string back. If it did not, every card would silently fall back to
        // the master and the srcset would stop saving anything, with no error to notice
        // it by.
        assert_eq!(
            variant_width(Some(&CARD_VARIANT_WIDTH.to_string())),
            Some(CARD_VARIANT_WIDTH)
        );
    }

    #[test]
    fn variant_width_serves_the_master_rather_than_erroring() {
        // Every rejected shape must mean "serve the master". A hand-edited or stale URL
        // has to render a cover, and an open parameter would let one page mint unbounded
        // derivatives into the covers DB.
        assert_eq!(variant_width(None), None, "absent");
        assert_eq!(variant_width(Some("")), None, "empty");
        assert_eq!(variant_width(Some("abc")), None, "not a number");
        assert_eq!(variant_width(Some("-8")), None, "negative");
        assert_eq!(variant_width(Some("193")), None, "off the allowlist");
        assert_eq!(variant_width(Some("99999")), None, "absurd");
    }

    #[test]
    fn downscale_variant_narrows_the_master_and_keeps_aspect_ratio() {
        let master = process_cover(&noisy_png(600, 900)).expect("master encodes");
        let small = downscale_variant(&master, CARD_VARIANT_WIDTH)
            .expect("variant encodes")
            .expect("a 512px master is wider than a card, so a derivative is produced");

        let dims = |b: &[u8]| {
            let i = image::load_from_memory(b).expect("decodes");
            i.dimensions()
        };
        let (mw, mh) = dims(&master);
        let (vw, vh) = dims(&small);
        assert_eq!(vw, CARD_VARIANT_WIDTH, "variant is exactly the asked width");
        // 2:3 in, 2:3 out — off-by-one from rounding is fine, a squashed cover is not.
        let expected_h = (mh as f32 * (vw as f32 / mw as f32)).round() as u32;
        assert!(
            vh.abs_diff(expected_h) <= 1,
            "aspect ratio drifted: {vw}x{vh} from {mw}x{mh}"
        );
        assert!(
            small.len() < master.len(),
            "a card variant that is not smaller than its master ({} vs {} bytes) buys nothing",
            small.len(),
            master.len()
        );
    }

    #[test]
    fn the_hidpi_rung_is_wider_but_cheaper_per_pixel() {
        // Both rungs must come out at exactly the asked width, and the 2x rung must cost
        // LESS per pixel than the 1x one — that is the whole point of
        // `HIDPI_VARIANT_QUALITY`. Without the per-rung quality the 320 rung would encode
        // at 82 and give back only the pixel-count saving.
        let master = process_cover(&noisy_png(1200, 1800)).expect("master encodes");
        let one_x = downscale_variant(&master, CARD_VARIANT_WIDTH)
            .expect("1x encodes")
            .expect("wider than a card");
        let two_x = downscale_variant(&master, CARD_VARIANT_WIDTH_2X)
            .expect("2x encodes")
            .expect("wider than a card");

        let dims = |b: &[u8]| {
            let i = image::load_from_memory(b).expect("decodes");
            (i.width(), i.height())
        };
        assert_eq!(dims(&one_x).0, CARD_VARIANT_WIDTH);
        assert_eq!(dims(&two_x).0, CARD_VARIANT_WIDTH_2X);

        let bpp = |bytes: &[u8], (w, h): (u32, u32)| bytes.len() as f64 / (w as f64 * h as f64);
        assert!(
            bpp(&two_x, dims(&two_x)) < bpp(&one_x, dims(&one_x)),
            "the 2x rung must encode at a lower quality than the 1x rung"
        );
    }

    #[test]
    fn downscale_variant_never_upscales() {
        // A master already narrower than the card width yields NO derivative: enlarging it
        // would cost bytes to add no detail, and storing a byte-identical copy would waste
        // a `cover_variant` row per cover (masters are 240–364 px wide, so at the 320 rung
        // that is most of the catalogue).
        let master = process_cover(&noisy_png(120, 180)).expect("master encodes");
        let out = downscale_variant(&master, CARD_VARIANT_WIDTH).expect("passes through");
        assert_eq!(out, None, "a small master must not mint a derivative");
    }

    #[test]
    fn both_card_variant_widths_are_actually_servable() {
        // Same round trip as `the_card_variant_width_is_actually_servable`, for the 2x rung.
        // A drift here is silent: the width falls off the allowlist, every 2x viewer is
        // served the master again, and the only symptom is the byte count.
        assert_eq!(
            variant_width(Some(&CARD_VARIANT_WIDTH_2X.to_string())),
            Some(CARD_VARIANT_WIDTH_2X)
        );
    }

    #[test]
    fn the_lossy_header_test_separates_the_two_cover_generations() {
        // What `cover_recompress` selects on. Lossy in, lossy out; the pure-Rust lossless
        // encoder (what every pre-switch master was written with) must read as NOT lossy or
        // the recompress pass would skip exactly the rows it exists for.
        let lossy = process_cover(&noisy_png(600, 900)).expect("master encodes");
        assert!(webp_is_lossy(&lossy), "process_cover output is VP8 lossy");

        let rgba = image::load_from_memory(&lossy).expect("decodes").to_rgba8();
        let lossless = crate::avatar::encode_lossless(&rgba).expect("lossless encodes");
        assert!(
            !webp_is_lossy(&lossless),
            "a VP8L master must be selected for recompression"
        );
        assert!(!webp_is_lossy(b""), "empty");
        assert!(!webp_is_lossy(b"not an image at all"), "garbage");
    }

    #[test]
    fn the_edge_ladder_bottoms_out_inside_the_byte_budget() {
        // The pathological case the 320/256 rungs were added for: a cover so noisy that
        // the 384px encode still misses the budget. Before those rungs it shipped OVER
        // MAX_COVER_BYTES through the "smallest produced" fallback.
        let encoded = process_cover(&noisy_png(1400, 2100)).expect("encodes");
        assert!(
            encoded.len() <= MAX_COVER_BYTES,
            "worst-case cover shipped {} bytes, over the {} byte budget",
            encoded.len(),
            MAX_COVER_BYTES
        );
    }

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
        use crate::database::TestPoolOptions;
        let main = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(main.write())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('w_iss', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&main)
        .await
        .unwrap();

        record_cover_issue(&main, "w_iss", "unsupported", "boom").await;
        record_cover_issue(&main, "w_iss", "too_large", "again").await;
        let (reason, attempts): (String, i64) = crate::db_types::query_tuple(
            "SELECT reason, attempts FROM work_cover_issue WHERE work_id = $1",
        )
        .bind("w_iss")
        .fetch_one(&main)
        .await
        .unwrap();
        assert_eq!(attempts, 2, "second record increments attempts");
        assert_eq!(reason, "too_large", "reason refreshed to the latest");

        clear_cover_issue(&main, "w_iss").await;
        let n: i64 = crate::db_types::query_scalar(
            "SELECT COUNT(*) FROM work_cover_issue WHERE work_id = $1",
        )
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
    async fn re_saving_a_master_drops_its_stale_derivatives() {
        use crate::database::TestPoolOptions;
        let main = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(main.write())
            .await
            .unwrap();
        let covers = crate::sqlite_database::SqliteTestPoolOptions::new()
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
            "CREATE TABLE suwayomi_cover_blob (manga_id INTEGER PRIMARY KEY, \
             webp BLOB NOT NULL, updated_at TEXT NOT NULL)",
        )
        .execute(&covers)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE cover_variant (asset_key TEXT NOT NULL, width INTEGER NOT NULL, \
             webp BLOB NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (asset_key, width))",
        )
        .execute(&covers)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO work (id, created_at, updated_at) \
             VALUES ('w_a', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&main)
        .await
        .unwrap();

        let old_art = process_cover(&noisy_png(400, 600)).unwrap();
        let variant = downscale_variant(&old_art, CARD_VARIANT_WIDTH)
            .unwrap()
            .expect("a 400x600 master is wider than a card");
        put_cover_variant(&covers, "w_a", CARD_VARIANT_WIDTH, &variant)
            .await
            .unwrap();
        put_cover_variant(
            &covers,
            &suwayomi_variant_key(7),
            CARD_VARIANT_WIDTH,
            &variant,
        )
        .await
        .unwrap();
        assert!(
            get_cover_variant(&covers, "w_a", CARD_VARIANT_WIDTH)
                .await
                .is_some(),
            "precondition: the derivative is stored"
        );

        // New art for the SAME work. Without invalidation the derivative below would keep
        // serving the old cover to every 1x viewer — and for the Suwayomi family, whose
        // public URL carries no `?v=` token, it would do so for ever.
        let new_art = process_cover(&noisy_png(500, 750)).unwrap();
        put_work_cover(&main, &covers, "w_a", &new_art)
            .await
            .unwrap();
        assert!(
            get_cover_variant(&covers, "w_a", CARD_VARIANT_WIDTH)
                .await
                .is_none(),
            "a re-saved work master must drop its derivatives"
        );

        put_suwayomi_cover(&covers, 7, &new_art).await.unwrap();
        assert!(
            get_cover_variant(&covers, &suwayomi_variant_key(7), CARD_VARIANT_WIDTH)
                .await
                .is_none(),
            "a re-saved Suwayomi master must drop its derivatives"
        );
    }

    #[tokio::test]
    async fn put_work_cover_stores_blob_and_flips_version() {
        use crate::database::TestPoolOptions;
        // Main (replicated) DB: `work` + the `cover_cached_version` pointer. After
        // migration 0040 it has NO `work_cover_blob` table.
        let main = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(main.write())
            .await
            .unwrap();
        // Covers (un-replicated) DB: just the blob table, as `db::init_covers` builds.
        let covers = crate::sqlite_database::SqliteTestPoolOptions::new()
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
            "CREATE TABLE cover_variant (asset_key TEXT NOT NULL, width INTEGER NOT NULL, \
             webp BLOB NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (asset_key, width))",
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
        let ver0: Option<i64> = crate::db_types::query_scalar(
            "SELECT cover_cached_version FROM work WHERE id = 'w_test'",
        )
        .fetch_one(&main)
        .await
        .unwrap();
        assert!(ver0.is_none());

        sqlx::query("INSERT INTO feed_updates (work_id, title, latest_at, cover_url) VALUES ('w_test', 'Cover fixture', '2026-01-01', '/old.webp')")
            .execute(&main).await.unwrap();
        sqlx::query("INSERT INTO feed_series_updates (work_id, reader_id, title, released_at, cover_url) VALUES ('w_test', '7', 'Cover fixture', TIMESTAMPTZ '1970-01-01 00:00:00.001+00', '/old.webp')")
            .execute(&main).await.unwrap();
        sqlx::query("INSERT INTO browse_catalogue (work_id, reader_id, title, created_at, cover_url) VALUES ('w_test', '7', 'Cover fixture', '2026-01-01', '/old.webp')")
            .execute(&main).await.unwrap();

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
        let work_ver: Option<i64> = crate::db_types::query_scalar(
            "SELECT cover_cached_version FROM work WHERE id = 'w_test'",
        )
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
        for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
            let query = format!("SELECT cover_url FROM {table} WHERE work_id = 'w_test'");
            let url: String = crate::db_types::query_scalar(&query)
                .fetch_one(&main)
                .await
                .unwrap();
            assert_eq!(
                url,
                cover_path("w_test", blob_ver),
                "{table} publishes the new thumbnail version"
            );
            sqlx::query(&format!(
                "UPDATE {table} SET cover_url = '/old.webp' WHERE work_id = 'w_test'"
            ))
            .execute(&main)
            .await
            .unwrap();
        }
        // Existing repairs also need their stale materialized URLs corrected once.
        sqlx::raw_sql(
            &include_str!("../migrations/0109_cover_card_versions.sql")
                .replace("cover_url IS NOT (", "cover_url IS DISTINCT FROM ("),
        )
        .execute(&main)
        .await
        .unwrap();
        for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
            let url: String = crate::db_types::query_scalar(&format!(
                "SELECT cover_url FROM {table} WHERE work_id = 'w_test'"
            ))
            .fetch_one(&main)
            .await
            .unwrap();
            assert_eq!(
                url,
                cover_path("w_test", blob_ver),
                "migration repairs {table}"
            );
        }

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
        use crate::database::TestPoolOptions;
        let main = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(main.write())
            .await
            .unwrap();
        for id in ["w_ok", "w_broken"] {
            sqlx::query(
                "INSERT INTO work (id, cover_file_name, created_at, updated_at) \
                 VALUES ($1, 'c.jpg', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(id)
            .execute(&main)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
                 VALUES ($1, $2, 'mangadex', $3, '2026-01-01T00:00:00Z')",
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

    #[tokio::test]
    async fn repair_falls_back_after_bad_mangadex_and_unavailable_first_suwayomi() {
        let sources = vec![
            RepairCoverSource::MangaDex {
                manga_id: "md".into(),
                file_name: "bad.jpg".into(),
            },
            RepairCoverSource::Suwayomi {
                path: "/first".into(),
            },
            RepairCoverSource::Suwayomi {
                path: "/second".into(),
            },
        ];
        let good = noisy_png(300, 450);
        let mut visited = Vec::new();
        let webp = process_repair_sources(sources.clone(), |source| {
            visited.push(source.clone());
            std::future::ready(match source {
                RepairCoverSource::MangaDex { .. } => Some(b"not an image".to_vec()),
                RepairCoverSource::Suwayomi { path } if path == "/second" => Some(good.clone()),
                _ => None,
            })
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(visited, sources);
        assert_eq!(
            image::load_from_memory(&webp).unwrap().dimensions(),
            (300, 450)
        );
    }

    #[tokio::test]
    async fn repair_uses_suwayomi_without_mangadex_or_cached_metadata_and_keeps_old_cover_on_failure(
    ) {
        let (main, covers) = warm_test_pools().await;
        sqlx::query("CREATE TABLE work_cover_blob (work_id TEXT PRIMARY KEY, webp BLOB NOT NULL, version INTEGER NOT NULL, updated_at TEXT NOT NULL)")
            .execute(&covers).await.unwrap();
        sqlx::query("INSERT INTO work (id, created_at, updated_at) VALUES ('w_repair', '2026-01-01', '2026-01-01')")
            .execute(&main).await.unwrap();
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_key, created_at) VALUES ('ss_repair', 'w_repair', 'suwayomi', '7', '2026-01-01')")
            .execute(&main).await.unwrap();
        assert_eq!(
            repair_cover_sources(&main, "w_repair").await.unwrap(),
            vec![RepairCoverSource::Suwayomi {
                path: "/api/v1/manga/7/thumbnail".into()
            }]
        );
        record_cover_issue(&main, "w_repair", "unsupported", "old failure").await;
        let origin = crate::suwayomi::testsrv::spawn(
            crate::suwayomi::testsrv::jpeg(300, 450),
            Duration::ZERO,
        )
        .await;
        let suwayomi = crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, None);
        let mangadex = MangaDexClient::new("cover-repair-test", 5.0, 40.0);
        assert!(
            retry_one_cover(&main, &covers, &mangadex, &suwayomi, "w_repair")
                .await
                .unwrap()
        );
        let before: (Vec<u8>, i64) =
            sqlx::query_as("SELECT webp, version FROM work_cover_blob WHERE work_id = 'w_repair'")
                .fetch_one(&covers)
                .await
                .unwrap();
        assert_eq!(
            image::load_from_memory(&before.0).unwrap().dimensions(),
            (300, 450)
        );
        let issues: i64 = crate::db_types::query_scalar(
            "SELECT count(*) FROM work_cover_issue WHERE work_id = 'w_repair'",
        )
        .fetch_one(&main)
        .await
        .unwrap();
        assert_eq!(issues, 0);
        assert_eq!(origin.hits.load(Ordering::SeqCst), 1);
        let bad_origin =
            crate::suwayomi::testsrv::spawn(b"not an image".to_vec(), Duration::ZERO).await;
        let bad_client =
            crate::suwayomi::SuwayomiClient::new(bad_origin.base_url.clone(), None, None);
        assert!(
            retry_one_cover(&main, &covers, &mangadex, &bad_client, "w_repair")
                .await
                .is_err()
        );
        let after: (Vec<u8>, i64) =
            sqlx::query_as("SELECT webp, version FROM work_cover_blob WHERE work_id = 'w_repair'")
                .fetch_one(&covers)
                .await
                .unwrap();
        assert_eq!(
            after, before,
            "failed sources must not replace the stored cover"
        );
    }

    #[tokio::test]
    async fn with_mangadex_off_a_cover_repair_uses_the_suwayomi_sources() {
        let (main, covers) = warm_test_pools().await;
        sqlx::query("CREATE TABLE work_cover_blob (work_id TEXT PRIMARY KEY, webp BLOB NOT NULL, version INTEGER NOT NULL, updated_at TEXT NOT NULL)")
            .execute(&covers).await.unwrap();
        sqlx::query(r#"INSERT INTO work (id, cover_file_name, created_at, updated_at) VALUES ('w_both', 'c.jpg', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')"#)
            .execute(&main).await.unwrap();
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_key, created_at) VALUES ('a', 'w_both', 'mangadex', 'md-uuid', '2026-01-01'), ('b', 'w_both', 'suwayomi', '9', '2026-01-02')")
            .execute(&main).await.unwrap();
        // Two counting origins: the MangaDex one (tried FIRST when it is on) must see
        // nothing while it is off; the Suwayomi one supplies the cover.
        let suw_origin = crate::suwayomi::testsrv::spawn(
            crate::suwayomi::testsrv::jpeg(300, 450),
            Duration::ZERO,
        )
        .await;
        let md_origin = crate::suwayomi::testsrv::spawn(
            crate::suwayomi::testsrv::jpeg(300, 450),
            Duration::ZERO,
        )
        .await;
        let suwayomi =
            crate::suwayomi::SuwayomiClient::new(suw_origin.base_url.clone(), None, None);
        let mangadex = MangaDexClient::new("cover-repair-off", 5.0, 40.0)
            .with_bases(&md_origin.base_url, &md_origin.base_url);
        assert!(
            retry_one_cover(&main, &covers, &mangadex, &suwayomi, "w_both")
                .await
                .unwrap()
        );
        assert_eq!(md_origin.hits.load(Ordering::SeqCst), 0);
        assert_eq!(suw_origin.hits.load(Ordering::SeqCst), 1);
        // On, MangaDex is the preferred source again.
        mangadex.set_enabled(true);
        assert!(
            retry_one_cover(&main, &covers, &mangadex, &suwayomi, "w_both")
                .await
                .unwrap()
        );
        assert_eq!(md_origin.hits.load(Ordering::SeqCst), 1);
        assert_eq!(suw_origin.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn with_mangadex_off_the_crawl_skips_only_its_mangadex_pass() {
        let (main, covers) = warm_test_pools().await;
        sqlx::query("CREATE TABLE work_cover_blob (work_id TEXT PRIMARY KEY, webp BLOB NOT NULL, version INTEGER NOT NULL, updated_at TEXT NOT NULL)")
            .execute(&covers).await.unwrap();
        // One MangaDex-anchored work and one Suwayomi-anchored work, both uncached.
        sqlx::query("INSERT INTO work (id, cover_file_name, created_at, updated_at) VALUES ('w_md', 'c.jpg', '2026-01-01', '2026-01-01'), ('w_suw', NULL, '2026-01-01', '2026-01-01')")
            .execute(&main).await.unwrap();
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_key, created_at) VALUES ('ss_md', 'w_md', 'mangadex', 'md-uuid', '2026-01-01'), ('ss_suw', 'w_suw', 'suwayomi', '7', '2026-01-01')")
            .execute(&main).await.unwrap();
        insert_series(&main, 7, Some(&thumb_path(7))).await;
        let origin = crate::suwayomi::testsrv::spawn(
            crate::suwayomi::testsrv::jpeg(300, 450),
            Duration::ZERO,
        )
        .await;
        let suwayomi = crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, None);
        // Off (the default), and pointed at a dead port should the gate ever leak.
        let mangadex = MangaDexClient::new("cover-split-test", 5.0, 40.0)
            .with_bases("http://127.0.0.1:1", "http://127.0.0.1:1");
        assert_eq!(pending_cover_count(&main).await.unwrap(), 2);

        crawl_uncached_covers(&main, &covers, &mangadex, &suwayomi, Some(10)).await;

        let cached: Vec<String> = sqlx::query_scalar("SELECT work_id FROM work_cover_blob")
            .fetch_all(&covers)
            .await
            .unwrap();
        assert_eq!(
            cached,
            ["w_suw"],
            "Suwayomi covers keep flowing with MangaDex off"
        );
        // The MangaDex work was not attempted: no failure marker that would bury it
        // behind a backoff once MangaDex is turned back on.
        let issues: i64 = crate::db_types::query_scalar("SELECT COUNT(*) FROM work_cover_issue")
            .fetch_one(&main)
            .await
            .unwrap();
        assert_eq!(issues, 0);
        assert_eq!(pending_cover_count(&main).await.unwrap(), 1);
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
    async fn warm_test_pools() -> (Database, SqliteDatabase) {
        use crate::database::TestPoolOptions;
        let main = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(main.write())
            .await
            .unwrap();
        let covers = crate::sqlite_database::SqliteTestPoolOptions::new()
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
        // Mirrors `db::init_covers`. Without it, `put_suwayomi_cover`'s invalidation hits
        // a missing table — and because that failure is deliberately swallowed, every
        // test through this pool would keep passing while proving less than it looks.
        sqlx::query(
            "CREATE TABLE cover_variant (asset_key TEXT NOT NULL, width INTEGER NOT NULL, \
             webp BLOB NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (asset_key, width))",
        )
        .execute(&covers)
        .await
        .unwrap();
        (main, covers)
    }

    async fn insert_series(main: &Database, id: i64, thumb: Option<&str>) {
        insert_series_from(main, id, thumb, "1", None).await;
    }

    /// Same, with the two columns the backoff keys on: the source, and the
    /// `last_fetched_at` a marker is fingerprinted against.
    async fn insert_series_from(
        main: &Database,
        id: i64,
        thumb: Option<&str>,
        source_id: &str,
        fetched_at: Option<&str>,
    ) {
        sqlx::query(
            r#"INSERT INTO suwayomi_series (id, title, status, source_id, in_library, thumbnail_url, last_fetched_at, updated_at) VALUES ($1, $2, 'ONGOING', $3, TRUE, $4, $5, '2026-01-01T00:00:00Z')"#,
        )
        .bind(id)
        .bind(format!("s{id}"))
        .bind(source_id)
        .bind(thumb)
        .bind(fetched_at)
        .execute(main)
        .await
        .unwrap();
    }

    fn thumb_path(id: i64) -> String {
        format!("/api/v1/manga/{id}/thumbnail")
    }

    async fn issue_row(main: &Database, manga_id: i64) -> Option<(String, i64, i64, String)> {
        crate::db_types::query_tuple(
            "SELECT reason, permanent, attempts, retry_after FROM suwayomi_cover_issue \
             WHERE manga_id = $1",
        )
        .bind(manga_id)
        .fetch_optional(main)
        .await
        .unwrap()
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
        // Bottom reached. Since 0100 the wrap does NOT re-fetch immediately: all six now
        // carry a transient marker, so the tick costs zero requests — the rotation and
        // the backoff compose (the cursor still slides, there is simply nothing to do).
        let (_, failed) = warm_suwayomi_covers(&covers, &main, &client, 2, &cursor, &rx).await;
        assert_eq!(failed, 0, "backed-off ids are not re-fetched on the wrap");

        // Once the backoff elapses the wrap behaves as before: newest window first.
        sqlx::query("UPDATE suwayomi_cover_issue SET retry_after = '2020-01-01T00:00:00+00:00'")
            .execute(&main)
            .await
            .unwrap();
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

    // ---------------------------------------------------------------------------
    // Retry backoff (0100): the warmer must stop re-fetching covers that keep failing
    // ---------------------------------------------------------------------------

    /// The classification IS the mechanism: a `data:` thumbnail / 400 / 404 can never
    /// succeed unprompted, while a WAF 403, a 429 and a 5xx are expected to lapse.
    #[test]
    fn classify_fetch_error_splits_permanent_from_transient() {
        use CoverFailure::*;
        let cases: [(&str, CoverFailure); 9] = [
            // What Suwayomi actually answers for the 46 `data:` thumbnails.
            (
                "Suwayomi cover error 400 Bad Request: Expected URL scheme 'http' or 'https' but was 'data'",
                Permanent("data_uri"),
            ),
            ("Suwayomi cover error 400 Bad Request", Permanent("bad_request")),
            ("Suwayomi cover error 404 Not Found", Permanent("not_found")),
            ("response body exceeds cap (26000000)", Permanent("too_large")),
            // Philia's Cloudflare block — 28 covers, ~8k fetches/day.
            ("Suwayomi cover error 403 Forbidden", Transient("blocked")),
            ("Suwayomi cover error 429 Too Many Requests", Transient("rate_limited")),
            ("Suwayomi cover error 502 Bad Gateway", Transient("upstream_5xx")),
            ("truncated JPEG (no EOI marker)", Transient("truncated")),
            ("error sending request for url (http://x/api/v1/manga/404/thumbnail)", Transient("fetch")),
        ];
        for (msg, want) in cases {
            assert_eq!(
                classify_fetch_error(&anyhow!("{msg}")),
                want,
                "misclassified: {msg}"
            );
        }
    }

    /// Transient failures back off exponentially and STOP at a ceiling — a cover is
    /// slowed down, never abandoned. Permanent ones get one long (but finite) window.
    #[test]
    fn transient_backoff_is_exponential_with_a_ceiling() {
        let t = CoverFailure::Transient("blocked");
        assert_eq!(t.backoff_secs(1), TRANSIENT_BACKOFF_BASE_SECS);
        assert_eq!(t.backoff_secs(2), TRANSIENT_BACKOFF_BASE_SECS * 2);
        assert_eq!(t.backoff_secs(3), TRANSIENT_BACKOFF_BASE_SECS * 4);
        assert_eq!(
            t.backoff_secs(50),
            TRANSIENT_BACKOFF_MAX_SECS,
            "capped, not overflowing"
        );
        assert!(t.backoff_secs(50) <= TRANSIENT_BACKOFF_MAX_SECS);
        assert_eq!(
            CoverFailure::Permanent("data_uri").backoff_secs(1),
            PERMANENT_BACKOFF_SECS
        );
    }

    /// THE regression this exists for: `saved=0 failed=92` on every 5-minute tick. A
    /// deterministically-failing cover (Suwayomi's 400 for a `data:` thumbnail) must be
    /// fetched ONCE and then skipped on the next cycle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn permanently_failing_cover_is_not_retried_next_cycle() {
        let (main, covers) = warm_test_pools().await;
        insert_series(&main, 7, Some(&thumb_path(7))).await;
        let origin = crate::suwayomi::testsrv::spawn_error(
            400,
            "Expected URL scheme 'http' or 'https' but was 'data'",
        )
        .await;
        let client =
            crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, Some("1".into()));
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let cursor = AtomicI64::new(WARM_CURSOR_START);

        let (saved, failed) = warm_suwayomi_covers(&covers, &main, &client, 10, &cursor, &rx).await;
        assert_eq!((saved, failed), (0, 1));
        let (reason, permanent, attempts, _) = issue_row(&main, 7).await.expect("marker recorded");
        assert_eq!((reason.as_str(), permanent, attempts), ("data_uri", 1, 1));
        let after_first = origin.hits.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(after_first, 1);

        // Next cycle: the candidate is gone, so not one byte leaves the box.
        let (saved, failed) = warm_suwayomi_covers(&covers, &main, &client, 10, &cursor, &rx).await;
        assert_eq!(
            (saved, failed),
            (0, 0),
            "the failing cover is no longer a candidate"
        );
        assert_eq!(
            origin.hits.load(std::sync::atomic::Ordering::SeqCst),
            after_first,
            "a permanently-failing cover must not be re-fetched every tick"
        );
        assert_eq!(
            pending_suwayomi_cover_count(&main, &covers).await.unwrap(),
            0,
            "the gauge must agree with the warmer, or the drainer runs empty passes"
        );
        // …and the deferral is still VISIBLE, not silently counted as full coverage.
        assert_eq!(suwayomi_cover_backoff_stats(&main).await.unwrap(), (1, 0));
    }

    /// A transient failure is BACKED OFF, not abandoned: skipped while the window is
    /// open, retried (with a longer window) once it elapses.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn transient_failure_is_retried_once_its_backoff_elapses() {
        let (main, covers) = warm_test_pools().await;
        // Below SOURCE_BACKOFF_THRESHOLD, so this exercises the per-cover ladder only.
        insert_series(&main, 8, Some(&thumb_path(8))).await;
        let origin = crate::suwayomi::testsrv::spawn_error(503, "upstream is sulking").await;
        let client =
            crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, Some("1".into()));
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let cursor = AtomicI64::new(WARM_CURSOR_START);

        assert_eq!(
            warm_suwayomi_covers(&covers, &main, &client, 10, &cursor, &rx).await,
            (0, 1)
        );
        let (reason, permanent, attempts, _) = issue_row(&main, 8).await.expect("marker recorded");
        assert_eq!(
            (reason.as_str(), permanent, attempts),
            ("upstream_5xx", 0, 1)
        );
        assert_eq!(origin.hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Inside the window: skipped.
        assert_eq!(
            warm_suwayomi_covers(&covers, &main, &client, 10, &cursor, &rx).await,
            (0, 0)
        );
        assert_eq!(origin.hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Window elapsed (backdated, so the test doesn't sleep for 30 minutes).
        sqlx::query("UPDATE suwayomi_cover_issue SET retry_after = '2020-01-01T00:00:00+00:00'")
            .execute(&main)
            .await
            .unwrap();
        assert_eq!(
            warm_suwayomi_covers(&covers, &main, &client, 10, &cursor, &rx).await,
            (0, 1),
            "a transient failure must be retried after its backoff, not abandoned"
        );
        assert_eq!(origin.hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        let (_, _, attempts, _) = issue_row(&main, 8).await.unwrap();
        assert_eq!(attempts, 2, "the next window is longer than the last");
    }

    /// Self-healing #1: a fetch that works clears the marker outright.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_successful_fetch_clears_the_marker() {
        let (main, covers) = warm_test_pools().await;
        insert_series(&main, 9, Some(&thumb_path(9))).await;
        // A marker from an earlier failure whose window has already elapsed.
        record_suwayomi_cover_issue(
            &main,
            9,
            Some("1"),
            None,
            CoverFailure::Transient("blocked"),
            "403 last week",
        )
        .await;
        sqlx::query("UPDATE suwayomi_cover_issue SET retry_after = '2020-01-01T00:00:00+00:00'")
            .execute(&main)
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

        assert_eq!(
            warm_suwayomi_covers(&covers, &main, &client, 10, &cursor, &rx).await,
            (1, 0)
        );
        assert!(
            issue_row(&main, 9).await.is_none(),
            "success clears the marker"
        );
        assert!(get_suwayomi_cover(&covers, 9).await.is_some());
    }

    /// Self-healing #2: a PERMANENT marker is void the moment the extension re-fetches
    /// the manga — that is exactly when a `data:` thumbnail can become a real URL, and
    /// it must not wait out the 30-day window.
    #[tokio::test]
    async fn a_re_fetched_series_voids_even_a_permanent_marker() {
        let (main, _covers) = warm_test_pools().await;
        insert_series_from(
            &main,
            12,
            Some(&thumb_path(12)),
            "1",
            Some("2026-08-01T00:00:00Z"),
        )
        .await;
        record_suwayomi_cover_issue(
            &main,
            12,
            Some("1"),
            Some("2026-08-01T00:00:00Z"),
            CoverFailure::Permanent("data_uri"),
            "data: thumbnail",
        )
        .await;
        assert!(
            warm_candidates(&main, WARM_CURSOR_START)
                .await
                .unwrap()
                .is_empty(),
            "a fresh permanent marker suppresses the candidate"
        );

        sqlx::query(
            "UPDATE suwayomi_series SET last_fetched_at = '2026-08-12T00:00:00Z' WHERE id = 12",
        )
        .execute(&main)
        .await
        .unwrap();
        let again = warm_candidates(&main, WARM_CURSOR_START).await.unwrap();
        assert_eq!(again.len(), 1, "a re-fetched series is a candidate again");
        assert!(
            again[0].had_issue,
            "…and the stale marker is cleared on success"
        );

        // Re-recording against the NEW fetch stamp restarts the ladder rather than
        // inheriting the old attempt count.
        record_suwayomi_cover_issue(
            &main,
            12,
            Some("1"),
            Some("2026-08-12T00:00:00Z"),
            CoverFailure::Transient("blocked"),
            "403",
        )
        .await;
        assert_eq!(
            issue_row(&main, 12).await.unwrap().2,
            1,
            "attempts reset on new input"
        );
    }

    /// Per-SOURCE park. 28 individually-backed-off covers at one host still means 28
    /// requests per tick at a host that is banning us, so a source that fails wholesale
    /// is parked — including covers enrolled AFTER the park, which have no marker of
    /// their own. A healthy source in the same tick is untouched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_wholly_failing_source_is_parked_for_all_its_covers() {
        let (main, covers) = warm_test_pools().await;
        for id in 41..=43 {
            insert_series_from(&main, id, Some(&thumb_path(id)), "philia", None).await;
        }
        let origin = crate::suwayomi::testsrv::spawn_error(403, "blocked by WAF").await;
        let client =
            crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, Some("1".into()));
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let cursor = AtomicI64::new(WARM_CURSOR_START);

        assert_eq!(
            warm_suwayomi_covers(&covers, &main, &client, 10, &cursor, &rx).await,
            (0, 3)
        );
        let hits = origin.hits.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(hits, 3);
        let (_, parked) = suwayomi_cover_backoff_stats(&main).await.unwrap();
        assert_eq!(
            parked, 1,
            "the source itself is parked, not just its covers"
        );

        // A brand-new cover from the parked source: no marker of its own, still skipped.
        insert_series_from(&main, 44, Some(&thumb_path(44)), "philia", None).await;
        assert_eq!(
            warm_suwayomi_covers(&covers, &main, &client, 10, &cursor, &rx).await,
            (0, 0)
        );
        assert_eq!(
            origin.hits.load(std::sync::atomic::Ordering::SeqCst),
            hits,
            "a parked source must cost zero requests, per-cover markers or not"
        );

        // A different source is unaffected by the park.
        insert_series_from(&main, 45, Some(&thumb_path(45)), "healthy", None).await;
        assert_eq!(
            warm_suwayomi_covers(&covers, &main, &client, 10, &cursor, &rx).await,
            (0, 1)
        );
        assert_eq!(
            origin.hits.load(std::sync::atomic::Ordering::SeqCst),
            hits + 1
        );
    }

    /// A park is lifted by a single success — no operator action, no manual DELETE.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_source_park_lifts_as_soon_as_one_cover_succeeds() {
        let (main, covers) = warm_test_pools().await;
        for id in 51..=53 {
            insert_series_from(&main, id, Some(&thumb_path(id)), "philia", None).await;
        }
        let dead = crate::suwayomi::SuwayomiClient::new(
            "http://127.0.0.1:1".into(),
            None,
            Some("1".into()),
        );
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let cursor = AtomicI64::new(WARM_CURSOR_START);
        assert_eq!(
            warm_suwayomi_covers(&covers, &main, &dead, 10, &cursor, &rx).await,
            (0, 3)
        );
        assert_eq!(suwayomi_cover_backoff_stats(&main).await.unwrap().1, 1);

        // The block lifts (window elapsed) and the source answers again.
        sqlx::query("UPDATE cover_source_backoff SET retry_after = '2020-01-01T00:00:00+00:00'")
            .execute(&main)
            .await
            .unwrap();
        sqlx::query("UPDATE suwayomi_cover_issue SET retry_after = '2020-01-01T00:00:00+00:00'")
            .execute(&main)
            .await
            .unwrap();
        let origin = crate::suwayomi::testsrv::spawn(
            crate::suwayomi::testsrv::jpeg(200, 300),
            std::time::Duration::ZERO,
        )
        .await;
        let client =
            crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, Some("1".into()));
        assert_eq!(
            warm_suwayomi_covers(&covers, &main, &client, 10, &cursor, &rx).await,
            (3, 0)
        );
        assert_eq!(
            suwayomi_cover_backoff_stats(&main).await.unwrap(),
            (0, 0),
            "success clears both the per-cover markers and the source park"
        );
    }

    /// The work-side marker (0043) grows the same expiry, so a work is no longer excluded
    /// from the crawl for ever — a decoder/cap improvement is picked up by itself.
    #[tokio::test]
    async fn work_cover_issue_marker_expires_instead_of_excluding_for_ever() {
        use crate::database::TestPoolOptions;
        let main = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(main.write())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO work (id, cover_file_name, created_at, updated_at) \
             VALUES ('w_b', 'c.jpg', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .execute(&main)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('ss_b', 'w_b', 'mangadex', 'md-b', '2026-01-01T00:00:00Z')",
        )
        .execute(&main)
        .await
        .unwrap();

        record_cover_issue_backoff(&main, "w_b", CoverFailure::Transient("blocked"), "403").await;
        assert_eq!(pending_cover_count(&main).await.unwrap(), 0, "backed off");

        sqlx::query("UPDATE work_cover_issue SET retry_after = '2020-01-01T00:00:00+00:00'")
            .execute(&main)
            .await
            .unwrap();
        assert_eq!(
            pending_cover_count(&main).await.unwrap(),
            1,
            "an elapsed backoff makes the work selectable again"
        );
    }
}
