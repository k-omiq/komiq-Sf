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

#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::cover_store::{self, CoverStore};
use crate::database::SqliteDatabase;
use crate::db::Database;
use crate::source_budget::{SourceBudgets, SourceUse};
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

/// A stored derivative of `asset` at `width`, made from whichever master it names.
/// Fixtures only; serving uses [`variant_of_master`].
#[cfg(test)]
pub async fn get_cover_variant(
    main: &Database,
    covers: &CoverStore,
    asset_key: &str,
    width: u32,
) -> Option<Vec<u8>> {
    let row = cover_store::row(main, asset_key, cover_store::Kind::Width(width))
        .await
        .ok()
        .flatten()?;
    covers.read_object(&row.sha).await.ok().flatten()
}

/// The asset key (lock string) for a Suwayomi source cover.
pub fn suwayomi_variant_key(manga_id: i64) -> String {
    format!("s:{manga_id}")
}

/// Distinguish a cold cover from a failed cover store. Public serving paths must
/// not turn a storage outage into a new source fetch.
pub async fn read_suwayomi_cover(
    main: &Database,
    covers: &CoverStore,
    manga_id: i64,
    width: Option<u32>,
) -> std::result::Result<Option<(cover_store::Master, Option<Vec<u8>>)>, cover_store::StoreError> {
    covers
        .read_master(main, &suwayomi_variant_key(manga_id), width)
        .await
}

/// Best-effort fixture lookup. Serving uses the error-preserving helper above.
#[cfg(test)]
pub async fn get_suwayomi_cover(
    main: &Database,
    covers: &CoverStore,
    manga_id: i64,
) -> Option<Vec<u8>> {
    read_suwayomi_cover(main, covers, manga_id, None)
        .await
        .ok()
        .flatten()
        .map(|(master, _)| master.bytes)
}

/// Store (or replace) a Suwayomi source cover with no generation or takedown check.
/// Test fixtures only: production paths publish through
/// [`publish_suwayomi_cover_in_generation`] or
/// [`publish_suwayomi_cover_if_missing_in_generation`].
#[cfg(test)]
pub async fn put_suwayomi_cover(
    main: &Database,
    covers: &CoverStore,
    manga_id: i64,
    webp: &[u8],
) -> Result<()> {
    let key = suwayomi_variant_key(manga_id);
    let mut staged = covers.stage(webp).await?;
    let mut tx = main.begin().await?;
    let _asset = lock_assets_and_files(&mut tx, &[&key], &[staged.sha]).await?;
    covers.place(&mut staged).await?;
    cover_store::put_master(&mut tx, &key, &staged, None).await?;
    tx.commit().await?;
    Ok(())
}

/// The public path for one version of a cached work cover. The `/covers/{file}`
/// route serves that version's bytes with immutable caching while it is current;
/// older versions redirect forward; newer versions are served without caching.
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
    let barred = crate::catalog::work_metadata::barred_sql(
        &crate::catalog::work_metadata::mapping_source_sql("ss"),
        "'cover'",
    );
    let row = crate::db_types::query_tuple::<(Option<String>, Option<String>)>(&format!(
        "SELECT ss.source_key, w.cover_file_name \
         FROM work w \
         JOIN source_series ss ON ss.work_id = w.id AND ss.source_type = 'mangadex' \
         WHERE w.id = $1 AND NOT {barred} \
         ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1"
    ))
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

/// The process-local guard of one cover asset. Every publication also holds the
/// asset's PostgreSQL `Cover` key, taken first (see [`lock_assets_and_files`]).
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

/// The publish locks: the `Cover` key of each asset and the `CoverFile` key of each
/// object a publication places, in ONE sorted advisory call (the protocol takes every
/// non-membership advisory key together, before work rows), then the in-process asset
/// guards in sorted order. GC takes the same `CoverFile` key before it unlinks, so a file
/// a publication renames or touches cannot be removed before that publication commits.
pub async fn lock_assets_and_files(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    assets: &[&str],
    files: &[cover_store::Sha],
) -> Result<Vec<tokio::sync::OwnedMutexGuard<()>>> {
    lock_assets_files_and_source(tx, assets, files, None).await
}

async fn lock_assets_files_and_source(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    assets: &[&str],
    files: &[cover_store::Sha],
    source: Option<&str>,
) -> Result<Vec<tokio::sync::OwnedMutexGuard<()>>> {
    let mut assets = assets.to_vec();
    assets.sort_unstable();
    assets.dedup();
    let mut keys: Vec<_> = assets
        .iter()
        .map(|asset| crate::locks::AdvisoryKey::new(crate::locks::KeyClass::Cover, &[*asset]))
        .collect();
    keys.extend(files.iter().map(cover_store::file_key));
    let shared: Vec<_> = source
        .into_iter()
        .map(|source| {
            crate::locks::AdvisoryKey::new(crate::locks::KeyClass::SourcePolicy, &[source])
        })
        .collect();
    crate::locks::advisory_with_shared(tx, &keys, &shared).await?;
    let mut guards = Vec::with_capacity(assets.len());
    for asset in assets {
        guards.push(asset_mutex(asset).await);
    }
    Ok(guards)
}

/// A stored derivative of `asset` at `width`, but only while `master` is still the
/// current master: both are checked in one index read, so a derivative already written
/// for a newer master is never returned for an older one.
pub async fn variant_of_master(
    main: &Database,
    covers: &CoverStore,
    asset: &str,
    width: u32,
    master: &cover_store::Master,
) -> Option<Vec<u8>> {
    covers.derivative_of(main, asset, width, master).await
}

/// [`variant_of_master`] for a master still read from the SQLite cache during the copy.
pub async fn legacy_variant_of_master(
    legacy: &SqliteDatabase,
    asset: &str,
    width: u32,
    master: &[u8],
) -> Option<Vec<u8>> {
    // A single statement pins one SQLite snapshot and returns only the small variant.
    // Compare the master inside SQLite instead of copying it back into Rust again.
    let result = if let Some(id) = asset.strip_prefix("s:") {
        let id = id.parse::<i64>().ok()?;
        sqlx::query_scalar(
            "SELECT v.webp FROM cover_variant v WHERE v.asset_key = $1 AND v.width = $2 \
             AND EXISTS (SELECT 1 FROM suwayomi_cover_blob WHERE manga_id = $3 AND webp = $4)",
        )
        .bind(asset)
        .bind(width)
        .bind(id)
        .bind(master)
        .fetch_optional(legacy)
        .await
    } else {
        sqlx::query_scalar(
            "SELECT v.webp FROM cover_variant v WHERE v.asset_key = $1 AND v.width = $2 \
             AND EXISTS (SELECT 1 FROM work_cover_blob WHERE work_id = $3 AND webp = $4)",
        )
        .bind(asset)
        .bind(width)
        .bind(asset)
        .bind(master)
        .fetch_optional(legacy)
        .await
    };
    result
        .inspect_err(crate::db::record_read_database_error)
        .ok()
        .flatten()
}

/// Store a derivative made from `master`, only while `master` is still the indexed
/// master. A master read from the SQLite cache is not indexed yet: its derivative is
/// served but not stored (the copy brings the SQLite derivatives along).
pub async fn put_current_variant(
    main: &Database,
    covers: &CoverStore,
    asset: &str,
    width: u32,
    master: &cover_store::Master,
    derivative: &[u8],
) -> Result<()> {
    if master.legacy {
        return Ok(());
    }
    let mut staged = covers.stage(derivative).await?;
    let mut tx = main.begin().await?;
    if asset.starts_with("s:") {
        crate::engine_generation::shared(&mut tx, None).await?;
    }
    let _asset = lock_assets_and_files(&mut tx, &[asset], &[staged.sha]).await?;
    let current = cover_store::row(&mut *tx, asset, cover_store::Kind::Master).await?;
    if current.is_some_and(|row| row.sha == master.sha) {
        covers.place(&mut staged).await?;
        cover_store::put_derivative(&mut tx, asset, width, &staged, &master.sha).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The master a publication finds under its locks.
enum CurrentMaster {
    None,
    Indexed(cover_store::ObjectRow),
    /// Only in the SQLite cache, which the copy has not reached yet.
    Legacy {
        bytes: Vec<u8>,
        version: Option<i64>,
    },
}

impl CurrentMaster {
    /// Whether this is the master a guarded publication expects. `None` expects no
    /// master; a row whose file is gone counts as no master.
    fn is(&self, expected: Option<&[u8]>, expected_sha: Option<&cover_store::Sha>) -> bool {
        match (self, expected) {
            (CurrentMaster::None, None) => true,
            (CurrentMaster::Indexed(row), Some(_)) => Some(&row.sha) == expected_sha,
            (CurrentMaster::Legacy { bytes, .. }, Some(expected)) => bytes.as_slice() == expected,
            _ => false,
        }
    }

    fn version(&self) -> Option<i64> {
        match self {
            CurrentMaster::None => None,
            CurrentMaster::Indexed(row) => row.version,
            CurrentMaster::Legacy { version, .. } => *version,
        }
    }
}

/// Read the current master of `asset` under its publish locks. An indexed row whose file
/// is missing is no master, so a guarded fill can replace it.
async fn current_master(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    covers: &CoverStore,
    asset: &str,
) -> Result<CurrentMaster> {
    if let Some(row) = cover_store::row(&mut **tx, asset, cover_store::Kind::Master).await? {
        if covers.object_exists(&row.sha).await? {
            return Ok(CurrentMaster::Indexed(row));
        }
        // A lost file of an uploaded cover is not a missing master: the upload row still
        // holds its bytes and the next read restores them. Treating it as missing would let
        // a source fill replace the upload and delete that backup.
        let uploaded: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM cover_upload WHERE work_id = $1 AND sha256 = $2)",
        )
        .bind(asset)
        .bind(row.sha.as_slice())
        .fetch_one(&mut **tx)
        .await?;
        return Ok(if uploaded {
            CurrentMaster::Indexed(row)
        } else {
            CurrentMaster::None
        });
    }
    let Some(legacy) = covers.legacy_for_read(asset) else {
        return Ok(CurrentMaster::None);
    };
    if !asset.starts_with("s:") && legacy_work_cover_revoked(&mut **tx, asset).await? {
        return Ok(CurrentMaster::None);
    }
    let stored: Option<(Vec<u8>, Option<i64>)> = if let Some(id) = asset.strip_prefix("s:") {
        sqlx::query_as("SELECT webp, NULL FROM suwayomi_cover_blob WHERE manga_id = $1")
            .bind(id.parse::<i64>()?)
            .fetch_optional(legacy)
            .await?
    } else {
        sqlx::query_as("SELECT webp, version FROM work_cover_blob WHERE work_id = $1")
            .bind(asset)
            .fetch_optional(legacy)
            .await?
    };
    Ok(match stored {
        Some((bytes, version)) => CurrentMaster::Legacy { bytes, version },
        None => CurrentMaster::None,
    })
}

#[cfg(test)]
pub async fn publish_suwayomi_cover(
    main: &Database,
    covers: &CoverStore,
    manga_id: i64,
    webp: &[u8],
) -> Result<()> {
    publish_suwayomi_cover_in_generation(main, covers, manga_id, webp, None).await
}

async fn publish_suwayomi_cover_in_generation(
    main: &Database,
    covers: &CoverStore,
    manga_id: i64,
    webp: &[u8],
    generation: Option<uuid::Uuid>,
) -> Result<()> {
    publish_suwayomi_master(main, covers, manga_id, webp, generation, false).await?;
    Ok(())
}

#[cfg(test)]
pub async fn publish_suwayomi_cover_if_missing(
    main: &Database,
    covers: &CoverStore,
    manga_id: i64,
    webp: &[u8],
) -> Result<bool> {
    publish_suwayomi_cover_if_missing_in_generation(main, covers, manga_id, webp, None).await
}

pub async fn publish_suwayomi_cover_if_missing_in_generation(
    main: &Database,
    covers: &CoverStore,
    manga_id: i64,
    webp: &[u8],
    generation: Option<uuid::Uuid>,
) -> Result<bool> {
    publish_suwayomi_master(main, covers, manga_id, webp, generation, true).await
}

/// The one publication of a numeric source master: under the shared generation lock
/// (checked against `generation` when given), its asset and file keys, refusing a
/// taken-down series; `only_if_missing` keeps a master that is already there.
async fn publish_suwayomi_master(
    main: &Database,
    covers: &CoverStore,
    manga_id: i64,
    webp: &[u8],
    generation: Option<uuid::Uuid>,
    only_if_missing: bool,
) -> Result<bool> {
    let key = suwayomi_variant_key(manga_id);
    let mut staged = covers.stage(webp).await?;
    crate::engine_generation::wait(main).await?;
    let mut tx = main.begin().await?;
    crate::engine_generation::shared(&mut tx, generation).await?;
    let _asset = lock_assets_and_files(&mut tx, &[&key], &[staged.sha]).await?;
    if crate::media_policy::engine_series_taken_down(&mut *tx, manga_id).await? {
        tx.rollback().await?;
        return Ok(false);
    }
    if only_if_missing
        && !matches!(
            current_master(&mut tx, covers, &key).await?,
            CurrentMaster::None
        )
    {
        tx.rollback().await?;
        return Ok(false);
    }
    covers.place(&mut staged).await?;
    cover_store::put_master(&mut tx, &key, &staged, None).await?;
    crate::engine_generation::commit_ready(main)?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
tokio::task_local! { pub(crate) static COVER_PUBLICATION_PAUSE: crate::locks::pause_points::PausePoints; }

/// Where a work master comes from. It decides what happens to the work's
/// `cover_upload` row (its admin cover), which must always name the current master or
/// not exist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CoverOrigin<'a> {
    /// Fetched from a source: an admin cover it replaces is gone.
    Source(&'a str),
    /// The admin uploaded it: kept in PostgreSQL so it survives the loss of the files.
    Upload,
    /// The admin picked it from the work's cover gallery (`choice` is the gallery key):
    /// fetched from `source` and kept in PostgreSQL like an upload.
    Pick { source: &'a str, choice: &'a str },
    /// The cover ranking chose `source`: refused while the work has an admin cover or
    /// its published version is no longer `version`.
    Ranked {
        source: &'a str,
        version: Option<i64>,
    },
    /// The same art re-encoded: an uploaded original follows it.
    Recompress,
}

impl<'a> CoverOrigin<'a> {
    fn source(self) -> Option<&'a str> {
        match self {
            Self::Source(source) | Self::Pick { source, .. } | Self::Ranked { source, .. } => {
                Some(source)
            }
            Self::Upload | Self::Recompress => None,
        }
    }
}

#[cfg(test)]
pub async fn put_work_cover(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
    webp: &[u8],
) -> Result<()> {
    put_source_work_cover(main, covers, work_id, webp, "mangadex").await?;
    Ok(())
}

pub async fn put_source_work_cover(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
    webp: &[u8],
    source_id: &str,
) -> Result<bool> {
    publish_work_cover(
        main,
        covers,
        work_id,
        webp,
        None,
        None,
        None,
        CoverOrigin::Source(source_id),
    )
    .await
}

/// The admin upload: published like any cover, and kept in `cover_upload`.
pub async fn put_uploaded_work_cover(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
    webp: &[u8],
) -> Result<bool> {
    publish_work_cover(
        main,
        covers,
        work_id,
        webp,
        None,
        None,
        None,
        CoverOrigin::Upload,
    )
    .await
}

/// An admin pick from the gallery or a ranking choice ([`CoverOrigin::Pick`] or
/// [`CoverOrigin::Ranked`]): published like a source cover under the same guards.
pub(crate) async fn put_chosen_work_cover(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
    webp: &[u8],
    origin: CoverOrigin<'_>,
) -> Result<bool> {
    debug_assert!(matches!(
        origin,
        CoverOrigin::Pick { .. } | CoverOrigin::Ranked { .. }
    ));
    publish_work_cover(main, covers, work_id, webp, None, None, None, origin).await
}

/// Recompression: replace the master only while it is still `expected`.
pub async fn put_work_cover_if_current(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
    webp: &[u8],
    expected: Option<&[u8]>,
) -> Result<bool> {
    publish_work_cover(
        main,
        covers,
        work_id,
        webp,
        Some(expected),
        None,
        None,
        CoverOrigin::Recompress,
    )
    .await
}

pub async fn put_work_cover_for_missing_anchor(
    main: &Database,
    covers: &CoverStore,
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
        None,
        CoverOrigin::Source("mangadex"),
    )
    .await
}

/// A work cover fetched from the work's Suwayomi anchor: published only while the
/// master is missing and the anchor and engine generation are still current. A queued
/// crawl job can outlive a generation change, and the number may name another series.
#[cfg(test)]
pub async fn put_work_cover_for_suwayomi_anchor(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
    webp: &[u8],
    generation: Option<uuid::Uuid>,
    thumbnail_url: &str,
) -> Result<bool> {
    let source: Option<String> = sqlx::query_scalar(&format!(
        "SELECT COALESCE(ss.source_id, ss.source_type) FROM source_series ss \
         WHERE ss.work_id = $1 AND ss.source_type = 'suwayomi' \
         ORDER BY {}, ss.created_at, ss.id LIMIT 1",
        crate::catalog::work_metadata::cover_rank_sql("ss")
    ))
    .bind(work_id)
    .fetch_optional(main)
    .await?;
    put_work_cover_for_suwayomi_source_anchor(
        main,
        covers,
        work_id,
        webp,
        generation,
        thumbnail_url,
        source.as_deref().unwrap_or("suwayomi"),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn put_work_cover_for_suwayomi_source_anchor(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
    webp: &[u8],
    generation: Option<uuid::Uuid>,
    thumbnail_url: &str,
    source_id: &str,
) -> Result<bool> {
    publish_work_cover(
        main,
        covers,
        work_id,
        webp,
        Some(None),
        None,
        Some((generation, thumbnail_url)),
        CoverOrigin::Source(source_id),
    )
    .await
}

/// The one publication of a work master. The file is staged before any lock; then ONE
/// PostgreSQL transaction takes the generation lock (Suwayomi anchor only), the work's
/// `Cover` key with the object's `CoverFile` key, and the work row; checks the guards;
/// places the file; and writes the index row, drops the old derivatives, and publishes
/// the version and every materialized card URL. A crash anywhere before commit leaves the
/// old cover in place.
#[allow(clippy::too_many_arguments)]
async fn publish_work_cover(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
    webp: &[u8],
    expected: Option<Option<&[u8]>>,
    expected_anchor: Option<(&str, &str)>,
    expected_suwayomi: Option<(Option<uuid::Uuid>, &str)>,
    origin: CoverOrigin<'_>,
) -> Result<bool> {
    let now = Utc::now();
    // Hashing and the temp file write stay outside every lock.
    let expected_sha = expected.flatten().map(cover_store::sha256);
    let mut staged = covers.stage(webp).await?;
    #[cfg(test)]
    if let Ok(points) = COVER_PUBLICATION_PAUSE.try_with(Clone::clone) {
        points.hit("cover-before-lock").await;
    }
    let mut tx = main.begin().await?;
    // The generation lock comes before any work lock: a new generation holds it
    // exclusively while it locks works.
    if let Some((generation, _)) = expected_suwayomi {
        match crate::engine_generation::shared(&mut tx, generation).await {
            Ok(_) => {}
            Err(e) if e.is::<crate::engine_generation::FenceError>() => {
                tx.rollback().await?;
                return Ok(false);
            }
            Err(e) => return Err(e),
        }
    }
    let source_id = origin.source();
    let _asset =
        lock_assets_files_and_source(&mut tx, &[work_id], &[staged.sha], source_id).await?;
    if !crate::locks::lock_works(&mut tx, &[work_id])
        .await?
        .missing
        .is_empty()
    {
        tx.rollback().await?;
        return Ok(false);
    }
    #[cfg(test)]
    crate::catalog::guards::pause("cover-work-locked").await;
    if let Some(source) = source_id {
        if crate::catalog::work_metadata::barred_groups_on(&mut tx, source)
            .await?
            .contains(&crate::catalog::work_metadata::FieldGroup::Cover)
        {
            tx.rollback().await?;
            return Ok(false);
        }
    }
    // A takedown locks the same work row, so a fetch that was in flight when it landed
    // cannot store the art again after the takedown deleted it.
    let taken_down: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM media_takedown \
         WHERE subject_kind = 'work' AND subject_id = $1)",
    )
    .bind(work_id)
    .fetch_one(&mut *tx)
    .await?;
    if taken_down {
        tx.rollback().await?;
        return Ok(false);
    }
    if let CoverOrigin::Ranked { version, .. } = origin {
        let current: bool = sqlx::query_scalar(
            "SELECT w.cover_cached_version IS NOT DISTINCT FROM $2 \
                AND NOT EXISTS (SELECT 1 FROM cover_upload u WHERE u.work_id = w.id) \
             FROM work w WHERE w.id = $1",
        )
        .bind(work_id)
        .bind(version)
        .fetch_one(&mut *tx)
        .await?;
        if !current {
            tx.rollback().await?;
            return Ok(false);
        }
    }
    if let Some((_, thumbnail_url)) = expected_suwayomi {
        let barred = crate::catalog::work_metadata::barred_sql(
            &crate::catalog::work_metadata::mapping_source_sql("ss"),
            "'cover'",
        );
        let current: Option<(Option<String>, String)> = sqlx::query_as(&format!(
            "SELECT s.thumbnail_url, COALESCE(ss.source_id, ss.source_type) \
             FROM source_series ss JOIN suwayomi_series s ON s.id = CAST(ss.source_key AS INTEGER) \
             WHERE ss.work_id = $1 AND ss.source_type = 'suwayomi' AND NOT {barred} \
             ORDER BY {rank}, ss.created_at ASC, ss.id ASC LIMIT 1",
            rank = crate::catalog::work_metadata::cover_rank_sql("ss"),
        ))
        .bind(work_id)
        .fetch_optional(&mut *tx)
        .await?;
        if current
            .as_ref()
            .map(|(url, source)| (url.as_deref(), Some(source.as_str())))
            != Some((Some(thumbnail_url), source_id))
        {
            tx.rollback().await?;
            return Ok(false);
        }
    }
    if let Some((mangadex_id, file_name)) = expected_anchor {
        let barred = crate::catalog::work_metadata::barred_sql(
            &crate::catalog::work_metadata::mapping_source_sql("ss"),
            "'cover'",
        );
        let current: Option<(String, Option<String>)> = sqlx::query_as(&format!(
            "SELECT ss.source_key, w.cover_file_name FROM work w \
             JOIN source_series ss ON ss.work_id = w.id AND ss.source_type = 'mangadex' \
             WHERE w.id = $1 AND NOT {barred} ORDER BY ss.created_at, ss.id LIMIT 1"
        ))
        .bind(work_id)
        .fetch_optional(&mut *tx)
        .await?;
        if current
            .as_ref()
            .map(|(id, file)| (id.as_str(), file.as_deref()))
            != Some((mangadex_id, Some(file_name)))
        {
            tx.rollback().await?;
            return Ok(false);
        }
    }
    let current = current_master(&mut tx, covers, work_id).await?;
    if let Some(expected) = expected {
        if !current.is(expected, expected_sha.as_ref()) {
            tx.rollback().await?;
            return Ok(false);
        }
    }
    let previous = current.version();
    let published: Option<i64> =
        sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id = $1")
            .bind(work_id)
            .fetch_one(&mut *tx)
            .await?;
    let version = now.timestamp_millis().max(
        previous
            .unwrap_or(0)
            .max(published.unwrap_or(0))
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("cover version exhausted"))?,
    );
    #[cfg(test)]
    if let Ok(points) = COVER_PUBLICATION_PAUSE.try_with(Clone::clone) {
        points.hit("cover-locked").await;
    }
    covers.place(&mut staged).await?;
    // The index row replaces the old master and drops its derivatives: they are the old
    // art. The `?v=` bust only stops browsers and the edge from reusing old bytes.
    cover_store::put_master(&mut tx, work_id, &staged, Some(version)).await?;
    match origin {
        CoverOrigin::Upload | CoverOrigin::Pick { .. } => {
            let choice = match origin {
                CoverOrigin::Pick { choice, .. } => Some(choice),
                _ => None,
            };
            sqlx::query(
                "INSERT INTO cover_upload (work_id, bytes, sha256, created_at, choice) VALUES ($1, $2, $3, now(), $4) \
                 ON CONFLICT (work_id) DO UPDATE SET bytes = excluded.bytes, sha256 = excluded.sha256, \
                   created_at = excluded.created_at, choice = excluded.choice",
            )
            .bind(work_id)
            .bind(webp)
            .bind(staged.sha.as_slice())
            .bind(choice)
            .execute(&mut *tx)
            .await?;
        }
        CoverOrigin::Recompress => {
            sqlx::query("UPDATE cover_upload SET bytes = $2, sha256 = $3 WHERE work_id = $1")
                .bind(work_id)
                .bind(webp)
                .bind(staged.sha.as_slice())
                .execute(&mut *tx)
                .await?;
        }
        CoverOrigin::Source(_) | CoverOrigin::Ranked { .. } => {
            sqlx::query("DELETE FROM cover_upload WHERE work_id = $1")
                .bind(work_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    // Publish the version and every materialized card URL in the same transaction.
    // Updating only work left library/browse cards pointing at immutable damaged
    // thumbnails even after an admin had successfully fetched a replacement.
    let provenance = match origin {
        CoverOrigin::Recompress => None,
        origin => Some(origin.source()),
    };
    publish_work_cover_version_with_origin(&mut tx, work_id, version, provenance).await?;
    tx.commit().await?;
    Ok(true)
}

/// Caller holds the Cover asset lock and work row lock, as in publish_work_cover.
pub(crate) async fn publish_work_cover_version(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    work_id: &str,
    version: i64,
) -> Result<bool> {
    publish_work_cover_version_with_origin(tx, work_id, version, None).await
}

async fn publish_work_cover_version_with_origin(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    work_id: &str,
    version: i64,
    origin: Option<Option<&str>>,
) -> Result<bool> {
    let changed = sqlx::query(
        "UPDATE work SET cover_cached_version = $1, \
         cover_source = CASE WHEN $3 THEN $4 ELSE cover_source END \
         WHERE id = $2 AND cover_cached_version IS DISTINCT FROM $1",
    )
    .bind(version)
    .bind(work_id)
    .bind(origin.is_some())
    .bind(origin.flatten())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if changed == 0 {
        return Ok(false);
    }
    let url = cover_path(work_id, version);
    for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
        sqlx::query(&format!(
            "UPDATE {table} SET cover_url = $1 WHERE work_id = $2 AND cover_url IS DISTINCT FROM $1"
        ))
        .bind(&url)
        .bind(work_id)
        .execute(&mut **tx)
        .await?;
    }
    Ok(true)
}

pub(crate) async fn legacy_work_cover_revoked<'e, E>(conn: E, work_id: &str) -> Result<bool>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    Ok(
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM work_cover_revocation WHERE work_id = $1)",
        )
        .bind(work_id)
        .fetch_one(conn)
        .await?,
    )
}

pub(crate) async fn source_cover_barred(main: &Database, source: &str) -> Result<bool> {
    let mut conn = main.acquire().await?;
    Ok(
        crate::catalog::work_metadata::barred_groups_on(&mut conn, source)
            .await?
            .contains(&crate::catalog::work_metadata::FieldGroup::Cover),
    )
}

/// One retried unit after a cover bar commits. Provenance, rather than today's
/// mappings, identifies the master to remove. Files are reclaimed by GC.
pub(crate) async fn remove_barred_work_cover(
    main: &Database,
    work_id: &str,
    source_id: &str,
) -> Result<bool> {
    crate::db::retry_transaction(|| async {
        let mut tx = main.begin().await?;
        let _asset = lock_assets_files_and_source(&mut tx, &[work_id], &[], Some(source_id)).await?;
        if !crate::locks::lock_works(&mut tx, &[work_id]).await?.missing.is_empty() {
            tx.rollback().await?;
            return Ok(false);
        }
        let origin: Option<String> = sqlx::query_scalar("SELECT cover_source FROM work WHERE id = $1")
            .bind(work_id).fetch_one(&mut *tx).await?;
        if origin.as_deref() != Some(source_id)
            || !crate::catalog::work_metadata::barred_groups_on(&mut tx, source_id).await?
                .contains(&crate::catalog::work_metadata::FieldGroup::Cover)
        {
            tx.rollback().await?;
            return Ok(false);
        }
        cover_store::delete_asset(&mut tx, work_id).await?;
        // A cover with this provenance and an admin row is a gallery pick from the source.
        sqlx::query("DELETE FROM cover_upload WHERE work_id = $1")
            .bind(work_id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO work_cover_revocation (work_id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(work_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE work SET cover_cached_version = NULL, cover_source = NULL WHERE id = $1")
            .bind(work_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM work_cover_issue WHERE work_id = $1")
            .bind(work_id).execute(&mut *tx).await?;
        for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
            sqlx::query(&format!("UPDATE {table} SET cover_url = '' WHERE work_id = $1 AND cover_url IS DISTINCT FROM ''"))
                .bind(work_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(true)
    }).await
}

/// A work takedown (M5 `media_policy`): delete the stored covers of `work_ids` (the work
/// and its merged-away ids) and of the engine series `manga_ids`, with every derivative
/// and any uploaded original, and clear each work's cover pointer. Under the same locks a
/// publication takes, one asset at a time. The cover routes refuse a denied work before
/// any lazy fill, and the drainer skips it, so nothing fetches the art again while the
/// takedown holds. The files go with GC once nothing names them.
pub async fn delete_for_takedown(
    main: &Database,
    covers: &CoverStore,
    work_ids: &[String],
    manga_ids: &[i64],
) -> Result<()> {
    for work_id in work_ids {
        let mut tx = main.begin().await?;
        let _asset = lock_asset(&mut tx, work_id).await?;
        crate::locks::lock_works(&mut tx, &[work_id.as_str()]).await?;
        cover_store::delete_asset(&mut tx, work_id).await?;
        sqlx::query("DELETE FROM cover_upload WHERE work_id = $1")
            .bind(work_id)
            .execute(&mut *tx)
            .await?;
        if let Some(legacy) = covers.legacy() {
            let mut cache = legacy.begin().await?;
            sqlx::query("DELETE FROM work_cover_blob WHERE work_id = $1")
                .bind(work_id)
                .execute(&mut *cache)
                .await?;
            sqlx::query(
                "DELETE FROM cover_variant WHERE asset_key = $1 \
                 OR substr(asset_key, 1, length($1) + 2) = $1 || '@v'",
            )
            .bind(work_id)
            .execute(&mut *cache)
            .await?;
            cache.commit().await?;
        }
        sqlx::query(
            "UPDATE work SET cover_cached_version = NULL, cover_source = NULL \
             WHERE id = $1 AND cover_cached_version IS NOT NULL",
        )
        .bind(work_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    for &manga_id in manga_ids {
        let key = suwayomi_variant_key(manga_id);
        let mut tx = main.begin().await?;
        let _asset = lock_asset(&mut tx, &key).await?;
        cover_store::delete_asset(&mut tx, &key).await?;
        if let Some(legacy) = covers.legacy() {
            let mut cache = legacy.begin().await?;
            sqlx::query("DELETE FROM suwayomi_cover_blob WHERE manga_id = $1")
                .bind(manga_id)
                .execute(&mut *cache)
                .await?;
            sqlx::query(
                "DELETE FROM cover_variant WHERE asset_key = $1 \
                 OR substr(asset_key, 1, length($1) + 2) = $1 || '@v'",
            )
            .bind(&key)
            .execute(&mut *cache)
            .await?;
            cache.commit().await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

/// The stored version of `work_id`'s master: the index row, or the SQLite cache for a
/// work the copy has not reached.
async fn stored_work_version<'e, E>(
    conn: E,
    covers: &CoverStore,
    work_id: &str,
) -> Result<Option<i64>>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row: Option<(Option<i64>, bool, bool)> = sqlx::query_as(
        "SELECT c.version, c.asset_key IS NOT NULL, \
         EXISTS (SELECT 1 FROM work_cover_revocation r WHERE r.work_id = w.id) \
         FROM work w LEFT JOIN cover_object c ON c.work_id = w.id AND c.kind = 'master' WHERE w.id = $1",
    ).bind(work_id).fetch_optional(conn).await?;
    if let Some((version, indexed, revoked)) = row {
        if indexed || revoked {
            return Ok(version);
        }
    }
    let Some(legacy) = covers.legacy_for_read(work_id) else {
        return Ok(None);
    };
    Ok(
        sqlx::query_scalar("SELECT version FROM work_cover_blob WHERE work_id = $1")
            .bind(work_id)
            .fetch_optional(legacy)
            .await?,
    )
}

/// Move a work's published pointer up to its stored master's version. The index row and
/// the pointer change together, so a lower pointer is only left behind by a master the
/// SQLite cache still holds (published there before the copy), or by the copy itself.
/// Re-read under the publication locks: a queued repair must not republish its request's
/// old snapshot, and never lowers a pointer.
async fn repair_work_cover_version(
    main: &Database,
    covers: &CoverStore,
    work_id: &str,
) -> Result<bool> {
    // Most old links merely came from a browser cache. As at startup, probe first
    // so an already-current pointer does not acquire a writer or dirty row locks.
    let published: Option<Option<i64>> =
        sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id = $1")
            .bind(work_id)
            .fetch_optional(main)
            .await?;
    let Some(published) = published else {
        return Ok(false);
    };
    let Some(version) = stored_work_version(main, covers, work_id).await? else {
        return Ok(false);
    };
    if published.is_some_and(|published| published >= version) {
        return Ok(false);
    }
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
    let Some(version) = stored_work_version(&mut *tx, covers, work_id).await? else {
        tx.rollback().await?;
        return Ok(false);
    };
    let published: Option<i64> =
        sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id = $1")
            .bind(work_id)
            .fetch_one(&mut *tx)
            .await?;
    if published.is_some_and(|published| published >= version) {
        tx.rollback().await?;
        return Ok(false);
    }
    let changed = publish_work_cover_version(&mut tx, work_id, version).await?;
    tx.commit().await?;
    Ok(changed)
}

#[derive(Default)]
struct RecentCoverRepairs(std::collections::HashMap<String, std::time::Instant>);

impl RecentCoverRepairs {
    fn reserve(&mut self, work_id: &str, now: std::time::Instant) -> bool {
        self.0
            .retain(|_, seen| now.duration_since(*seen) < Duration::from_secs(300));
        if self.0.contains_key(work_id) || self.0.len() >= 1024 {
            return false;
        }
        self.0.insert(work_id.to_owned(), now);
        true
    }
}

static RECENT_COVER_REPAIRS: std::sync::LazyLock<std::sync::Mutex<RecentCoverRepairs>> =
    std::sync::LazyLock::new(Default::default);

pub fn defer_work_cover_version_repair(main: &Database, covers: &CoverStore, work_id: &str) {
    if !RECENT_COVER_REPAIRS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .reserve(work_id, std::time::Instant::now())
    {
        return;
    }
    let database = main.background();
    let covers = covers.clone();
    let work = work_id.to_owned();
    let queued = main.defer(async move {
        let result = repair_work_cover_version(&database, &covers, &work).await;
        if result.is_err() {
            RECENT_COVER_REPAIRS
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .0
                .remove(&work);
        }
        if result? {
            crate::graphql::clear_feed_caches();
        }
        Ok(())
    });
    if !queued {
        RECENT_COVER_REPAIRS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .remove(work_id);
    }
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
    let barred = crate::catalog::work_metadata::barred_sql(
        &crate::catalog::work_metadata::mapping_source_sql("ss"),
        "'cover'",
    );
    let n: i64 = crate::db_types::query_scalar(
        &format!("SELECT COUNT(*) FROM work w \
         WHERE w.cover_cached_version IS NULL \
           AND NOT EXISTS (SELECT 1 FROM media_takedown td \
                           WHERE td.subject_kind = 'work' AND td.subject_id = w.id) \
           AND NOT EXISTS (SELECT 1 FROM work_cover_issue i WHERE i.work_id = w.id \
                           AND (i.retry_after IS NULL OR i.retry_after > $1)) AND ( \
             (w.cover_file_name IS NOT NULL \
              AND EXISTS (SELECT 1 FROM source_series ss \
                          WHERE ss.work_id = w.id AND ss.source_type = 'mangadex' AND NOT {barred})) \
             OR EXISTS (SELECT 1 FROM source_series ss \
                        WHERE ss.work_id = w.id AND ss.source_type = 'suwayomi' AND NOT {barred}))"),
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

/// One cover a work's sources can supply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepairCoverSource {
    MangaDex { manga_id: String, file_name: String },
    Suwayomi { path: String, source_id: String },
}

impl RepairCoverSource {
    pub(crate) fn source_id(&self) -> &str {
        match self {
            Self::MangaDex { .. } => "mangadex",
            Self::Suwayomi { source_id, .. } => source_id,
        }
    }
}

async fn repair_cover_sources(main: &Database, work_id: &str) -> Result<Vec<RepairCoverSource>> {
    Ok(ranked_cover_sources(main, work_id)
        .await?
        .into_iter()
        .map(|(_, source)| source)
        .collect())
}

/// The covers `work_id`'s allowed sources can supply, each with its cover ranking
/// position (`i32::MAX` unranked), in cover order: by position, then MangaDex, then
/// the oldest Suwayomi mapping.
pub(crate) async fn ranked_cover_sources(
    main: &Database,
    work_id: &str,
) -> Result<Vec<(i32, RepairCoverSource)>> {
    let barred = crate::catalog::work_metadata::barred_sql(
        &crate::catalog::work_metadata::mapping_source_sql("ss"),
        "'cover'",
    );
    let rank = crate::catalog::work_metadata::cover_rank_sql("ss");
    let md = crate::db_types::query_tuple::<(Option<String>, Option<String>, i32)>(&format!(
        "SELECT (SELECT ss.source_key FROM source_series ss \
                 WHERE ss.work_id = w.id AND ss.source_type = 'mangadex' AND NOT {barred} \
                 ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1), \
                w.cover_file_name, \
                COALESCE((SELECT r.position FROM cover_source_rank r WHERE r.source_id = 'mangadex'), 2147483647) \
         FROM work w WHERE w.id = $1"
    ))
    .bind(work_id)
    .fetch_optional(main)
    .await?;
    let mut sources = Vec::new();
    if let Some((Some(manga_id), Some(file_name), position)) = md {
        if !manga_id.is_empty() && !file_name.is_empty() {
            sources.push((
                position,
                RepairCoverSource::MangaDex {
                    manga_id,
                    file_name,
                },
            ));
        }
    }
    // Every linked engine manga can supply a cover, even before its metadata is
    // mirrored locally. Use the internal thumbnail endpoint rather than requiring
    // a cached thumbnail_url or following a stale public/CDN URL.
    let keys: Vec<(String, String, i32)> = sqlx::query_as(&format!(
        "SELECT ss.source_key, COALESCE(ss.source_id, ss.source_type), {rank} \
         FROM source_series ss WHERE ss.work_id = $1 \
         AND ss.source_type = 'suwayomi' AND NOT {barred} ORDER BY {rank}, ss.created_at ASC, ss.id ASC"
    ))
    .bind(work_id)
    .fetch_all(main)
    .await?;
    for (key, source_id, position) in keys {
        let Ok(id) = key.parse::<i64>() else { continue };
        if id < 0 {
            continue;
        }
        let candidate = RepairCoverSource::Suwayomi {
            path: format!("/api/v1/manga/{id}/thumbnail"),
            source_id,
        };
        if !sources.iter().any(|(_, source)| *source == candidate) {
            sources.push((position, candidate));
        }
    }
    // Stable: equal positions keep MangaDex first, then the mapping order.
    sources.sort_by_key(|(position, _)| *position);
    Ok(sources)
}

/// Download one source's cover: MangaDex through its client (refused while MangaDex is
/// off), an engine manga through its regional route or the owner engine.
pub(crate) async fn fetch_cover_source(
    main: &Database,
    mangadex: &MangaDexClient,
    suwayomi: &crate::suwayomi::SuwayomiClient,
    source: &RepairCoverSource,
) -> Option<Vec<u8>> {
    match source {
        RepairCoverSource::MangaDex {
            manga_id,
            file_name,
        } => {
            if !mangadex.is_enabled() {
                return None;
            }
            mangadex.cover_thumb_bytes(manga_id, file_name).await
        }
        RepairCoverSource::Suwayomi { path, .. } => match crate::engine_route::cover_manga_id(path)
        {
            Some(id) => {
                crate::engine_route::cover_bytes(
                    suwayomi,
                    main,
                    crate::engine_route::CoverTarget::OwnerManga(id),
                    Some(path),
                )
                .await
            }
            None => suwayomi.cover_bytes(Some(path)).await,
        },
    }
}

async fn process_repair_sources<F, Fut>(
    sources: Vec<RepairCoverSource>,
    mut fetch: F,
) -> Result<Option<(RepairCoverSource, Vec<u8>)>>
where
    F: FnMut(RepairCoverSource) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<u8>>>,
{
    let mut last_error = None;
    for source in sources {
        let work = master_cover_work().await;
        let Some(bytes) = fetch(source.clone()).await else {
            continue;
        };
        // A download is only usable after it has decoded and processed. A corrupt
        // MangaDex image must not prevent trying Suwayomi, nor may a bad first
        // Suwayomi source prevent trying the remaining linked sources.
        match work.process(move || process_cover(&bytes)).await? {
            Ok(webp) => return Ok(Some((source, webp))),
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

/// Admin cover repair: try MangaDex and every linked Suwayomi source, in cover rank
/// order, until one supplies a complete, processable image. Works without MangaDex use Suwayomi
/// directly. Only store a successful replacement; leave the current cover intact
/// if every source fails.
pub async fn retry_one_cover(
    main: &Database,
    covers: &CoverStore,
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
        fetch_cover_source(main, mangadex, suwayomi, &source).await
    })
    .await;
    match processed {
        Ok(Some((source, webp))) => {
            if !put_source_work_cover(main, covers, work_id, &webp, source.source_id()).await? {
                return Ok(false);
            }
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
    covers: &CoverStore,
    mangadex: &MangaDexClient,
    suwayomi: &crate::suwayomi::SuwayomiClient,
    limit: Option<i64>,
    source_budgets: &SourceBudgets,
    shutdown: &tokio::sync::watch::Receiver<bool>,
) {
    // The mangadex anchor lives on `source_series`, not `work`; pick the same
    // deterministic anchor the reader uses (oldest source_series) so the cover
    // matches the pages. `limit` bounds a single (auto-drainer) tick; None = drain
    // everything still uncached (the manual admin crawl).
    let barred = crate::catalog::work_metadata::barred_sql(
        &crate::catalog::work_metadata::mapping_source_sql("ss"),
        "'cover'",
    );
    let base = format!(
        "SELECT w.id, \
                (SELECT ss.source_key FROM source_series ss \
                 WHERE ss.work_id = w.id AND ss.source_type = 'mangadex' AND NOT {barred} \
                 ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1) AS mangadex_id, \
                w.cover_file_name \
         FROM work w \
         WHERE w.cover_cached_version IS NULL AND w.cover_file_name IS NOT NULL \
           AND (NOT EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id \
                            AND ss.source_type = 'mangadex') \
                OR EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id \
                           AND ss.source_type = 'mangadex' AND NOT {barred})) \
           AND NOT EXISTS (SELECT 1 FROM media_takedown td \
                           WHERE td.subject_kind = 'work' AND td.subject_id = w.id) \
           AND NOT EXISTS (SELECT 1 FROM work_cover_issue i WHERE i.work_id = w.id \
                           AND (i.retry_after IS NULL OR i.retry_after > $1))"
    );
    // A marker only hides a work while its backoff is unexpired (0100), so `now` is
    // bound into every crawl SELECT rather than the marker being an absolute exclusion.
    let now = Utc::now().to_rfc3339();
    let loaded = match limit {
        Some(n) => {
            crate::db_types::query_tuple::<(String, Option<String>, Option<String>)>(&format!(
                "{base} LIMIT $2"
            ))
            .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
            .bind(n)
            .fetch_all(main)
            .await
        }
        None => {
            crate::db_types::query_tuple::<(String, Option<String>, Option<String>)>(&base)
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
                Ok(Ok(webp)) => match put_work_cover_for_missing_anchor(
                    main,
                    covers,
                    &job.work_id,
                    &webp,
                    &job.mangadex_id,
                    &job.file_name,
                )
                .await
                {
                    Ok(true) => {
                        saved += 1;
                        clear_cover_issue(main, &job.work_id).await;
                    }
                    Ok(false) => {
                        tracing::debug!(work_id = %job.work_id, "cover crawl: master already stored or anchor changed; skipped");
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
    let rank = crate::catalog::work_metadata::cover_rank_sql("ss");
    let suw_base = format!(
        "SELECT w.id, \
                (SELECT s.thumbnail_url FROM source_series ss \
                 JOIN suwayomi_series s ON s.id = CAST(ss.source_key AS INTEGER) \
                 WHERE ss.work_id = w.id AND ss.source_type = 'suwayomi' AND NOT {barred} \
                 ORDER BY {rank}, ss.created_at ASC, ss.id ASC LIMIT 1) AS thumbnail_url, \
                (SELECT s.source_id FROM source_series ss \
                 JOIN suwayomi_series s ON s.id = CAST(ss.source_key AS INTEGER) \
                 WHERE ss.work_id = w.id AND ss.source_type = 'suwayomi' AND NOT {barred} \
                 ORDER BY {rank}, ss.created_at ASC, ss.id ASC LIMIT 1) AS source_id, \
                (SELECT COALESCE(ss.source_id, ss.source_type) FROM source_series ss \
                 JOIN suwayomi_series s ON s.id = CAST(ss.source_key AS INTEGER) \
                 WHERE ss.work_id = w.id AND ss.source_type = 'suwayomi' AND NOT {barred} \
                 ORDER BY {rank}, ss.created_at ASC, ss.id ASC LIMIT 1) AS origin_source \
         FROM work w \
         WHERE w.cover_cached_version IS NULL \
           AND NOT EXISTS (SELECT 1 FROM media_takedown td \
                           WHERE td.subject_kind = 'work' AND td.subject_id = w.id) \
           AND NOT EXISTS (SELECT 1 FROM work_cover_issue i WHERE i.work_id = w.id \
                           AND (i.retry_after IS NULL OR i.retry_after > $1)) \
           AND EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id \
                       AND ss.source_type = 'suwayomi' AND NOT {barred})"
    );
    // Read before the jobs: a job is then never tagged with a newer generation than
    // the anchor rows it came from.
    let suw_generation = match crate::engine_generation::current(main).await {
        Ok(generation) => generation,
        Err(e) => {
            tracing::warn!(error = %e, "cover crawl (suwayomi): engine generation unreadable");
            return;
        }
    };
    let suw_loaded = match limit {
        Some(n) => crate::db_types::query_tuple::<(
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        )>(&format!("{suw_base} LIMIT $2"))
        .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
        .bind(n)
        .fetch_all(main)
        .await,
        None => crate::db_types::query_tuple::<(
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        )>(&suw_base)
        .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
        .fetch_all(main)
        .await,
    };
    let suw_jobs: Vec<(String, String, Option<String>, String)> = match suw_loaded {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|(work_id, thumb, source_id, origin)| {
                Some((
                    work_id,
                    thumb.filter(|t| !t.is_empty())?,
                    source_id,
                    origin?,
                ))
            })
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
        .map(|(work_id, thumb, source_id, origin)| async move {
            // The source's rate budget first, so a paced fetch holds no work slot.
            if let Some(source_id) = &source_id {
                if !source_budgets.spend(source_id, SourceUse::Cover, shutdown).await {
                    return (0usize, 0usize);
                }
            }
            let work = master_cover_work().await;
            // The spend above pays for a regional engine's lookup too.
            let fetched = match crate::engine_route::cover_manga_id(&thumb) {
                Some(id) => crate::engine_route::prepaid(crate::engine_route::cover_bytes(
                    suwayomi, main, crate::engine_route::CoverTarget::OwnerManga(id), Some(&thumb),
                )).await,
                None => suwayomi.cover_bytes(Some(&thumb)).await,
            };
            let bytes = match fetched {
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
                Ok(Ok(webp)) => match put_work_cover_for_suwayomi_source_anchor(
                    main,
                    covers,
                    &work_id,
                    &webp,
                    suw_generation,
                    &thumb,
                    &origin,
                )
                .await
                {
                    Ok(true) => {
                        clear_cover_issue(main, &work_id).await;
                        (1, 0)
                    }
                    Ok(false) => {
                        tracing::debug!(work_id = %work_id, "cover crawl (suwayomi): master already stored, anchor or engine generation changed; skipped");
                        (0, 0)
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
///
/// The index holds the copied and newly published masters; during the copy the SQLite
/// cache adds those not copied yet.
async fn cached_suwayomi_cover_ids(
    main: &Database,
    covers: &CoverStore,
) -> Result<std::collections::HashSet<i64>> {
    let mut ids: std::collections::HashSet<i64> = sqlx::query_scalar::<_, String>(
        "SELECT substr(asset_key, 3) FROM cover_object WHERE asset_key LIKE 's:%' AND kind = 'master'",
    )
    .fetch_all(main)
    .await?
    .into_iter()
    .filter_map(|id| id.parse().ok())
    .collect();
    if let Some(legacy) = covers.legacy() {
        // Only ids the copy has not passed: serving no longer reads SQLite for the others.
        let cursor = covers.copy_cursor();
        ids.extend(
            sqlx::query_scalar::<_, i64>("SELECT manga_id FROM suwayomi_cover_blob")
                .fetch_all(legacy)
                .await?
                .into_iter()
                .filter(|id| !cursor.passed(&format!("s:{id}"))),
        );
    }
    Ok(ids)
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
/// Five exclusions, in the SELECT rather than in Rust so the "nothing to do" gauge
/// ([`pending_suwayomi_cover_count`]) sees exactly what the warmer sees:
///
/// 1. no per-cover marker whose `retry_after` is still in the future;
/// 2. …unless the series has been re-fetched since (`source_fetched_at IS
///    s.last_fetched_at` — SQLite `IS` is null-safe). The extension re-fetching the
///    manga is precisely the event that can replace a `data:` thumbnail with a real
///    URL, so the marker is void from that moment, permanent or not;
/// 3. no un-expired park on the whole source (`cover_source_backoff`);
/// 4. no work takedown for the series;
/// 5. with the cover prune on, no cover the prune removes (`cover_choice`).
async fn warm_candidates(main: &Database, before_id: i64) -> Result<Vec<WarmCandidate>> {
    let now = Utc::now().to_rfc3339();
    Ok(sqlx::query_as::<_, WarmCandidate>(&format!(
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
            AND NOT EXISTS (SELECT 1 FROM source_series ss \
                             JOIN media_takedown td ON td.subject_kind = 'work' \
                               AND td.subject_id = ss.work_id \
                             WHERE ss.source_type = 'suwayomi' AND ss.source_key = s.id::text) \
            AND NOT {unpicked} \
          ORDER BY s.id DESC",
        unpicked = crate::cover_choice::unpicked_warm_sql("$4::boolean"),
    ))
    .bind(before_id)
    .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
    .bind(crate::db_types::date(&now).expect("internally generated UTC timestamp"))
    .bind(crate::cover_choice::prune_enabled(main).await)
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
    covers: &CoverStore,
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
    covers: &CoverStore,
    limit: i64,
    before_id: i64,
) -> Result<Vec<WarmCandidate>> {
    let cached = cached_suwayomi_cover_ids(main, covers).await?;
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
pub async fn pending_suwayomi_cover_count(main: &Database, covers: &CoverStore) -> Result<i64> {
    let cached = cached_suwayomi_cover_ids(main, covers).await?;
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
    covers: &CoverStore,
    main: &Database,
    suwayomi: &crate::suwayomi::SuwayomiClient,
    limit: i64,
    cursor: &std::sync::atomic::AtomicI64,
    shutdown: &tokio::sync::watch::Receiver<bool>,
    source_budgets: &SourceBudgets,
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
            // The source's rate budget first, so a paced fetch holds no work slot.
            if !source_budgets.spend(&source_id, SourceUse::Cover, shutdown).await {
                return (source_id, WarmOutcome::Skipped);
            }
            let work = master_cover_work().await;
            if *shutdown.borrow() {
                return (source_id, WarmOutcome::Skipped);
            }
            let generation = suwayomi.fence.as_ref().and_then(|f| f.generation());
            let path = format!("/api/v1/manga/{manga_id}/thumbnail");
            let fetch_result = match crate::engine_route::prepaid(crate::engine_route::route_cover(
                suwayomi, main, crate::engine_route::CoverTarget::OwnerManga(manga_id), true,
            )).await {
                crate::engine_route::Routed::Served(cover) => Ok(cover),
                crate::engine_route::Routed::Owner => suwayomi.fetch_cover_background(&path).await,
                crate::engine_route::Routed::Busy | crate::engine_route::Routed::Refuse => {
                    Err(anyhow::anyhow!("regional cover unavailable"))
                }
            };
            let bytes = match fetch_result {
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
                Ok(Ok(webp)) => match publish_suwayomi_cover_in_generation(main, covers, manga_id, &webp, generation).await {
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
    covers: CoverStore,
    mangadex: Arc<MangaDexClient>,
    suwayomi: crate::suwayomi::SuwayomiClient,
    interval_secs: u64,
    batch: i64,
    shutdown: tokio::sync::watch::Receiver<bool>,
    source_budgets: Arc<SourceBudgets>,
) {
    // Panic-supervised (crate::task::supervise): an unhandled panic in a crawl used to
    // end cover caching silently for the process lifetime. The factory re-clones the
    // loop's state on each restart. All captured handles are cheap to clone (pools are
    // Arc-backed, the client is Arc).
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
                interval_secs,
                batch,
                shutdown.clone(),
                source_budgets.clone(),
            )
        },
    ));
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    main: Database,
    covers: CoverStore,
    mangadex: Arc<MangaDexClient>,
    suwayomi: crate::suwayomi::SuwayomiClient,
    interval_secs: u64,
    batch: i64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    source_budgets: Arc<SourceBudgets>,
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
                // Yield to a manual full crawl or another server's tick if one is in
                // flight (don't stack two MangaDex crawls); we'll pick up the remainder
                // next tick. The cover-crawl lease covers both passes of the tick.
                let Some(_crawl) = crate::job_lease::JobLease::try_take(
                    &main,
                    crate::job_lease::JobFamily::CoverCrawl,
                )
                .await
                else {
                    continue;
                };
                match pending_cover_count(&main).await {
                    Ok(0) => { /* nothing to do this tick */ }
                    Ok(_) => crawl_uncached_covers(&main, &covers, &mangadex, &suwayomi, Some(batch), &source_budgets, &warm_shutdown).await,
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
                                &source_budgets,
                            )
                            .await;
                        }
                        Err(e) => { let e = crate::db::observed_read_database_error(e); tracing::warn!(error = %e, "suwayomi cover warmer: count failed"); },
                    }
                }
                // The cover ranking and the unpicked-cover prune resume from their
                // cursors here, a bounded slice per tick; each skips while an admin run
                // or another server holds its lease.
                if !*warm_shutdown.borrow() {
                    let clients = crate::cover_choice::Clients {
                        main: &main,
                        covers: &covers,
                        mangadex: &mangadex,
                        suwayomi: &suwayomi,
                        budgets: &source_budgets,
                        shutdown: &warm_shutdown,
                        waiting: false,
                    };
                    match crate::cover_choice::rank_pass(&clients, Some(1)).await {
                        Ok(0) => {}
                        Ok(changed) => tracing::info!(changed, "cover ranking: covers moved to their ranked source"),
                        Err(e) => { let e = crate::db::observed_database_error(e); tracing::warn!(error = %e, "cover ranking: pass failed"); },
                    }
                    match crate::cover_choice::prune_pass(&main, &warm_shutdown, Some(20)).await {
                        Ok(0) => {}
                        Ok(removed) => tracing::info!(removed, "cover prune: unpicked source covers removed"),
                        Err(e) => { let e = crate::db::observed_database_error(e); tracing::warn!(error = %e, "cover prune: pass failed"); },
                    }
                }
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
    use crate::cover_store::test_support::{legacy_pool, store, store_with_legacy, TestDir};
    use image::{DynamicImage, RgbImage};
    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

    async fn publication_pools() -> (Database, CoverStore, TestDir) {
        let (main, covers, dir) = warm_test_pools().await;
        sqlx::query(
            "INSERT INTO work(id,created_at,updated_at) VALUES ('w_publication',now(),now())",
        )
        .execute(&main)
        .await
        .unwrap();
        (main.foreground(), covers, dir)
    }

    async fn stored_work(main: &Database, covers: &CoverStore, work_id: &str) -> (Vec<u8>, i64) {
        let (master, _) = covers
            .read_master(main, work_id, None)
            .await
            .unwrap()
            .unwrap();
        (master.bytes, master.version.unwrap())
    }

    async fn seed_version(main: &Database, covers: &CoverStore, version: i64) {
        let mut staged = covers.stage(b"initial").await.unwrap();
        let mut tx = main.begin().await.unwrap();
        let _guards = lock_assets_and_files(&mut tx, &["w_publication"], &[staged.sha])
            .await
            .unwrap();
        crate::locks::lock_works(&mut tx, &["w_publication"])
            .await
            .unwrap();
        covers.place(&mut staged).await.unwrap();
        cover_store::put_master(&mut tx, "w_publication", &staged, Some(version))
            .await
            .unwrap();
        publish_work_cover_version(&mut tx, "w_publication", version)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    #[test]
    fn stale_cover_repairs_are_deduplicated_bounded_and_expire() {
        let now = std::time::Instant::now();
        let mut recent = RecentCoverRepairs::default();
        assert!(recent.reserve("hot", now));
        assert!(!recent.reserve("hot", now + Duration::from_secs(299)));
        for i in 1..1024 {
            assert!(recent.reserve(&format!("work-{i}"), now));
        }
        assert!(!recent.reserve("overflow", now));
        assert_eq!(recent.0.len(), 1024);
        assert!(recent.reserve("hot", now + Duration::from_secs(300)));
        assert_eq!(recent.0.len(), 1);
    }

    #[tokio::test]
    async fn stale_cover_repair_rechecks_under_publication_locks_and_is_a_noop_when_equal() {
        let (main, _covers, dir) = publication_pools().await;
        let legacy = legacy_pool().await;
        let covers = store_with_legacy(&dir, legacy.clone(), cover_store::CopyCursor::default());
        sqlx::query("UPDATE work SET cover_cached_version=7 WHERE id='w_publication'")
            .execute(&main)
            .await
            .unwrap();
        sqlx::query("INSERT INTO work_cover_blob VALUES ('w_publication',$1,8,'now')")
            .bind(b"version8".as_slice())
            .execute(&legacy)
            .await
            .unwrap();
        let mut held = main.begin().await.unwrap();
        let guard = lock_asset(&mut held, "w_publication").await.unwrap();
        crate::locks::lock_works(&mut held, &["w_publication"])
            .await
            .unwrap();
        let writer = main.clone();
        let cache = covers.clone();
        let repair = tokio::spawn(async move {
            repair_work_cover_version(&writer, &cache, "w_publication").await
        });
        wait_for_writer_lock(&main).await;
        // A later blob committed before the repair gets the guards must win.
        sqlx::query("UPDATE work_cover_blob SET version=9 WHERE work_id='w_publication'")
            .execute(&legacy)
            .await
            .unwrap();
        held.commit().await.unwrap();
        drop(guard);
        assert!(repair.await.unwrap().unwrap());
        let version: i64 =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id='w_publication'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert_eq!(version, 9);
        // Even an equal-value UPDATE is forbidden: this catches no-op writes to any
        // publication table, rather than merely checking that the values stayed equal.
        sqlx::raw_sql("CREATE FUNCTION reject_repair_update() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'unexpected no-op write'; END $$")
            .execute(&main).await.unwrap();
        for table in [
            "work",
            "feed_updates",
            "feed_series_updates",
            "browse_catalogue",
        ] {
            sqlx::raw_sql(&format!("CREATE TRIGGER reject_repair_update BEFORE UPDATE ON {table} FOR EACH STATEMENT EXECUTE FUNCTION reject_repair_update()"))
                .execute(&main).await.unwrap();
        }
        let mut held = main.begin().await.unwrap();
        let guard = lock_asset(&mut held, "w_publication").await.unwrap();
        assert!(!tokio::time::timeout(
            Duration::from_secs(2),
            repair_work_cover_version(&main, &covers, "w_publication")
        )
        .await
        .expect("equal pointer occupied the writer lane")
        .unwrap());
        held.rollback().await.unwrap();
        drop(guard);
        // Missing works and missing blobs are harmless, with no publication writes.
        assert!(!repair_work_cover_version(&main, &covers, "gone")
            .await
            .unwrap());
        sqlx::query("DELETE FROM work_cover_blob")
            .execute(&legacy)
            .await
            .unwrap();
        assert!(!repair_work_cover_version(&main, &covers, "w_publication")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn stale_cover_repair_never_replaces_a_newer_published_version() {
        let (main, _covers, dir) = publication_pools().await;
        let legacy = legacy_pool().await;
        let covers = store_with_legacy(&dir, legacy.clone(), cover_store::CopyCursor::default());
        sqlx::query("UPDATE work SET cover_cached_version=9 WHERE id='w_publication'")
            .execute(&main)
            .await
            .unwrap();
        sqlx::query("INSERT INTO work_cover_blob VALUES ('w_publication',$1,7,'now')")
            .bind(b"local-version7".as_slice())
            .execute(&legacy)
            .await
            .unwrap();
        let url = cover_path("w_publication", 9);
        sqlx::query("INSERT INTO feed_updates(work_id,title,latest_at,cover_url) VALUES ('w_publication','Cover',NOW(),$1)")
            .bind(&url).execute(&main).await.unwrap();
        sqlx::query("INSERT INTO feed_series_updates(work_id,reader_id,title,released_at,cover_url) VALUES ('w_publication','7','Cover',NOW(),$1)")
            .bind(&url).execute(&main).await.unwrap();
        sqlx::query("INSERT INTO browse_catalogue(work_id,reader_id,title,created_at,cover_url) VALUES ('w_publication','7','Cover',NOW(),$1)")
            .bind(&url).execute(&main).await.unwrap();

        // A server with an older local cache must skip even acquiring the guards.
        let mut held = main.begin().await.unwrap();
        let guard = lock_asset(&mut held, "w_publication").await.unwrap();
        assert!(!tokio::time::timeout(
            Duration::from_secs(2),
            repair_work_cover_version(&main, &covers, "w_publication"),
        )
        .await
        .expect("older blob occupied the writer lane")
        .unwrap());
        held.rollback().await.unwrap();
        drop(guard);

        // The first probe permits a repair, but another copy publishes version 9
        // while it is queued. Its own version 7 must not undo that publication.
        sqlx::query("UPDATE work SET cover_cached_version=6 WHERE id='w_publication'")
            .execute(&main)
            .await
            .unwrap();
        let mut held = main.begin().await.unwrap();
        let guard = lock_asset(&mut held, "w_publication").await.unwrap();
        crate::locks::lock_works(&mut held, &["w_publication"])
            .await
            .unwrap();
        let writer = main.clone();
        let cache = covers.clone();
        let repair = tokio::spawn(async move {
            repair_work_cover_version(&writer, &cache, "w_publication").await
        });
        wait_for_writer_lock(&main).await;
        publish_work_cover_version(&mut held, "w_publication", 9)
            .await
            .unwrap();
        held.commit().await.unwrap();
        drop(guard);
        assert!(!repair.await.unwrap().unwrap());
        let published: i64 =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id='w_publication'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert_eq!(published, 9);
        for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
            let published_url: String = sqlx::query_scalar(&format!(
                "SELECT cover_url FROM {table} WHERE work_id='w_publication'"
            ))
            .fetch_one(&main)
            .await
            .unwrap();
            assert_eq!(published_url, url, "{table}");
        }

        // Absent and older pointers still repair to the available local version.
        for published in [None, Some(6_i64)] {
            sqlx::query("UPDATE work SET cover_cached_version=$1 WHERE id='w_publication'")
                .bind(published)
                .execute(&main)
                .await
                .unwrap();
            assert!(repair_work_cover_version(&main, &covers, "w_publication")
                .await
                .unwrap());
            let repaired: i64 = sqlx::query_scalar(
                "SELECT cover_cached_version FROM work WHERE id='w_publication'",
            )
            .fetch_one(&main)
            .await
            .unwrap();
            assert_eq!(repaired, 7);
        }
    }

    #[tokio::test]
    async fn variant_lookup_compares_binary_master_and_never_returns_orphan_derivatives() {
        let (main, covers, _dir) = publication_pools().await;
        let bytes = [0, 255, 1, 0];
        for asset in ["w_publication", "s:7"] {
            if asset.starts_with("s:") {
                put_suwayomi_cover(&main, &covers, 7, &bytes).await.unwrap();
            } else {
                put_work_cover(&main, &covers, asset, &bytes).await.unwrap();
            }
            let (master, _) = covers
                .read_master(&main, asset, None)
                .await
                .unwrap()
                .unwrap();
            put_current_variant(&main, &covers, asset, 192, &master, b"small")
                .await
                .unwrap();
            assert_eq!(
                variant_of_master(&main, &covers, asset, 192, &master).await,
                Some(b"small".to_vec())
            );
            let wrong = cover_store::Master {
                bytes: vec![0, 255, 2, 0],
                sha: cover_store::sha256(&[0, 255, 2, 0]),
                ..master.clone()
            };
            assert_eq!(
                variant_of_master(&main, &covers, asset, 192, &wrong).await,
                None
            );
            assert_eq!(
                variant_of_master(&main, &covers, asset, 320, &master).await,
                None
            );
            // Leave the derivative indexed to check that the master existence guard matters.
            sqlx::query("DELETE FROM cover_object WHERE asset_key=$1 AND kind='master'")
                .bind(cover_store::index_key(asset))
                .execute(&main)
                .await
                .unwrap();
            assert_eq!(
                variant_of_master(&main, &covers, asset, 192, &master).await,
                None
            );
        }
    }

    #[tokio::test]
    async fn delayed_cover_misses_recheck_the_current_asset_and_work_anchor() {
        let (main, covers, _dir) = publication_pools().await;
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
        assert!(
            cover_store::row(&main, "w_publication", cover_store::Kind::Master)
                .await
                .unwrap()
                .is_none()
        );
        let mut staged = covers.stage(b"fresh-source").await.unwrap();
        let mut first = main.begin().await.unwrap();
        crate::engine_generation::shared(&mut first, None)
            .await
            .unwrap();
        let _asset = lock_assets_and_files(&mut first, &["s:7"], &[staged.sha])
            .await
            .unwrap();
        covers.place(&mut staged).await.unwrap();
        cover_store::put_master(&mut first, "s:7", &staged, None)
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
            get_suwayomi_cover(&main, &covers, 7).await.unwrap(),
            b"fresh-source"
        );
    }

    #[tokio::test]
    async fn overlapping_cover_backoffs_recompute_all_three_ladders_under_the_guard() {
        for kind in 0..3 {
            let (main, _covers, _dir) = publication_pools().await;
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
        let (main, covers, _dir) = publication_pools().await;
        seed_version(&main, &covers, 4102444800000).await;
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
        let (bytes, version) = stored_work(&main, &covers, "w_publication").await;
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
    async fn separate_cover_stores_mint_distinct_versions_under_the_publication_lock() {
        let (main, first_covers, _dir) = publication_pools().await;
        let second_dir = TestDir::new();
        let second_covers = store(&second_dir);
        seed_version(&main, &first_covers, 4102444800000).await;
        sqlx::query("UPDATE work SET cover_cached_version=4102444800001 WHERE id='w_publication'")
            .execute(&main)
            .await
            .unwrap();
        let points = crate::locks::pause_points::PausePoints::default();
        let armed = points.arm("cover-locked");
        let writer = main.clone();
        let cache = first_covers.clone();
        let first = tokio::spawn(COVER_PUBLICATION_PAUSE.scope(points, async move {
            put_work_cover(&writer, &cache, "w_publication", b"first").await
        }));
        tokio::time::timeout(Duration::from_secs(30), armed.reached())
            .await
            .unwrap();
        let second_points = crate::locks::pause_points::PausePoints::default();
        let second_locked = second_points.arm("cover-locked");
        let writer = main.clone();
        let cache = second_covers.clone();
        let second = tokio::spawn(COVER_PUBLICATION_PAUSE.scope(second_points, async move {
            put_work_cover(&writer, &cache, "w_publication", b"second").await
        }));
        wait_for_writer_lock(&main).await;
        armed.release();
        first.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(30), second_locked.reached())
            .await
            .unwrap();
        assert_eq!(
            stored_work(&main, &first_covers, "w_publication").await,
            (b"first".to_vec(), 4102444800002)
        );
        second_locked.release();
        second.await.unwrap().unwrap();

        // One index serves both stores; the old file remains immutable in the first dir.
        assert_eq!(
            first_covers
                .read_object(&cover_store::sha256(b"first"))
                .await
                .unwrap(),
            Some(b"first".to_vec())
        );
        assert_eq!(
            stored_work(&main, &second_covers, "w_publication").await,
            (b"second".to_vec(), 4102444800003)
        );
        let published: i64 =
            sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id='w_publication'")
                .fetch_one(&main)
                .await
                .unwrap();
        assert_eq!(published, 4102444800003);
    }

    #[tokio::test]
    async fn a_delayed_crawl_fill_cannot_replace_an_admin_upload() {
        for mangadex_anchor in [true, false] {
            let (main, covers, _dir) = publication_pools().await;
            sqlx::query("UPDATE work SET cover_file_name='cover.jpg' WHERE id='w_publication'")
                .execute(&main)
                .await
                .unwrap();
            let (source_type, source_key) = if mangadex_anchor {
                ("mangadex", "md-uuid")
            } else {
                ("suwayomi", "7")
            };
            sqlx::query("INSERT INTO source_series(id,work_id,source_type,source_key,created_at) VALUES ('ss_cover','w_publication',$1,$2,now())")
                .bind(source_type).bind(source_key).execute(&main).await.unwrap();
            if !mangadex_anchor {
                insert_series(&main, 7, Some(&thumb_path(7))).await;
            }
            let origin = crate::suwayomi::testsrv::spawn(
                crate::suwayomi::testsrv::jpeg(300, 450),
                Duration::ZERO,
            )
            .await;
            let suwayomi =
                crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, None);
            let mangadex = MangaDexClient::new("cover-crawl-race-test", 5.0, 40.0)
                .with_bases(&origin.base_url, &origin.base_url);
            mangadex.set_enabled(mangadex_anchor);
            let points = crate::locks::pause_points::PausePoints::default();
            let armed = points.arm("cover-before-lock");
            let writer = main.clone();
            let cache = covers.clone();
            let crawl = tokio::spawn(COVER_PUBLICATION_PAUSE.scope(points, async move {
                crawl_uncached_covers(
                    &writer,
                    &cache,
                    &mangadex,
                    &suwayomi,
                    Some(10),
                    &SourceBudgets::default(),
                    &tokio::sync::watch::channel(false).1,
                )
                .await;
            }));
            tokio::time::timeout(Duration::from_secs(30), armed.reached())
                .await
                .unwrap();
            // The fetch and encoding finished, but publication has not taken its locks.
            put_uploaded_work_cover(&main, &covers, "w_publication", b"admin-upload")
                .await
                .unwrap();
            let uploaded = stored_work(&main, &covers, "w_publication").await;
            armed.release();
            tokio::time::timeout(Duration::from_secs(30), crawl)
                .await
                .unwrap()
                .unwrap();
            let stored = stored_work(&main, &covers, "w_publication").await;
            assert_eq!(stored, uploaded, "{source_type} crawl replaced the upload");
            let published: i64 = sqlx::query_scalar(
                "SELECT cover_cached_version FROM work WHERE id='w_publication'",
            )
            .fetch_one(&main)
            .await
            .unwrap();
            assert_eq!(published, uploaded.1);
            assert_eq!(origin.hits.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn a_delayed_derivative_cannot_publish_after_master_replacement() {
        let (main, covers, _dir) = publication_pools().await;
        put_work_cover(&main, &covers, "w_publication", b"old")
            .await
            .unwrap();
        let (old, _) = covers
            .read_master(&main, "w_publication", None)
            .await
            .unwrap()
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
                &old,
                b"old-small",
            )
            .await
        });
        wait_for_writer_lock(&main).await;
        armed.release();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert!(
            get_cover_variant(&main, &covers, "w_publication", CARD_VARIANT_WIDTH)
                .await
                .is_none()
        );
    }

    /// The conditional master writers publish only over the master they captured:
    /// a lazy miss expects no master, recompression expects the bytes it re-encoded.
    #[tokio::test]
    async fn a_stale_fetch_or_recompression_cannot_replace_a_newer_master() {
        let (main, covers, _dir) = publication_pools().await;
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
            let blob = stored_work(&main, &covers, "w_publication").await;
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
        let (main, covers, _dir) = publication_pools().await;
        let old_art = process_cover(&noisy_png(400, 600)).unwrap();
        let variant = downscale_variant(&old_art, CARD_VARIANT_WIDTH)
            .unwrap()
            .expect("a 400x600 master is wider than a card");
        put_work_cover(&main, &covers, "w_publication", &old_art)
            .await
            .unwrap();
        put_suwayomi_cover(&main, &covers, 7, &old_art)
            .await
            .unwrap();
        for asset in ["w_publication", "s:7"] {
            let (master, _) = covers
                .read_master(&main, asset, None)
                .await
                .unwrap()
                .unwrap();
            put_current_variant(&main, &covers, asset, CARD_VARIANT_WIDTH, &master, &variant)
                .await
                .unwrap();
            assert!(
                get_cover_variant(&main, &covers, asset, CARD_VARIANT_WIDTH)
                    .await
                    .is_some(),
                "precondition: the derivative is stored"
            );
        }

        // New art for the same asset must invalidate the old card in both families.
        let new_art = process_cover(&noisy_png(500, 750)).unwrap();
        put_work_cover(&main, &covers, "w_publication", &new_art)
            .await
            .unwrap();
        put_suwayomi_cover(&main, &covers, 7, &new_art)
            .await
            .unwrap();
        for asset in ["w_publication", "s:7"] {
            assert!(
                get_cover_variant(&main, &covers, asset, CARD_VARIANT_WIDTH)
                    .await
                    .is_none(),
                "a re-saved {asset} master must drop its derivatives"
            );
        }
    }

    #[tokio::test]
    async fn put_work_cover_stores_blob_and_flips_version() {
        let (main, covers, _dir) = warm_test_pools().await;
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

        // The indexed file and the work's version pointer are published together.
        let (blob, blob_ver) = stored_work(&main, &covers, "w_test").await;
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
                source_id: "suwayomi".into(),
            },
            RepairCoverSource::Suwayomi {
                path: "/second".into(),
                source_id: "suwayomi".into(),
            },
        ];
        let good = noisy_png(300, 450);
        let mut visited = Vec::new();
        let (winner, webp) = process_repair_sources(sources.clone(), |source| {
            visited.push(source.clone());
            std::future::ready(match source {
                RepairCoverSource::MangaDex { .. } => Some(b"not an image".to_vec()),
                RepairCoverSource::Suwayomi { path, .. } if path == "/second" => Some(good.clone()),
                _ => None,
            })
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(visited, sources);
        assert_eq!(winner, sources[2]);
        assert_eq!(
            image::load_from_memory(&webp).unwrap().dimensions(),
            (300, 450)
        );
    }

    #[tokio::test]
    async fn repair_uses_suwayomi_without_mangadex_or_cached_metadata_and_keeps_old_cover_on_failure(
    ) {
        let (main, covers, _dir) = warm_test_pools().await;
        sqlx::query("INSERT INTO work (id, created_at, updated_at) VALUES ('w_repair', '2026-01-01', '2026-01-01')")
            .execute(&main).await.unwrap();
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_key, created_at) VALUES ('ss_repair', 'w_repair', 'suwayomi', '7', '2026-01-01')")
            .execute(&main).await.unwrap();
        assert_eq!(
            repair_cover_sources(&main, "w_repair").await.unwrap(),
            vec![RepairCoverSource::Suwayomi {
                path: "/api/v1/manga/7/thumbnail".into(),
                source_id: "suwayomi".into(),
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
        let before = stored_work(&main, &covers, "w_repair").await;
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
        let after = stored_work(&main, &covers, "w_repair").await;
        assert_eq!(
            after, before,
            "failed sources must not replace the stored cover"
        );
    }

    #[tokio::test]
    async fn with_mangadex_off_a_cover_repair_uses_the_suwayomi_sources() {
        let (main, covers, _dir) = warm_test_pools().await;
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
        let (main, covers, _dir) = warm_test_pools().await;
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

        crawl_uncached_covers(
            &main,
            &covers,
            &mangadex,
            &suwayomi,
            Some(10),
            &SourceBudgets::default(),
            &tokio::sync::watch::channel(false).1,
        )
        .await;

        let cached: Vec<String> = sqlx::query_scalar("SELECT work_id FROM cover_object WHERE kind='master' AND work_id IS NOT NULL ORDER BY work_id")
            .fetch_all(&main)
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

    /// Keep the directory alive alongside the migrated database and file store.
    async fn warm_test_pools() -> (Database, CoverStore, TestDir) {
        use crate::database::TestPoolOptions;
        let main = TestPoolOptions::new()
            .max_connections(4)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations_pg")
            .run(main.write())
            .await
            .unwrap();
        let dir = TestDir::new();
        let covers = store(&dir);
        (main, covers, dir)
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

    /// Only series with a usable thumbnail and no indexed master are candidates,
    /// newest first, capped.
    #[tokio::test]
    async fn uncached_ids_excludes_cached_and_thumbless_series() {
        let (main, covers, _dir) = warm_test_pools().await;
        insert_series(&main, 10, Some("/api/v1/manga/10/thumbnail")).await;
        insert_series(&main, 11, Some("/api/v1/manga/11/thumbnail")).await;
        insert_series(&main, 12, None).await; // no thumbnail → not a candidate
        insert_series(&main, 13, Some("")).await; // empty thumbnail → not a candidate
        insert_series(&main, 14, Some("/api/v1/manga/14/thumbnail")).await;
        put_suwayomi_cover(&main, &covers, 11, b"already-warm")
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

    /// The warmer must populate the numeric assets the reader actually reads;
    /// warming canonical work covers alone leaves discovery covers cold.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn warmer_populates_the_cache_the_reader_reads() {
        let (main, covers, _dir) = warm_test_pools().await;
        for id in [21, 22, 23] {
            insert_series(&main, id, Some(&format!("/api/v1/manga/{id}/thumbnail"))).await;
        }
        // 22 is already warm; the warmer must not refetch it.
        put_suwayomi_cover(&main, &covers, 22, b"already-warm")
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
        let (saved, failed) = warm_suwayomi_covers(
            &covers,
            &main,
            &client,
            500,
            &cursor,
            &rx,
            &SourceBudgets::default(),
        )
        .await;
        assert_eq!((saved, failed), (2, 0), "both uncached series warmed");

        // Exactly what `serve_suwayomi_cover` reads on the request path.
        for id in [21, 23] {
            let blob = get_suwayomi_cover(&main, &covers, id)
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
            get_suwayomi_cover(&main, &covers, 22).await.as_deref(),
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
            warm_suwayomi_covers(
                &covers,
                &main,
                &client,
                500,
                &cursor,
                &rx,
                &SourceBudgets::default()
            )
            .await,
            (0, 0),
            "a fully warm cache does no work"
        );
    }

    /// The warmer must never occupy enough of the shared cover-fetch pool to make an
    /// on-demand request fail fast — that would trade the outage for a different one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn warmer_leaves_headroom_for_on_demand_requests() {
        let (main, covers, _dir) = warm_test_pools().await;
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
                warm_suwayomi_covers(
                    &covers,
                    &main,
                    &client,
                    40,
                    &cursor,
                    &rx,
                    &SourceBudgets::default(),
                )
                .await
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
        let (main, covers, _dir) = warm_test_pools().await;
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
        let (saved, failed) = warm_suwayomi_covers(
            &covers,
            &main,
            &client,
            30,
            &cursor,
            &rx,
            &SourceBudgets::default(),
        )
        .await;
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
        let (main, covers, _dir) = warm_test_pools().await;
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
            let (saved, failed) = warm_suwayomi_covers(
                &covers,
                &main,
                &client,
                2,
                &cursor,
                &rx,
                &SourceBudgets::default(),
            )
            .await;
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
        let (_, failed) = warm_suwayomi_covers(
            &covers,
            &main,
            &client,
            2,
            &cursor,
            &rx,
            &SourceBudgets::default(),
        )
        .await;
        assert_eq!(failed, 0, "backed-off ids are not re-fetched on the wrap");

        // Once the backoff elapses the wrap behaves as before: newest window first.
        sqlx::query("UPDATE suwayomi_cover_issue SET retry_after = '2020-01-01T00:00:00+00:00'")
            .execute(&main)
            .await
            .unwrap();
        let (_, failed) = warm_suwayomi_covers(
            &covers,
            &main,
            &client,
            2,
            &cursor,
            &rx,
            &SourceBudgets::default(),
        )
        .await;
        assert_eq!(failed, 2, "wrapped and retried the newest ids");
        assert_eq!(cursor.load(AtomicOrdering::Relaxed), 5);
    }

    /// The coverage gauge must be a SET DIFFERENCE, not `series - COUNT(*)`.
    /// The index also holds covers the request path materialized for Browse
    /// manga that were never enrolled in `suwayomi_series`; counting those as coverage
    /// drives `pending` to 0 while real candidates remain, and the drainer then skips the
    /// warmer forever.
    #[tokio::test]
    async fn pending_count_ignores_blobs_for_unenrolled_manga() {
        let (main, covers, _dir) = warm_test_pools().await;
        for id in [31, 32] {
            insert_series(&main, id, Some(&format!("/api/v1/manga/{id}/thumbnail"))).await;
        }
        // Three Browse covers cached on the request path; none is an enrolled series.
        for id in [900, 901, 902] {
            put_suwayomi_cover(&main, &covers, id, b"browse")
                .await
                .unwrap();
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
        let (main, covers, _dir) = warm_test_pools().await;
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

        let (saved, failed) = warm_suwayomi_covers(
            &covers,
            &main,
            &client,
            10,
            &cursor,
            &rx,
            &SourceBudgets::default(),
        )
        .await;
        assert_eq!((saved, failed), (0, 1));
        let (reason, permanent, attempts, _) = issue_row(&main, 7).await.expect("marker recorded");
        assert_eq!((reason.as_str(), permanent, attempts), ("data_uri", 1, 1));
        let after_first = origin.hits.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(after_first, 1);

        // Next cycle: the candidate is gone, so not one byte leaves the box.
        let (saved, failed) = warm_suwayomi_covers(
            &covers,
            &main,
            &client,
            10,
            &cursor,
            &rx,
            &SourceBudgets::default(),
        )
        .await;
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
        let (main, covers, _dir) = warm_test_pools().await;
        // Below SOURCE_BACKOFF_THRESHOLD, so this exercises the per-cover ladder only.
        insert_series(&main, 8, Some(&thumb_path(8))).await;
        let origin = crate::suwayomi::testsrv::spawn_error(503, "upstream is sulking").await;
        let client =
            crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, Some("1".into()));
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let cursor = AtomicI64::new(WARM_CURSOR_START);

        assert_eq!(
            warm_suwayomi_covers(
                &covers,
                &main,
                &client,
                10,
                &cursor,
                &rx,
                &SourceBudgets::default()
            )
            .await,
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
            warm_suwayomi_covers(
                &covers,
                &main,
                &client,
                10,
                &cursor,
                &rx,
                &SourceBudgets::default()
            )
            .await,
            (0, 0)
        );
        assert_eq!(origin.hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Window elapsed (backdated, so the test doesn't sleep for 30 minutes).
        sqlx::query("UPDATE suwayomi_cover_issue SET retry_after = '2020-01-01T00:00:00+00:00'")
            .execute(&main)
            .await
            .unwrap();
        assert_eq!(
            warm_suwayomi_covers(
                &covers,
                &main,
                &client,
                10,
                &cursor,
                &rx,
                &SourceBudgets::default()
            )
            .await,
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
        let (main, covers, _dir) = warm_test_pools().await;
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
            warm_suwayomi_covers(
                &covers,
                &main,
                &client,
                10,
                &cursor,
                &rx,
                &SourceBudgets::default()
            )
            .await,
            (1, 0)
        );
        assert!(
            issue_row(&main, 9).await.is_none(),
            "success clears the marker"
        );
        assert!(get_suwayomi_cover(&main, &covers, 9).await.is_some());
    }

    /// Self-healing #2: a PERMANENT marker is void the moment the extension re-fetches
    /// the manga — that is exactly when a `data:` thumbnail can become a real URL, and
    /// it must not wait out the 30-day window.
    #[tokio::test]
    async fn a_re_fetched_series_voids_even_a_permanent_marker() {
        let (main, _covers, _dir) = warm_test_pools().await;
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
        let (main, covers, _dir) = warm_test_pools().await;
        for id in 41..=43 {
            insert_series_from(&main, id, Some(&thumb_path(id)), "philia", None).await;
        }
        let origin = crate::suwayomi::testsrv::spawn_error(403, "blocked by WAF").await;
        let client =
            crate::suwayomi::SuwayomiClient::new(origin.base_url.clone(), None, Some("1".into()));
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let cursor = AtomicI64::new(WARM_CURSOR_START);

        assert_eq!(
            warm_suwayomi_covers(
                &covers,
                &main,
                &client,
                10,
                &cursor,
                &rx,
                &SourceBudgets::default()
            )
            .await,
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
            warm_suwayomi_covers(
                &covers,
                &main,
                &client,
                10,
                &cursor,
                &rx,
                &SourceBudgets::default()
            )
            .await,
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
            warm_suwayomi_covers(
                &covers,
                &main,
                &client,
                10,
                &cursor,
                &rx,
                &SourceBudgets::default()
            )
            .await,
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
        let (main, covers, _dir) = warm_test_pools().await;
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
            warm_suwayomi_covers(
                &covers,
                &main,
                &dead,
                10,
                &cursor,
                &rx,
                &SourceBudgets::default()
            )
            .await,
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
            warm_suwayomi_covers(
                &covers,
                &main,
                &client,
                10,
                &cursor,
                &rx,
                &SourceBudgets::default()
            )
            .await,
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
