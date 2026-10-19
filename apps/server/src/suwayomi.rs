//! Server-side federation client for a Suwayomi/Tachidesk server.
//!
//! Mirrors the TS `SuwayomiBackend` adapter: it talks Suwayomi's real GraphQL and
//! returns raw Suwayomi shapes, which `graphql` maps onto the Komika contract.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::source_health::{Subject, Surface};
use crate::telemetry::BodySource;
use crate::upstream_cache::TtlMemo;

/// How long after asking the engine to download a garbled thumbnail again the same
/// manga may ask again on its own. The repair fetches the manga's details from the
/// source, so a cover the source itself serves damaged must not do that on every view.
const THUMBNAIL_REPAIR_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);

/// Engine thumbnail requests, one at a time per manga.
///
/// Suwayomi downloads a thumbnail into one temp file per manga
/// (`/tmp/Tachidesk/thumbnails/<id>.tmp`). Two requests for the same manga at once
/// write into that file together, and the saved copy (`downloads/thumbnails/<id>.jpg`,
/// kept for good once the manga is in the library) is a mix of both: it still has its
/// start and end markers and decodes, with a band of garbage. Enrolment asks for the
/// cover twice at once (its pHash and the cover store), so the server itself started
/// that race. Every thumbnail request of this client takes the manga's turn first.
#[derive(Default)]
struct ThumbnailTurns {
    turns:
        std::sync::Mutex<std::collections::HashMap<i64, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    /// When each manga last asked the engine for a fresh download by itself.
    repaired: std::sync::Mutex<std::collections::HashMap<i64, std::time::Instant>>,
}

impl ThumbnailTurns {
    async fn take(&self, manga_id: i64) -> tokio::sync::OwnedMutexGuard<()> {
        let turn = {
            let mut turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
            turns.retain(|_, turn| turn.strong_count() > 0);
            match turns.get(&manga_id).and_then(std::sync::Weak::upgrade) {
                Some(turn) => turn,
                None => {
                    let turn = Arc::new(tokio::sync::Mutex::new(()));
                    turns.insert(manga_id, Arc::downgrade(&turn));
                    turn
                }
            }
        };
        turn.lock_owned().await
    }

    /// Whether `manga_id` may ask for a fresh download now (and note that it did).
    fn may_repair(&self, manga_id: i64) -> bool {
        let now = std::time::Instant::now();
        let mut repaired = self.repaired.lock().unwrap_or_else(|e| e.into_inner());
        repaired.retain(|_, at| now.duration_since(*at) < THUMBNAIL_REPAIR_EVERY);
        if repaired.contains_key(&manga_id) {
            return false;
        }
        repaired.insert(manga_id, now);
        true
    }
}

/// Max simultaneous upstream COVER fetches (Suwayomi -> source CDN) across the whole
/// process. Covers are the only Suwayomi call a browser `<img>` blocks on, and the
/// engine proxies each one to a third-party CDN, so an unbounded fan-out (a 30-card
/// grid on a cold cache) queues behind the engine's own connection pool and every
/// request pays the full timeout. Bounding it makes the tail deterministic.
///
/// # On-demand headroom — the exact accounting
///
/// On-demand fetches use `try_acquire` and never queue; background ones wait. Tokio's
/// semaphore hands a released permit to a queued waiter rather than back to the counter,
/// so headroom cannot come from fairness — it comes from capping how many permits
/// background work can ever hold at once:
///
/// | holder                                        | max concurrent |
/// |-----------------------------------------------|----------------|
/// | cover warmer / crawl (mutually exclusive, both under the drainer's `inflight` flag) | [`WARM_COVER_CONCURRENCY`] |
/// | detached materializations                     | [`BG_MATERIALIZE_CONCURRENCY`] |
///
/// So the guaranteed floor is `COVER_FETCH_CONCURRENCY - (WARM + BG)` = 5 concurrent
/// on-demand fetches, NOT `COVER_FETCH_CONCURRENCY - WARM`. One more consumer is
/// unaccounted: `cover_bytes` on the enrol paths in `graphql` (dedup pHash) also waits
/// patiently on this pool, one permit per in-flight admin/ingest request — sequential in
/// practice, but nothing structurally bounds it. If that ever becomes concurrent, it must
/// take the background pool instead.
pub const COVER_FETCH_CONCURRENCY: usize = 12;

/// Background cover-warmer share of [`COVER_FETCH_CONCURRENCY`]. Small on purpose.
pub const WARM_COVER_CONCURRENCY: usize = 3;

/// Max concurrent *background* materializations kicked off by a saturated on-demand
/// request (`serve_suwayomi_cover`'s fast-path miss). Bounded so a burst of misses
/// can't spawn thousands of detached fetch tasks.
const BG_MATERIALIZE_CONCURRENCY: usize = 4;

/// The headroom invariant above, enforced at compile time: background work must never be
/// able to hold every permit, or an on-demand request could be refused indefinitely.
const _: () =
    assert!(WARM_COVER_CONCURRENCY + BG_MATERIALIZE_CONCURRENCY < COVER_FETCH_CONCURRENCY);

/// Request timeout for a COVER fetch — deliberately far below the 30 s chapter-scan
/// timeout on the shared client. A cover is on the critical path of a page render:
/// failing fast (and retryably) beats a 29 s hang that the browser reports as a broken
/// image anyway. The scanner keeps the long timeout: a chapter-list fetch is background
/// work where patience is cheap and a retry is expensive.
const COVER_TIMEOUT_SECS: u64 = 8;

/// Hard cap on a single cover source body. Suwayomi thumbnails average ~0.8 MB; a few
/// MB is legitimate, 24 MB is not. Streamed (see [`read_capped`]) so we bail on the
/// first chunk that crosses the line instead of buffering a hostile body.
pub const MAX_COVER_SOURCE_BYTES: usize = 24 * 1024 * 1024;

/// Max concurrent in-flight PAGE fetches to Suwayomi. Pages are the read critical path
/// and arrive in bursts (a chapter is tens of pages), so this is more generous than the
/// cover pool — but still bounded, so a slow source (Suwayomi proxies to scraped sites,
/// often via FlareSolverr, which routinely stalls) can't pile unbounded fetches onto the
/// engine and starve the scanner. A SEPARATE pool from covers so page bursts and cover
/// demand never evict each other.
pub const PAGE_FETCH_CONCURRENCY: usize = 16;

/// How long a page fetch will WAIT for a pool permit before shedding with a retryable
/// 503. Covers fail fast (a background warmer converges out-of-band), but a page has no
/// such materializer — a hard failure is a visibly broken page needing a manual tap — so
/// we ride out a transient burst briefly instead of breaking. Bounded so saturation
/// degrades to a short wait, never the 30 s pile-up the unbounded path allowed.
const PAGE_ACQUIRE_WAIT_SECS: u64 = 6;

/// Request timeout for a PAGE fetch. Below the 30 s scan timeout (a hung page must fail
/// retryably, not freeze a slot for 30 s) but above the 8 s cover timeout — a
/// full-resolution page over a slow source legitimately takes longer than a thumbnail.
const PAGE_TIMEOUT_SECS: u64 = 20;

/// Re-opening a chapter should reuse its page list and immutable engine owner
/// without competing with cold source fetches. Viewer gates remain per request.
const READER_MEMO_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const READER_MEMO_CAP: usize = 4096;

/// Hard cap on a single page source body, mirroring the image Worker's `MAX_IMAGE_BYTES`.
/// Streamed via [`read_capped`] so a hostile/oversized body is refused, not buffered.
pub const MAX_PAGE_SOURCE_BYTES: usize = 32 * 1024 * 1024;

/// Why an on-demand cover fetch didn't produce bytes.
#[derive(Debug)]
pub enum CoverFetchError {
    /// The bounded cover-fetch pool is saturated. Callers should return immediately
    /// (placeholder / retry-later) rather than queue — queueing is what produced the
    /// 22 s p90.
    Busy,
    /// A real upstream failure (network, non-2xx, over-cap or truncated body).
    Upstream(anyhow::Error),
}

impl std::fmt::Display for CoverFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "cover fetch pool saturated"),
            Self::Upstream(e) => write!(f, "{e}"),
        }
    }
}

/// Why an on-demand page fetch didn't produce bytes. Mirrors [`CoverFetchError`] but
/// kept distinct: pages use their own pool/timeout and a saturated page is handled
/// differently (a short wait, then a retryable 503) from a saturated cover (fail fast).
#[derive(Debug)]
pub enum PageFetchError {
    /// The bounded page-fetch pool stayed saturated past [`PAGE_ACQUIRE_WAIT_SECS`].
    /// Callers should return a retryable status rather than queue indefinitely.
    Busy,
    /// A real upstream failure (network, non-2xx, over-cap or truncated body).
    Upstream(anyhow::Error),
}

impl std::fmt::Display for PageFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "page fetch pool saturated"),
            Self::Upstream(e) => write!(f, "{e}"),
        }
    }
}

/// Where a fetched value actually came from — the fresh source, or Suwayomi's local DB.
///
/// WHY THIS EXISTS (F11). Every fetch path in this client falls back to a plain
/// `manga(id:)` / `chapters(condition:)` QUERY when its `fetch*` MUTATION fails. Those
/// queries read Suwayomi's own database, so the call returns `Ok` with stale (or empty)
/// data and the caller cannot tell the difference. That is CORRECT for the reader — never
/// hard-fail a user over a transient upstream blip — and WRONG for the scanner, whose
/// entire job is detecting change: a cached list can't contain a chapter we haven't
/// already seen, so "no new chapters" from it is not a finding, and recording it as a
/// successful scan makes "this source is healthy" and "this source is completely broken"
/// indistinguishable.
///
/// Measured: `en.suryascans` (now rebranded "Genz Toons") 404s on every chapter fetch, yet
/// all 209 of its series reported `consecutive_failures = 0` with 0 chapters each, and
/// **zero** failures were recorded across all 14,098 series in the library. That number
/// was not a clean bill of health; it was this fallback.
///
/// So the fallback stays, and the *provenance* travels with the value. The reader ignores
/// it; the scanner treats [`Self::CachedFallback`] as an upstream failure
/// (`scanner::scan_due`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// The `fetch*` mutation succeeded: Suwayomi went out to the source site and this is
    /// what the site said (including "nothing", which is a real answer).
    Upstream,
    /// The mutation failed and this value was read from Suwayomi's LOCAL database. It may
    /// be stale, or empty because we never managed to fetch the series at all.
    CachedFallback,
}

impl Provenance {
    /// The weaker of two provenances — `Upstream` only if BOTH halves were fresh. Used
    /// where one call composes several fetches.
    pub fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Upstream, Self::Upstream) => Self::Upstream,
            _ => Self::CachedFallback,
        }
    }

    /// True when the value came out of Suwayomi's cache rather than the source.
    pub fn is_cached_fallback(self) -> bool {
        matches!(self, Self::CachedFallback)
    }
}

/// Read a response body into memory, streamed, refusing anything past `cap` bytes.
/// `Response::bytes()` is unbounded (only the request timeout bounds it), so a huge or
/// hostile body could otherwise balloon the process. Mirrors `mangadex::read_capped`.
async fn read_capped(mut res: reqwest::Response, cap: usize) -> Result<Vec<u8>> {
    if let Some(len) = res.content_length() {
        if len > cap as u64 {
            return Err(anyhow!("response body exceeds cap ({len} > {cap})"));
        }
    }
    let mut out: Vec<u8> =
        Vec::with_capacity(res.content_length().unwrap_or(0).min(1 << 20) as usize);
    while let Some(chunk) = res.chunk().await? {
        if out.len() + chunk.len() > cap {
            return Err(anyhow!("response body exceeds cap ({cap})"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Wait up to `wait` for a page-pool slot, counting the outcome in `lane`. Only a
/// wait that obtains a slot adds wait time, as in the other waiting lanes.
async fn acquire_page_slot(
    lane: &crate::telemetry::LaneStats,
    slots: &Arc<Semaphore>,
    wait: std::time::Duration,
) -> std::result::Result<OwnedSemaphorePermit, PageFetchError> {
    let waiting = std::time::Instant::now();
    match tokio::time::timeout(wait, slots.clone().acquire_owned()).await {
        Ok(Ok(permit)) => {
            lane.waited(waiting.elapsed());
            lane.admitted();
            Ok(permit)
        }
        // Semaphore closed (shutdown) — treat as an upstream failure, not a retry.
        Ok(Err(e)) => Err(PageFetchError::Upstream(anyhow!(e))),
        // Waited past the cap without a slot — shed, retryably.
        Err(_) => {
            lane.refuse();
            Err(PageFetchError::Busy)
        }
    }
}

/// `read_capped` fails either in transport (a `reqwest::Error`) or at its cap.
fn count_oversize(source: BodySource, error: &anyhow::Error) {
    if error.downcast_ref::<reqwest::Error>().is_none() {
        crate::telemetry::MEDIA.body_check(source, crate::telemetry::BodyCheck::TooLarge);
    }
}

/// Retain only a bounded prefix of an upstream error. A displayed 160-character
/// snippet must not first allocate an arbitrarily large error document.
async fn error_snippet(mut res: reqwest::Response) -> String {
    const CAP: usize = 4096;
    let mut bytes = Vec::new();
    while bytes.len() < CAP {
        match res.chunk().await {
            Ok(Some(chunk)) => {
                bytes.extend_from_slice(&chunk[..chunk.len().min(CAP - bytes.len())])
            }
            _ => break,
        }
    }
    String::from_utf8_lossy(&bytes)
        .trim()
        .chars()
        .take(160)
        .collect()
}

const MANGA_FIELDS: &str = r#"
fragment MangaFields on MangaType {
    id title url realUrl thumbnailUrl author artist description genre status
    inLibrary inLibraryAt lastFetchedAt sourceId
    source { lang }
    chapters { totalCount }
}"#;

const CHAPTER_FIELDS: &str = r#"
fragment ChapterFields on ChapterType {
    id mangaId url realUrl sourceOrder name chapterNumber scanlator uploadDate
    isRead isBookmarked isDownloaded lastPageRead pageCount
    manga { sourceId url realUrl title }
}"#;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuwayomiSourceLang {
    pub lang: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChapterCount {
    #[serde(rename = "totalCount")]
    pub total_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuwayomiManga {
    /// Captured before the engine request, never accepted from the engine wire.
    #[serde(default, rename = "_komiqGeneration")]
    pub engine_generation: Option<String>,
    pub id: i64,
    pub title: String,
    /// Source-relative manga URL (`MangaType.url`, e.g. `/manga/<uuid>` for the
    /// MangaDex extension). Carries the source's own manga identity — for MangaDex
    /// that's the canonical UUID, which lets ingest link a Suwayomi-mirrored
    /// MangaDex series straight to its catalogue `work` by exact id instead of
    /// fuzzy title/cover dedup. Not persisted in `suwayomi_series`, so it defaults
    /// to None on any DB-derived manga; only a live fetch (`MANGA_FIELDS`) fills it.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub real_url: Option<String>,
    pub thumbnail_url: Option<String>,
    pub author: Option<String>,
    pub artist: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub genre: Vec<String>,
    pub status: String,
    pub in_library: bool,
    pub in_library_at: Option<String>,
    pub last_fetched_at: Option<String>,
    /// Real newest-chapter time (millis-epoch string), from our cache — NOT part of the
    /// Suwayomi wire shape, so it defaults to None on a live fetch and is only populated
    /// when the value comes from `suwayomi_series` (see series_cache, migration 0050).
    #[serde(default)]
    pub latest_chapter_at: Option<String>,
    pub source_id: String,
    pub source: Option<SuwayomiSourceLang>,
    pub chapters: Option<ChapterCount>,
}

/// One configured extension store (repo), as `extensionStores` reports it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionStore {
    /// The store's canonical index URL — its identity in the engine, and the value an
    /// extension's `repo` carries.
    pub index_url: String,
    pub name: String,
    /// Short label the engine derives for the store (Keiyoushi's is `KEI`).
    #[serde(default)]
    pub badge_label: String,
}

/// One installed extension as reported by Suwayomi, flattened to the coordinates a
/// native device needs to install it (§2.1). One extension can back several sources
/// (e.g. an "all" extension), so `source_ids` carries every source it provides.
///
/// NOTE (could_not_verify): the upstream `ExtensionType` field names below are a
/// best guess and cannot be confirmed without a live operator Suwayomi. Parsing is
/// deliberately lenient (Option fields + `#[serde(default)]`) so a shape mismatch
/// degrades to empty/None rather than panicking. The query lives in one place
/// (`fetch_extensions`) so it's easy to correct once the real schema is checked.
#[derive(Debug, Clone)]
pub struct SuwayomiExtension {
    pub pkg_name: String,
    pub repo: Option<String>,
    pub apk_name: Option<String>,
    pub version_code: Option<i64>,
    pub lang: Option<String>,
    pub is_nsfw: bool,
    /// Suwayomi source ids this extension provides (join key to `source_series`).
    pub source_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChapterManga {
    pub source_id: String,
    pub url: String,
    pub real_url: Option<String>,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuwayomiChapter {
    #[serde(default, rename = "_komiqGeneration")]
    pub engine_generation: Option<String>,
    pub id: i64,
    pub manga_id: i64,
    /// Selected with chapter-only requests so keys keep their parent's identity.
    #[serde(default)]
    pub manga: Option<ChapterManga>,
    /// Absent on old cached records and DB-derived display rows.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub real_url: Option<String>,
    #[serde(default)]
    pub source_order: Option<i32>,
    pub name: String,
    pub chapter_number: f64,
    pub scanlator: Option<String>,
    pub upload_date: Option<String>,
    pub is_read: bool,
    pub is_bookmarked: bool,
    pub is_downloaded: bool,
    pub last_page_read: i64,
    pub page_count: i64,
}

/// A chapter's coordinates at its source — see [`SuwayomiClient::chapter_origin`].
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ChapterOrigin {
    /// Owning manga's row id in THIS engine (for the NSFW gate; never sent to a client).
    pub manga_id: i64,
    pub source_id: String,
    pub manga_url: String,
    pub manga_title: String,
    pub chapter_url: String,
    /// The chapter's page on the source's own WEBSITE (`ChapterType.realUrl`), as the
    /// extension reports it. Unlike `chapter_url` this is for a person, not an engine:
    /// it is what "Read on source" links to. `None` when the extension gives none (a
    /// local or non-HTTP source). Extension-controlled text — callers must validate the
    /// scheme before it reaches an `href`.
    pub chapter_web_url: Option<String>,
    /// The series' page on the source's website (`MangaType.realUrl`); same caveats.
    pub manga_web_url: Option<String>,
    pub source_order: i32,
}

/// One extension row from the configured extension stores — the admin-facing
/// management view (EXT-1), covering the FULL store index, not just installed
/// ones (`SuwayomiExtension` is the installed-only catalogue view). Verified
/// against Suwayomi v2.3.2243 `ExtensionType` (GQL-SCHEMA-FINDINGS.md §B2).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionListEntry {
    pub pkg_name: String,
    pub name: String,
    pub lang: String,
    pub version_name: String,
    pub is_installed: bool,
    pub has_update: bool,
    pub is_nsfw: bool,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
}

/// One installed Suwayomi source, flattened for the admin picker (EXT-1): the
/// coordinates the UI needs to feed `sourceBrowse(sourceId)`. `name` is the
/// user-facing display name; `pkg_name` joins back to the owning extension.
#[derive(Debug, Clone)]
pub struct SuwayomiSource {
    pub id: String,
    pub name: String,
    pub lang: String,
    pub is_nsfw: bool,
    pub icon_url: Option<String>,
    pub pkg_name: Option<String>,
}

/// Which source-manga listing to fetch.
#[derive(Clone, Copy)]
pub enum FetchType {
    Popular,
    Latest,
    Search,
}

impl FetchType {
    fn as_str(self) -> &'static str {
        match self {
            FetchType::Popular => "POPULAR",
            FetchType::Latest => "LATEST",
            FetchType::Search => "SEARCH",
        }
    }
}

/// Parse a JSON array element-by-element, skipping (and counting) records that
/// don't deserialize, so one malformed node (e.g. a manga with a null title, or a
/// chapter with a null name) doesn't fail the whole page/list. Mirrors the
/// per-record parse in `mangadex::list_manga`.
fn parse_records<T: DeserializeOwned>(raw: Vec<Value>, what: &str) -> Vec<T> {
    let mut out = Vec::with_capacity(raw.len());
    let mut skipped = 0usize;
    for v in raw {
        match serde_json::from_value::<T>(v) {
            Ok(x) => out.push(x),
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(
            skipped,
            kind = what,
            "suwayomi: skipped unparseable records"
        );
    }
    out
}

#[derive(Debug)]
pub(crate) struct EngineGraphqlError(pub String);
impl std::fmt::Display for EngineGraphqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for EngineGraphqlError {}

/// A read timeout alone does not establish engine replacement. Transport loss does.
fn is_transport_connection_error(error: &reqwest::Error) -> bool {
    error.is_connect() || (!error.is_timeout() && (error.is_body() || error.is_request()))
}
pub(crate) fn is_connection_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(is_transport_connection_error)
    })
}

fn stamp_generation(value: &mut Value, generation: Option<&str>) {
    match value {
        Value::Object(object) => {
            // Overwrite any upstream value, including when no fence is installed.
            object.insert(
                "_komiqGeneration".into(),
                generation.map_or(Value::Null, |id| json!(id)),
            );
            for (key, child) in object.iter_mut() {
                if key != "_komiqGeneration" {
                    stamp_generation(child, generation);
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                stamp_generation(child, generation);
            }
        }
        _ => {}
    }
}

#[derive(Clone)]
pub struct SuwayomiClient {
    pub(crate) fence: Option<Arc<crate::engine_generation::Fence>>,
    pub(crate) page_locator: Arc<crate::page_locator::PageLocator>,
    base_url: String,
    /// Public base used when building image URLs (covers/pages) handed to the
    /// browser. Defaults to `base_url` when no public URL is configured.
    image_base_url: String,
    /// `image_base_url` is the engine's own origin (no public URL, or one naming the
    /// engine itself): browsers load the engine's URLs, and no locator URL is given.
    images_on_engine: bool,
    http: reqwest::Client,
    /// Cached resolved source id; `Some` once resolved. `Arc<Mutex>` so cheap clones
    /// (e.g. into the cover drainer task) share the one resolved id rather than each
    /// re-resolving it against the engine.
    source_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    configured_source: Option<String>,
    /// Separate HTTP client for COVER fetches only, with a much shorter timeout than
    /// `http` (which the scanner shares). Same connection pool semantics, different
    /// patience — see [`COVER_TIMEOUT_SECS`].
    cover_http: reqwest::Client,
    /// Global bound on in-flight upstream cover fetches. Shared by every clone of the
    /// client (drainer, warmer, request handlers) — that is the whole point.
    cover_sem: Arc<Semaphore>,
    /// Bound on detached background materializations (see [`BG_MATERIALIZE_CONCURRENCY`]).
    bg_sem: Arc<Semaphore>,
    /// One thumbnail request at a time per manga (see [`ThumbnailTurns`]). Shared by
    /// every clone of the client.
    thumbnails: Arc<ThumbnailTurns>,
    /// Separate HTTP client for PAGE fetches — its own timeout ([`PAGE_TIMEOUT_SECS`]),
    /// between the cover and scan clients.
    page_http: reqwest::Client,
    /// Global bound on in-flight upstream PAGE fetches (see [`PAGE_FETCH_CONCURRENCY`]).
    /// Shared by every clone of the client; separate from `cover_sem`.
    page_sem: Arc<Semaphore>,
    pages_memo: Arc<TtlMemo<Vec<String>>>,
    chapter_owner_memo: Arc<TtlMemo<i64>>,
    /// Regional engines and the source routes to them (`engine_route`). Empty unless
    /// `KOMIKA_ENGINES` names one; shared by every clone of the owner client.
    pub(crate) regional: Arc<crate::engine_route::Engines>,
}

impl SuwayomiClient {
    pub fn new(
        base_url: String,
        public_url: Option<String>,
        configured_source: Option<String>,
    ) -> Self {
        Self::build(
            base_url,
            public_url,
            configured_source,
            reqwest::header::HeaderMap::new(),
        )
    }

    /// A client for a regional engine (`engine_route`): no public image base, no default
    /// source, and the engine's Basic credential on every request. The header is marked
    /// sensitive so it never appears in a debug print.
    pub(crate) fn with_authorization(
        base_url: String,
        authorization: Option<reqwest::header::HeaderValue>,
    ) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(mut value) = authorization {
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        Self::build(base_url, None, None, headers)
    }

    fn build(
        base_url: String,
        public_url: Option<String>,
        configured_source: Option<String>,
        headers: reqwest::header::HeaderMap,
    ) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        let image_base_url = public_url
            .map(|u| u.trim_end_matches('/').to_string())
            .unwrap_or_else(|| base_url.clone());
        // Origins ignore credentials: the desktop app passes the engine with a login in
        // `SUWAYOMI_URL` and without one in `SUWAYOMI_PUBLIC_URL`.
        let images_on_engine = image_base_url == base_url
            || matches!(
                (reqwest::Url::parse(&image_base_url), reqwest::Url::parse(&base_url)),
                (Ok(public), Ok(engine)) if public.origin() == engine.origin()
            );
        // Bound every request so a hung/slow upstream (Suwayomi proxies to source
        // sites, often via FlareSolverr, which routinely stalls) can't freeze the
        // scan scheduler or a reader cache-miss path forever. Mirrors
        // `MangaDexClient::new`. Falls back to the default client if the builder
        // somehow fails, so construction stays infallible.
        let http = reqwest::Client::builder()
            .default_headers(headers.clone())
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        // Cover client: same host, much less patience. A cover is on the critical path
        // of a page render, so a fast failure that the drainer/warmer can retry beats a
        // 29 s hang the browser renders as a broken image regardless.
        let cover_http = reqwest::Client::builder()
            .default_headers(headers.clone())
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(COVER_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| http.clone());
        // Page client: full-resolution content, so more patient than covers but still
        // short of the scan client, and bounded by its own pool below.
        let page_http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(PAGE_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| http.clone());
        Self {
            fence: None,
            page_locator: Arc::new(crate::page_locator::PageLocator::default()),
            base_url,
            image_base_url,
            images_on_engine,
            http,
            source_id: std::sync::Arc::new(std::sync::Mutex::new(configured_source.clone())),
            configured_source,
            cover_http,
            cover_sem: Arc::new(Semaphore::new(COVER_FETCH_CONCURRENCY)),
            bg_sem: Arc::new(Semaphore::new(BG_MATERIALIZE_CONCURRENCY)),
            thumbnails: Arc::default(),
            page_http,
            page_sem: Arc::new(Semaphore::new(PAGE_FETCH_CONCURRENCY)),
            pages_memo: Arc::new(TtlMemo::new(
                READER_MEMO_TTL,
                READER_MEMO_CAP,
                &crate::telemetry::CACHE_ENGINE_PAGES,
            )),
            chapter_owner_memo: Arc::new(TtlMemo::new(
                READER_MEMO_TTL,
                READER_MEMO_CAP,
                &crate::telemetry::CACHE_ENGINE_CHAPTER_OWNER,
            )),
            regional: Arc::default(),
        }
    }

    pub(crate) fn clear_numeric_memos(&self) {
        *self.source_id.lock().unwrap_or_else(|e| e.into_inner()) = self.configured_source.clone();
        self.pages_memo.clear();
        self.chapter_owner_memo.clear();
        self.page_locator.clear();
    }

    pub(crate) fn read_generation(&self) -> (Option<uuid::Uuid>, Option<u64>) {
        (
            self.fence.as_ref().and_then(|f| f.generation()),
            self.fence.as_ref().map(|f| f.revision()),
        )
    }

    pub(crate) async fn validate_read_generation(
        &self,
        token: (Option<uuid::Uuid>, Option<u64>),
    ) -> Result<()> {
        self.wait_for_generation().await?;
        if token != self.read_generation() {
            return Err(crate::engine_generation::FenceError::Unavailable.into());
        }
        Ok(())
    }

    pub(crate) async fn wait_for_generation(&self) -> Result<()> {
        if let Some(fence) = &self.fence {
            fence.wait().await?;
        }
        Ok(())
    }

    fn connection_failed(&self, error: &reqwest::Error) {
        if is_transport_connection_error(error) {
            if let Some(fence) = &self.fence {
                fence.invalidate();
            }
        }
    }

    fn body_failed(&self, error: &anyhow::Error) {
        if let Some(error) = error.downcast_ref::<reqwest::Error>() {
            self.connection_failed(error);
        }
    }

    #[cfg(test)]
    pub(crate) fn base_url_for_test(&mut self, url: &str) {
        self.base_url = url.to_string();
    }

    fn endpoint(&self) -> String {
        format!("{}/api/graphql", self.base_url)
    }

    /// Turn a possibly-relative Suwayomi image URL into an absolute, publicly
    /// reachable one (uses `image_base_url`, not the internal federation host).
    pub fn abs(&self, url: Option<&str>) -> String {
        match url {
            None | Some("") => String::new(),
            Some(u) if u.starts_with("http") => u.to_string(),
            Some(u) => format!("{}{}", self.image_base_url, u),
        }
    }

    /// A page's `/media/v1/c` locator URL, relative to the API origin. Only this server
    /// answers that route, so the URL is never built on the image base, which may be a
    /// public Suwayomi host. Every reader resolves it against the server it talks to.
    /// `epoch` is the engine's numeric epoch (`engine_generation::numeric_epoch`): the
    /// server refuses the URL once a new engine generation can reuse the chapter id.
    /// `None` while image URLs point at the engine itself: those pages keep the engine's
    /// URLs.
    pub fn page_locator_url(&self, epoch: i64, chapter_id: i64, index: usize) -> Option<String> {
        (!self.images_on_engine).then(|| format!("/media/v1/c/{epoch}/{chapter_id}/{index}"))
    }

    /// Fetch a cover thumbnail's raw bytes for server-side pHash (dedup signal,
    /// DD1). Uses the INTERNAL `base_url` (this runs server-side, not in the
    /// browser, so the public image host may be unreachable). Best-effort: returns
    /// `None` on any missing URL / network / non-success — a missing hash just
    /// means one fewer dedup signal, never a failure.
    pub async fn cover_bytes(&self, thumbnail_url: Option<&str>) -> Option<Vec<u8>> {
        self.wait_for_generation().await.ok()?;
        let raw = thumbnail_url?;
        if raw.is_empty() {
            return None;
        }
        let url = if raw.starts_with("http") {
            raw.to_string()
        } else {
            format!("{}{}", self.base_url, raw)
        };
        let manga_id = crate::engine_route::cover_manga_id(&url);
        let _turn = self.thumbnail_turn(manga_id).await;
        // Patient acquire. Two classes of caller, and only one is fan-out bounded:
        // the background crawl/warmer holds at most `WARM_COVER_CONCURRENCY` (enforced
        // by its `buffer_unordered`), but the pHash enrol paths in `graphql/mod.rs`
        // (`add_source_series`, `ingest_source_series`, `federated_ingest`) call this
        // directly and are bounded only by being strictly sequential — one permit per
        // in-flight request. They are NOT counted in the headroom arithmetic documented
        // at the top of this module; see it for the real on-demand floor. If enrolment
        // is ever parallelised it must take a background slot instead of competing here.
        let _permit = self.cover_sem.clone().acquire_owned().await.ok()?;
        self.checked_thumbnail(manga_id, || self.cover_bytes_once(&url))
            .await
            .inspect_err(|e| tracing::debug!(error = %e, "suwayomi cover bytes unavailable"))
            .ok()
            .map(|(bytes, _)| bytes)
    }

    /// One download for [`Self::cover_bytes`]. Assumes the caller holds a permit.
    async fn cover_bytes_once(&self, url: &str) -> Result<(Vec<u8>, String)> {
        let res = self
            .cover_http
            .get(url)
            .send()
            .await
            .inspect_err(|e| self.connection_failed(e))?;
        if !res.status().is_success() {
            return Err(anyhow!("Suwayomi cover error {}", res.status()));
        }
        let bytes = read_capped(res, MAX_COVER_SOURCE_BYTES)
            .await
            .inspect_err(|e| self.body_failed(e))?;
        // A well-formed HTTP response can still carry an INCOMPLETE image: the source
        // CDN (or Suwayomi's proxy of it) closes the body early and `zune-jpeg` decodes
        // the partial data to `Ok` with a flat decoder-fill tail. Reject it here so the
        // truncated bytes never reach `process_cover` and get frozen into the cache.
        if let Err(e) = crate::avatar::ensure_complete(&bytes) {
            tracing::warn!(error = %e, "suwayomi cover: rejected truncated source");
            return Err(e);
        }
        Ok((bytes, String::new()))
    }

    /// The manga's thumbnail turn (see [`ThumbnailTurns`]); `None` for a URL that names
    /// no manga, which then runs unserialized as before.
    async fn thumbnail_turn(
        &self,
        manga_id: Option<i64>,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        Some(self.thumbnails.take(manga_id?).await)
    }

    /// Run one thumbnail download and refuse garbled JPEG data. A garbled copy is the
    /// engine's SAVED thumbnail, so fetching again returns the same bytes: instead the
    /// engine is asked for the manga's details, which drops its saved thumbnail
    /// (Suwayomi `Manga.clearThumbnail`), and the download runs once more. At most once
    /// per [`THUMBNAIL_REPAIR_EVERY`] per manga. The caller holds the manga's turn.
    async fn checked_thumbnail<F, Fut>(
        &self,
        manga_id: Option<i64>,
        fetch: F,
    ) -> Result<(Vec<u8>, String)>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<(Vec<u8>, String)>>,
    {
        let fetched = fetch().await?;
        let Err(error) = crate::jpeg_scan::check(&fetched.0) else {
            return Ok(fetched);
        };
        let Some(id) = manga_id.filter(|&id| self.thumbnails.may_repair(id)) else {
            return Err(error);
        };
        tracing::warn!(
            manga_id = id,
            error = %error,
            "suwayomi cover: saved thumbnail is garbled, asking the engine to download it again"
        );
        self.drop_saved_thumbnail(id).await?;
        let fetched = fetch().await?;
        crate::jpeg_scan::check(&fetched.0)?;
        Ok(fetched)
    }

    /// Ask the engine to download `manga_id`'s thumbnail again on its next request:
    /// fetching the manga's details from the source drops the saved copy when the
    /// source names a thumbnail. Errors when the source could not be reached, because
    /// then the saved copy is still there.
    async fn drop_saved_thumbnail(&self, manga_id: i64) -> Result<()> {
        match self.series_with_provenance(manga_id).await? {
            (_, Provenance::Upstream) => Ok(()),
            (_, Provenance::CachedFallback) => Err(anyhow!(
                "the engine could not reach the source to download manga {manga_id}'s cover again"
            )),
        }
    }

    /// [`Self::drop_saved_thumbnail`] in the manga's turn, for an admin's explicit
    /// "fetch the cover again".
    pub async fn refresh_thumbnail(&self, manga_id: i64) -> Result<()> {
        let _turn = self.thumbnails.take(manga_id).await;
        self.drop_saved_thumbnail(manga_id).await
    }

    pub(crate) fn page_permits_available(&self) -> usize {
        self.page_sem.available_permits()
    }

    /// Number of free permits in the bounded cover-fetch pool. Exposed for tests and
    /// for the warmer's "is the on-demand path under pressure?" check.
    pub fn cover_permits_available(&self) -> usize {
        self.cover_sem.available_permits()
    }

    /// Take a slot for a DETACHED background cover materialization, or `None` if the
    /// bounded background pool is full. Holding the returned permit for the lifetime of
    /// the spawned task is what bounds the fan-out.
    pub fn try_background_slot(&self) -> Option<OwnedSemaphorePermit> {
        self.bg_sem.clone().try_acquire_owned().ok()
    }

    /// Fetch a cover image from the internal Suwayomi REST path for an ON-DEMAND
    /// request. Unlike a plain unbounded fetch this:
    ///
    /// * takes a permit from the bounded cover pool with `try_acquire` and returns
    ///   [`CoverFetchError::Busy`] immediately when saturated — no queueing, which is
    ///   what turned a 5.6 s p50 into a 22 s p90;
    /// * uses the short cover timeout, not the 30 s scan timeout;
    /// * streams the body under [`MAX_COVER_SOURCE_BYTES`] and rejects a truncated
    ///   image instead of handing partial bytes to the decoder.
    ///
    /// The one wait it does have is the manga's turn ([`ThumbnailTurns`]), taken before
    /// the permit: behind another request for the SAME cover only, so it never holds a
    /// permit while it waits.
    pub async fn fetch_cover_now(
        &self,
        path: &str,
    ) -> std::result::Result<(Vec<u8>, String), CoverFetchError> {
        let manga_id = crate::engine_route::cover_manga_id(path);
        let _turn = self.thumbnail_turn(manga_id).await;
        let lane = crate::telemetry::ADMISSION.lane(crate::telemetry::Lane::CoverFetch);
        let Ok(_permit) = self.cover_sem.clone().try_acquire_owned() else {
            lane.refuse();
            return Err(CoverFetchError::Busy);
        };
        lane.admitted();
        self.checked_thumbnail(manga_id, || self.fetch_cover_inner(path))
            .await
            .map_err(CoverFetchError::Upstream)
    }

    /// Fetch a cover image from the internal Suwayomi REST path for BACKGROUND work
    /// (warmer / detached materialization). Waits for a permit rather than failing —
    /// background work is patient — but the caller must bound its own fan-out to
    /// [`WARM_COVER_CONCURRENCY`] so on-demand requests keep their headroom.
    pub async fn fetch_cover_background(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let manga_id = crate::engine_route::cover_manga_id(path);
        let _turn = self.thumbnail_turn(manga_id).await;
        let _permit = self
            .cover_sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("cover fetch semaphore closed"))?;
        self.checked_thumbnail(manga_id, || self.fetch_cover_inner(path))
            .await
    }

    /// One download for the two cover fetchers. Assumes the caller holds a permit and
    /// the manga's turn.
    async fn fetch_cover_inner(&self, path: &str) -> Result<(Vec<u8>, String)> {
        self.wait_for_generation().await?;
        let generation = self.read_generation();
        let url = format!("{}{}", self.base_url, path);
        let res = self
            .cover_http
            .get(url)
            .send()
            .await
            .inspect_err(|e| self.connection_failed(e))?;
        if !res.status().is_success() {
            // Include a snippet of the body: Suwayomi answers 400 with "Expected URL
            // scheme 'http' or 'https' but was 'data'" when the extension stored a
            // `data:` thumbnail, which is the difference between a PERMANENT and a
            // transient cover failure (see `cover::classify_fetch_error`) and the only
            // thing that makes those rows diagnosable in the admin Bugs panel.
            let status = res.status();
            let snippet = error_snippet(res).await;
            return Err(if snippet.is_empty() {
                anyhow!("Suwayomi cover error {status}")
            } else {
                anyhow!("Suwayomi cover error {status}: {snippet}")
            });
        }
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = read_capped(res, MAX_COVER_SOURCE_BYTES)
            .await
            .inspect_err(|error| {
                self.body_failed(error);
                count_oversize(BodySource::EngineCover, error);
            })?;
        let content_type =
            crate::image_delivery::validate_from(BodySource::EngineCover, &bytes, &content_type)?
                .to_string();
        self.validate_read_generation(generation).await?;
        Ok((bytes, content_type))
    }

    /// Fetch a chapter PAGE from the internal Suwayomi REST path for an ON-DEMAND
    /// request. Like [`Self::fetch_cover_now`] this bounds concurrency (own pool,
    /// [`PAGE_FETCH_CONCURRENCY`]), uses a page-specific timeout ([`PAGE_TIMEOUT_SECS`])
    /// and streams under a byte cap ([`MAX_PAGE_SOURCE_BYTES`]) — so a slow source can
    /// neither freeze a slot for 30 s nor pile unbounded fetches onto the engine.
    ///
    /// Unlike covers it WAITS up to [`PAGE_ACQUIRE_WAIT_SECS`] for a permit before
    /// shedding with [`PageFetchError::Busy`]: a page has no background warmer, so
    /// riding out a transient burst beats a broken page the reader can only recover with
    /// a manual retry. Sustained saturation still sheds (retryable) rather than queueing
    /// into the 30 s pile-up the old unbounded path produced.
    pub async fn fetch_page_now(
        &self,
        path: &str,
    ) -> std::result::Result<(axum::body::Bytes, String), PageFetchError> {
        // Held for the whole fetch below (bound to a named local, not `_`, so it is not
        // dropped early) — that is what makes the concurrency bound effective.
        let permit = acquire_page_slot(
            crate::telemetry::ADMISSION.lane(crate::telemetry::Lane::PageFetch),
            &self.page_sem,
            std::time::Duration::from_secs(PAGE_ACQUIRE_WAIT_SECS),
        )
        .await?;
        let (bytes, content_type) = self
            .fetch_page_inner(path)
            .await
            .map_err(PageFetchError::Upstream)?;
        Ok((
            crate::image_delivery::retain_fetch_slot(bytes, permit),
            content_type,
        ))
    }

    /// Shared body of the page fetcher. Assumes the caller holds a page-pool permit.
    async fn fetch_page_inner(&self, path: &str) -> Result<(Vec<u8>, String)> {
        self.wait_for_generation().await?;
        let generation = self.read_generation();
        let url = format!("{}{}", self.base_url, path);
        let res = self
            .page_http
            .get(url)
            .send()
            .await
            .inspect_err(|e| self.connection_failed(e))?;
        if !res.status().is_success() {
            // Include a snippet of the body, as the cover fetcher does: Suwayomi answers
            // its own 500 with the SOURCE's error ("HTTP error 403") in the body, and
            // that is the only thing that says why the page failed.
            let status = res.status();
            let snippet = error_snippet(res).await;
            return Err(if snippet.is_empty() {
                anyhow!("Suwayomi page error {status}")
            } else {
                anyhow!("Suwayomi page error {status}: {snippet}")
            });
        }
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = read_capped(res, MAX_PAGE_SOURCE_BYTES)
            .await
            .inspect_err(|error| {
                self.body_failed(error);
                count_oversize(BodySource::Page, error);
            })?;
        let content_type =
            crate::image_delivery::validate_from(BodySource::Page, &bytes, &content_type)?
                .to_string();
        self.validate_read_generation(generation).await?;
        Ok((bytes, content_type))
    }

    pub(crate) async fn gql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Value,
    ) -> Result<T> {
        self.wait_for_generation().await?;
        let generation = self.fence.as_ref().and_then(|f| f.generation());
        let revision = self.fence.as_ref().map(|f| f.revision());
        let mut data = self.identity_query(query, variables).await?;
        self.validate_read_generation((generation, revision))
            .await?;
        stamp_generation(&mut data, generation.map(|id| id.to_string()).as_deref());
        Ok(serde_json::from_value(data)?)
    }

    /// Gate-free transport used only by the identity checker and by `gql`.
    pub(crate) async fn identity_query(&self, query: &str, variables: Value) -> Result<Value> {
        let _permit = crate::request_budget::upstream_permit()?;
        let res = self
            .http
            .post(self.endpoint())
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .inspect_err(|e| self.connection_failed(e))?;
        if !res.status().is_success() {
            return Err(anyhow!("Suwayomi error {}", res.status()));
        }
        let body: Value = res
            .json()
            .await
            .inspect_err(|e| self.connection_failed(e))?;
        if let Some(errors) = body.get("errors").and_then(|e| e.as_array()) {
            if !errors.is_empty() {
                let msg = errors
                    .iter()
                    .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(EngineGraphqlError(msg).into());
            }
        }
        body.get("data")
            .cloned()
            .ok_or_else(|| anyhow!("Suwayomi returned no data"))
    }

    /// Resolve (and cache) a source id to browse: configured → English → first real.
    ///
    /// Double-checked locking: the network resolve (`sources` query) runs WITHOUT the
    /// cache lock held, so concurrent first-callers don't serialize behind a single
    /// round-trip. A benign race may resolve twice, but the stored value is stable
    /// (it's process-lifetime; the first writer wins and later writers store the same
    /// deterministic pick), so the extra query is harmless.
    async fn resolve_source(&self) -> Result<String> {
        // Fast path: already cached.
        if let Some(id) = self
            .source_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Ok(id.clone());
        }
        // A configured source needs no network round-trip; cache and return.
        if let Some(id) = &self.configured_source {
            let mut guard = self.source_id.lock().unwrap_or_else(|e| e.into_inner());
            let id = guard.get_or_insert_with(|| id.clone()).clone();
            return Ok(id);
        }
        #[derive(Deserialize)]
        struct Src {
            id: String,
            lang: Option<String>,
        }
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<Src>,
        }
        #[derive(Deserialize)]
        struct Data {
            sources: Nodes,
        }
        // Resolve over the network with NO lock held.
        let data: Data = self
            .gql("query Sources { sources { nodes { id lang } } }", json!({}))
            .await?;
        let real: Vec<Src> = data
            .sources
            .nodes
            .into_iter()
            .filter(|s| s.id != "0")
            .collect();
        let chosen = real
            .iter()
            .find(|s| s.lang.as_deref() == Some("en"))
            .or_else(|| real.first())
            .ok_or_else(|| anyhow!("No Suwayomi source installed — add one first"))?;
        // Re-acquire and store. If another caller won the race, keep their value.
        let mut guard = self.source_id.lock().unwrap_or_else(|e| e.into_inner());
        let id = guard.get_or_insert_with(|| chosen.id.clone()).clone();
        Ok(id)
    }

    pub async fn fetch_source(
        &self,
        ty: FetchType,
        page: i32,
        query: Option<&str>,
    ) -> Result<(bool, Vec<SuwayomiManga>)> {
        let source = self.resolve_source().await?;
        self.browse_source(&source, ty, page, query).await
    }

    /// Browse/search one EXPLICIT source (`fetchSourceManga`, GQL-SCHEMA-FINDINGS.md
    /// §A1) — the admin "Sources & Extensions" surface picks the source id itself
    /// instead of going through the resolved default (EXT-1). Every returned manga
    /// is persisted by Suwayomi and gets an internal id (§A0), which is what the
    /// bulk-ingest flow feeds to `add_source_series`.
    pub async fn browse_source(
        &self,
        source_id: &str,
        ty: FetchType,
        page: i32,
        query: Option<&str>,
    ) -> Result<(bool, Vec<SuwayomiManga>)> {
        let doc = format!(
            "{MANGA_FIELDS}\n\
             mutation F($source: LongString!, $type: FetchSourceMangaType!, $page: Int!, $query: String) {{\
               fetchSourceManga(input: {{ source: $source, type: $type, page: $page, query: $query }}) {{\
                 hasNextPage mangas {{ ...MangaFields }} }} }}"
        );
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Payload {
            has_next_page: bool,
            mangas: Vec<Value>,
        }
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "fetchSourceManga")]
            fetch_source_manga: Payload,
        }
        let fetched = self
            .gql::<Data>(
                &doc,
                json!({ "source": source_id, "type": ty.as_str(), "page": page, "query": query }),
            )
            .await;
        // Every listing caller (discovery, source-sync, search, ingest) comes through
        // here, so this one line is the whole of the `browse` health surface.
        if !matches!(&fetched, Err(error) if crate::request_budget::is_upstream_busy(error)) {
            crate::source_health::observe(Subject::Source(source_id), Surface::Browse, &fetched);
        }
        let data = fetched?;
        Ok((
            data.fetch_source_manga.has_next_page,
            parse_records::<SuwayomiManga>(data.fetch_source_manga.mangas, "browse_manga"),
        ))
    }

    /// Fetch full detail from the source (populates genres/status/description),
    /// falling back to the DB-cached manga if the source fetch fails.
    ///
    /// Returns only the value; callers that need to know whether the data is FRESH must
    /// use [`Self::series_with_provenance`] — see [`Provenance`] for why (F11).
    pub async fn series(&self, id: i64) -> Result<SuwayomiManga> {
        self.series_with_provenance(id).await.map(|(m, _)| m)
    }

    /// [`Self::series`], plus where the value came from. See [`Provenance`].
    pub async fn series_with_provenance(&self, id: i64) -> Result<(SuwayomiManga, Provenance)> {
        let detail = format!(
            "{MANGA_FIELDS}\n\
             mutation D($id: Int!) {{ fetchMangaAndChapters(input: {{ id: $id, fetchManga: true, fetchChapters: false }}) {{ manga {{ ...MangaFields }} }} }}"
        );
        #[derive(Deserialize)]
        struct DetailPayload {
            manga: SuwayomiManga,
        }
        #[derive(Deserialize)]
        struct DetailData {
            #[serde(rename = "fetchMangaAndChapters")]
            f: DetailPayload,
        }
        match self.gql::<DetailData>(&detail, json!({ "id": id })).await {
            Ok(d) => Ok((d.f.manga, Provenance::Upstream)),
            Err(e) if crate::request_budget::is_upstream_busy(&e) => Err(e),
            Err(e) => {
                let doc = format!(
                    "{MANGA_FIELDS}\nquery M($id: Int!) {{ manga(id: $id) {{ ...MangaFields }} }}"
                );
                #[derive(Deserialize)]
                struct Data {
                    manga: SuwayomiManga,
                }
                let d: Data = self.gql(&doc, json!({ "id": id })).await?;
                tracing::debug!(series_id = id, error = %e, "suwayomi: series detail fell back to the local cache");
                Ok((d.manga, Provenance::CachedFallback))
            }
        }
    }

    /// Fetch the source's chapter list, falling back to Suwayomi's local cache on an
    /// upstream failure.
    ///
    /// Returns only the value; callers that need to know whether the list is FRESH must
    /// use [`Self::chapters_with_provenance`] — see [`Provenance`] for why (F11).
    pub async fn chapters(&self, series_id: i64) -> Result<Vec<SuwayomiChapter>> {
        self.chapters_with_provenance(series_id)
            .await
            .map(|(c, _)| c)
    }

    /// [`Self::chapters`], plus where the list came from. See [`Provenance`].
    pub async fn chapters_with_provenance(
        &self,
        series_id: i64,
    ) -> Result<(Vec<SuwayomiChapter>, Provenance)> {
        let fetch = format!(
            "{CHAPTER_FIELDS}\n\
             mutation FC($id: Int!) {{ fetchMangaAndChapters(input: {{ id: $id, fetchManga: false, fetchChapters: true }}) {{ chapters {{ ...ChapterFields }} }} }}"
        );
        #[derive(Deserialize)]
        struct FetchPayload {
            chapters: Option<Vec<Value>>,
        }
        #[derive(Deserialize)]
        struct FetchData {
            #[serde(rename = "fetchMangaAndChapters")]
            f: FetchPayload,
        }
        match self.refresh_chapters::<FetchData>(&fetch, series_id).await {
            Ok(d) => {
                crate::source_health::ok(Subject::Manga(series_id), Surface::Chapters);
                Ok((
                    parse_records::<SuwayomiChapter>(d.f.chapters.unwrap_or_default(), "chapters"),
                    Provenance::Upstream,
                ))
            }
            Err(e) => {
                // Admission refusal says nothing about source health and must not
                // start fallback calls that compete for the same exhausted pool.
                if crate::request_budget::is_upstream_busy(&e) {
                    return Err(e);
                }
                // "No chapters" is an upstream ANSWER, not an upstream failure: the
                // mutation reached the source and it has nothing. Reported as
                // `Upstream` so a genuinely empty series is not mistaken for a broken
                // source — the per-source health view counts zero-chapter series
                // separately (see `catalog::source_scan_health`).
                if e.to_string().contains("No chapters") {
                    crate::source_health::ok(Subject::Manga(series_id), Surface::Chapters);
                    return Ok((vec![], Provenance::Upstream));
                }
                // The one place the upstream error text still exists: below this line it
                // is replaced by Suwayomi's cache and never seen again (F11). Recorded
                // here rather than by the callers so the scanner and the reader's
                // cache-miss path are both covered.
                crate::source_health::fail(Subject::Manga(series_id), Surface::Chapters, &e);
                let doc = format!(
                    "{CHAPTER_FIELDS}\n\
                     query C($id: Int!) {{ chapters(condition: {{ mangaId: $id }}, order: {{ by: SOURCE_ORDER, byType: DESC }}) {{ nodes {{ ...ChapterFields }} }} }}"
                );
                #[derive(Deserialize)]
                struct Nodes {
                    nodes: Vec<Value>,
                }
                #[derive(Deserialize)]
                struct Data {
                    chapters: Nodes,
                }
                let d: Data = self.gql(&doc, json!({ "id": series_id })).await?;
                tracing::debug!(series_id, error = %e, "suwayomi: chapter list fell back to the local cache");
                Ok((
                    parse_records::<SuwayomiChapter>(d.chapters.nodes, "chapters"),
                    Provenance::CachedFallback,
                ))
            }
        }
    }

    /// Fetch fresh manga detail AND its chapter list in ONE upstream round-trip
    /// (`fetchMangaAndChapters` with both flags). The DB-driven scanner uses this so a
    /// due series costs a single engine call for current status (pause re-check) + the
    /// chapter list, instead of two. Falls back to the separate `series` + `chapters`
    /// calls if the combined mutation fails (older engine / partial support).
    ///
    /// Returns only the values; the scanner MUST use
    /// [`Self::series_and_chapters_with_provenance`] instead — see [`Provenance`] (F11).
    pub async fn series_and_chapters(
        &self,
        id: i64,
    ) -> Result<(SuwayomiManga, Vec<SuwayomiChapter>)> {
        self.series_and_chapters_with_provenance(id)
            .await
            .map(|(m, c, _)| (m, c))
    }

    /// Read both key sets from the engine's database, without refreshing the source.
    pub async fn cached_series_and_chapters(
        &self,
        id: i64,
    ) -> Result<(SuwayomiManga, Vec<SuwayomiChapter>)> {
        let doc = format!(
            "{MANGA_FIELDS}\n{CHAPTER_FIELDS}\n\
             query EK($id: Int!) {{ manga(id: $id) {{ ...MangaFields }} \
             chapters(condition: {{ mangaId: $id }}) {{ nodes {{ ...ChapterFields }} }} }}"
        );
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<SuwayomiChapter>,
        }
        #[derive(Deserialize)]
        struct Data {
            manga: SuwayomiManga,
            chapters: Nodes,
        }
        let d: Data = self.gql(&doc, json!({ "id": id })).await?;
        Ok((d.manga, d.chapters.nodes))
    }

    /// [`Self::series_and_chapters`], plus where the data came from. See [`Provenance`].
    ///
    /// The provenance is the WEAKER of the two halves: this call is only `Upstream` when
    /// the combined mutation itself succeeded. Once it fails, both the manga and the
    /// chapter list come from the per-call fallbacks below, and the chapter list is the
    /// half that matters — a cached list cannot contain a chapter we have not already
    /// seen, so "no new chapters" from it is not a finding.
    pub async fn series_and_chapters_with_provenance(
        &self,
        id: i64,
    ) -> Result<(SuwayomiManga, Vec<SuwayomiChapter>, Provenance)> {
        let doc = format!(
            "{MANGA_FIELDS}\n{CHAPTER_FIELDS}\n\
             mutation MC($id: Int!) {{ fetchMangaAndChapters(input: {{ id: $id, fetchManga: true, fetchChapters: true }}) {{ manga {{ ...MangaFields }} chapters {{ ...ChapterFields }} }} }}"
        );
        #[derive(Deserialize)]
        struct Payload {
            manga: SuwayomiManga,
            chapters: Option<Vec<Value>>,
        }
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "fetchMangaAndChapters")]
            f: Payload,
        }
        match self.refresh_chapters::<Data>(&doc, id).await {
            Ok(d) => {
                crate::source_health::ok(Subject::Manga(id), Surface::Chapters);
                Ok((
                    d.f.manga,
                    parse_records::<SuwayomiChapter>(d.f.chapters.unwrap_or_default(), "chapters"),
                    Provenance::Upstream,
                ))
            }
            Err(e) if crate::request_budget::is_upstream_busy(&e) => Err(e),
            Err(_) => {
                // Fallback: two calls (each carries its own older-engine fallback). The
                // failure is recorded by `chapters_with_provenance`, once, so a scan that
                // fails here is not counted twice.
                let (m, m_prov) = self.series_with_provenance(id).await?;
                let (chapters, ch_prov) = self.chapters_with_provenance(id).await?;
                Ok((m, chapters, m_prov.and(ch_prov)))
            }
        }
    }

    async fn refresh_chapters<T: DeserializeOwned>(&self, query: &str, manga_id: i64) -> Result<T> {
        let guard = self.page_locator.begin_refresh(manga_id);
        let result = self.gql(query, json!({ "id": manga_id })).await;
        // Once the request may have been sent, the engine may still be running the
        // mutation: a timeout, a proxy's 5xx or a fence change says nothing about it.
        // Only an admission refusal (never sent) or the engine's own GraphQL error
        // answer has a known outcome.
        let known = result.as_ref().err().is_none_or(|error| {
            crate::request_budget::is_upstream_busy(error)
                || error.chain().any(|cause| cause.is::<EngineGraphqlError>())
        });
        guard.finish(known);
        result
    }

    pub(crate) async fn chapter_location(&self, chapter_id: i64) -> Result<Option<(i64, i32)>> {
        let doc = "query CO($id: Int!) { chapters(condition: { id: $id }) { nodes { mangaId sourceOrder } } }";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            manga_id: i64,
            source_order: i32,
        }
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<Node>,
        }
        #[derive(Deserialize)]
        struct Data {
            chapters: Nodes,
        }
        let d: Data = self.gql(doc, json!({ "id": chapter_id })).await?;
        Ok(d.chapters
            .nodes
            .first()
            .map(|n| (n.manga_id, n.source_order)))
    }

    pub async fn pages(&self, chapter_id: i64) -> Result<Vec<String>> {
        self.wait_for_generation().await?;
        self.pages_memo
            .get_or_try_init(&chapter_id.to_string(), || self.fetch_pages(chapter_id))
            .await
    }

    pub(crate) fn reader_pages_cached(&self, chapter_id: i64) -> bool {
        let key = chapter_id.to_string();
        self.pages_memo.has_value(&key) && self.chapter_owner_memo.has_value(&key)
    }

    async fn fetch_pages(&self, chapter_id: i64) -> Result<Vec<String>> {
        let doc =
            "mutation P($id: Int!) { fetchChapterPages(input: { chapterId: $id }) { pages } }";
        #[derive(Deserialize)]
        struct Payload {
            pages: Vec<String>,
        }
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "fetchChapterPages")]
            fetch_chapter_pages: Payload,
        }
        let d: Data = self.gql(doc, json!({ "id": chapter_id })).await?;
        Ok(d.fetch_chapter_pages
            .pages
            .iter()
            .map(|u| self.abs(Some(u)))
            .collect())
    }

    /// Resolve the owning manga id for a Suwayomi chapter, for NSFW-gating `pages`.
    /// Suwayomi chapter ids are sequential integers, so a viewer who hasn't opted in
    /// could otherwise hand-craft one to read an NSFW series' page images — the gate
    /// needs the owning series, which isn't mirrored locally for Suwayomi sources.
    /// `None` if the chapter is unknown to the source server.
    pub async fn chapter_manga_id(&self, chapter_id: i64) -> Result<Option<i64>> {
        self.wait_for_generation().await?;
        self.chapter_owner_memo
            .get_or_try_init_optional(&chapter_id.to_string(), || {
                self.fetch_chapter_manga_id(chapter_id)
            })
            .await
    }

    async fn fetch_chapter_manga_id(&self, chapter_id: i64) -> Result<Option<i64>> {
        let doc = "query CM($id: Int!) { chapters(condition: { id: $id }) { nodes { mangaId } } }";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            manga_id: i64,
        }
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<Node>,
        }
        #[derive(Deserialize)]
        struct Data {
            chapters: Nodes,
        }
        let d: Data = self.gql(doc, json!({ "id": chapter_id })).await?;
        Ok(d.chapters.nodes.first().map(|n| n.manga_id))
    }

    /// Where a chapter lives AT ITS SOURCE: the source id plus the source's own keys for
    /// the manga and the chapter (`MangaType.url` / `ChapterType.url`).
    ///
    /// Our chapter and manga ids are row ids of THIS engine and mean nothing to another
    /// one; the urls are what the extension itself assigns, so a reader app running its
    /// own engine can find the same chapter with them and fetch the pages itself. Reads
    /// the engine's database only — nothing is requested from the source. `None` if the
    /// chapter is unknown.
    pub async fn chapter_origin(&self, chapter_id: i64) -> Result<Option<ChapterOrigin>> {
        let doc = "query CO($id: Int!) { chapters(condition: { id: $id }) { nodes { \
                   url realUrl sourceOrder mangaId manga { url realUrl title sourceId } } } }";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Manga {
            url: String,
            #[serde(default)]
            real_url: Option<String>,
            title: String,
            source_id: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            url: String,
            #[serde(default)]
            real_url: Option<String>,
            source_order: i32,
            manga_id: i64,
            manga: Manga,
        }
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<Node>,
        }
        #[derive(Deserialize)]
        struct Data {
            chapters: Nodes,
        }
        let d: Data = self.gql(doc, json!({ "id": chapter_id })).await?;
        Ok(d.chapters.nodes.into_iter().next().map(|n| ChapterOrigin {
            manga_id: n.manga_id,
            source_id: n.manga.source_id,
            manga_url: n.manga.url,
            manga_title: n.manga.title,
            chapter_url: n.url,
            chapter_web_url: n.real_url,
            manga_web_url: n.manga.real_url,
            source_order: n.source_order,
        }))
    }

    /// Page size for the paginated library walk. The in-library set can be 100k+, so a
    /// single unpaginated `mangas` query returns one enormous response; page it instead.
    const LIBRARY_PAGE_SIZE: i64 = 500;
    /// Safety bound on the library walk so a mis-behaving `hasNextPage` (never false)
    /// can't loop forever. 4000 pages × 500 = 2M series, far above any real library.
    const LIBRARY_MAX_PAGES: i64 = 4000;

    /// The full in-library set, fetched in bounded pages. Falls back to a single
    /// unpaginated query if the engine rejects the pagination args (older Suwayomi),
    /// so a working deployment can't regress.
    pub async fn library(&self) -> Result<Vec<SuwayomiManga>> {
        let mut out: Vec<SuwayomiManga> = Vec::new();
        let mut offset = 0i64;
        for _ in 0..Self::LIBRARY_MAX_PAGES {
            let (has_next, mut page) = match self.library_page(offset).await {
                Ok(p) => p,
                Err(e) if offset == 0 => {
                    tracing::warn!(error = %e, "library: paginated fetch failed on first page; falling back to unpaginated");
                    return self.library_unpaginated().await;
                }
                Err(e) => return Err(e),
            };
            let n = page.len();
            out.append(&mut page);
            if !has_next || n == 0 {
                break;
            }
            offset += Self::LIBRARY_PAGE_SIZE;
        }
        Ok(out)
    }

    /// Just the in-library manga ids (as strings), via a LIGHTWEIGHT id-only paginated
    /// query — the membership set the daily reconcile needs. This deliberately avoids the
    /// full `MANGA_FIELDS` selection: its per-manga `chapters { totalCount }` + `source
    /// { lang }` is an N+1 in Suwayomi that makes a full-record fetch of a ~13k library
    /// take ~50s (blowing the 30s client timeout on the whole set). An `id`-only walk is
    /// near-instant. Falls back to an id-only unpaginated query if the engine rejects the
    /// pagination args.
    pub async fn library_ids(&self) -> Result<std::collections::HashSet<String>> {
        let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut offset = 0i64;
        for _ in 0..Self::LIBRARY_MAX_PAGES {
            let (has_next, page) = match self.library_id_page(offset).await {
                Ok(p) => p,
                Err(e) if offset == 0 => {
                    tracing::warn!(error = %e, "library_ids: paginated id fetch failed on first page; falling back to unpaginated");
                    return Ok(self
                        .library_ids_unpaginated()
                        .await?
                        .into_iter()
                        .map(|id| id.to_string())
                        .collect());
                }
                Err(e) => return Err(e),
            };
            let n = page.len();
            ids.extend(page.into_iter().map(|id| id.to_string()));
            if !has_next || n == 0 {
                break;
            }
            offset += Self::LIBRARY_PAGE_SIZE;
        }
        Ok(ids)
    }

    /// One page of in-library manga IDS ONLY (no `MANGA_FIELDS`, so no per-manga N+1).
    /// Returns `(hasNextPage, ids)`.
    async fn library_id_page(&self, offset: i64) -> Result<(bool, Vec<i64>)> {
        let doc = "query L($first: Int!, $offset: Int!) { \
             mangas(condition: { inLibrary: true }, first: $first, offset: $offset) { \
               pageInfo { hasNextPage } nodes { id } } }";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            has_next_page: bool,
        }
        #[derive(Deserialize)]
        struct Node {
            id: i64,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Nodes {
            page_info: PageInfo,
            nodes: Vec<Node>,
        }
        #[derive(Deserialize)]
        struct Data {
            mangas: Nodes,
        }
        let d: Data = self
            .gql(
                doc,
                json!({ "first": Self::LIBRARY_PAGE_SIZE, "offset": offset }),
            )
            .await?;
        Ok((
            d.mangas.page_info.has_next_page,
            d.mangas.nodes.into_iter().map(|n| n.id).collect(),
        ))
    }

    /// id-only single-shot query — fallback for engines without `first`/`offset` paging.
    async fn library_ids_unpaginated(&self) -> Result<Vec<i64>> {
        let doc = "query L { mangas(condition: { inLibrary: true }) { nodes { id } } }";
        #[derive(Deserialize)]
        struct Node {
            id: i64,
        }
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<Node>,
        }
        #[derive(Deserialize)]
        struct Data {
            mangas: Nodes,
        }
        let d: Data = self.gql(doc, json!({})).await?;
        Ok(d.mangas.nodes.into_iter().map(|n| n.id).collect())
    }

    /// One page of the in-library set. Returns `(hasNextPage, mangas)`.
    async fn library_page(&self, offset: i64) -> Result<(bool, Vec<SuwayomiManga>)> {
        let doc = format!(
            "{MANGA_FIELDS}\nquery L($first: Int!, $offset: Int!) {{ \
               mangas(condition: {{ inLibrary: true }}, first: $first, offset: $offset) {{ \
                 pageInfo {{ hasNextPage }} nodes {{ ...MangaFields }} }} }}"
        );
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PageInfo {
            has_next_page: bool,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Nodes {
            page_info: PageInfo,
            nodes: Vec<Value>,
        }
        #[derive(Deserialize)]
        struct Data {
            mangas: Nodes,
        }
        let d: Data = self
            .gql(
                &doc,
                json!({ "first": Self::LIBRARY_PAGE_SIZE, "offset": offset }),
            )
            .await?;
        Ok((
            d.mangas.page_info.has_next_page,
            parse_records::<SuwayomiManga>(d.mangas.nodes, "library"),
        ))
    }

    /// The original single-shot library query, kept as a fallback for engines that don't
    /// support `first`/`offset` pagination on `mangas`.
    async fn library_unpaginated(&self) -> Result<Vec<SuwayomiManga>> {
        let doc = format!(
            "{MANGA_FIELDS}\nquery L {{ mangas(condition: {{ inLibrary: true }}) {{ nodes {{ ...MangaFields }} }} }}"
        );
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<Value>,
        }
        #[derive(Deserialize)]
        struct Data {
            mangas: Nodes,
        }
        let d: Data = self.gql(&doc, json!({})).await?;
        Ok(parse_records::<SuwayomiManga>(d.mangas.nodes, "library"))
    }

    /// List the installed extensions and their coordinates (§2.1), so the catalogue
    /// can record, per source id, the exact extension a device must install. Only
    /// installed extensions are relevant (they're what actually backs a source), so
    /// the query filters on `isInstalled`.
    ///
    /// Best-effort + could_not_verify: the `ExtensionType` shape is a best guess and
    /// unconfirmed without a live Suwayomi. Parsing is lenient (see `SuwayomiExtension`)
    /// so a schema mismatch surfaces as an error the caller logs and swallows, never a
    /// panic. Keep the query text here — it's the single place to fix if the real
    /// upstream field names differ.
    pub async fn fetch_extensions(&self) -> Result<Vec<SuwayomiExtension>> {
        let doc = "query Extensions { \
            extensions(condition: { isInstalled: true }) { \
              nodes { pkgName repo apkName versionCode lang isNsfw source { nodes { id } } } } }";
        #[derive(Deserialize)]
        struct SourceId {
            id: String,
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct SourceNodes {
            nodes: Vec<SourceId>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            pkg_name: String,
            #[serde(default)]
            repo: Option<String>,
            #[serde(default)]
            apk_name: Option<String>,
            #[serde(default)]
            version_code: Option<i64>,
            #[serde(default)]
            lang: Option<String>,
            #[serde(default)]
            is_nsfw: bool,
            #[serde(default)]
            source: SourceNodes,
        }
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<Node>,
        }
        #[derive(Deserialize)]
        struct Data {
            extensions: Nodes,
        }
        let data: Data = self.gql(doc, json!({})).await?;
        Ok(data
            .extensions
            .nodes
            .into_iter()
            .map(|n| SuwayomiExtension {
                pkg_name: n.pkg_name,
                repo: n.repo,
                apk_name: n.apk_name,
                version_code: n.version_code,
                lang: n.lang,
                is_nsfw: n.is_nsfw,
                source_ids: n.source.nodes.into_iter().map(|s| s.id).collect(),
            })
            .collect())
    }

    // ---- Extension management (EXT-1, GQL-SCHEMA-FINDINGS.md §B) ------------

    /// Number of configured extension stores (repos). Zero on a fresh engine:
    /// no store is seeded, the operator adds their own.
    pub async fn extension_store_count(&self) -> Result<i64> {
        let doc = "query { extensionStores { totalCount } }";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Stores {
            total_count: i64,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Data {
            extension_stores: Stores,
        }
        let d: Data = self.gql(doc, json!({})).await?;
        Ok(d.extension_stores.total_count)
    }

    /// Every configured extension store (repo), in the engine's order. The engine
    /// keeps the list itself (`extensionStores`, keyed by `indexUrl`); we hold no copy,
    /// so this is the one source of truth for the admin's repo list.
    pub async fn list_extension_stores(&self) -> Result<Vec<ExtensionStore>> {
        let doc = "query { extensionStores { nodes { indexUrl name badgeLabel } } }";
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<ExtensionStore>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Data {
            extension_stores: Nodes,
        }
        let d: Data = self.gql(doc, json!({})).await?;
        Ok(d.extension_stores.nodes)
    }

    /// Remove an extension store by its index URL (`removeExtensionStore`, GQL-SCHEMA-
    /// FINDINGS §B1). The URL must be the store's own `indexUrl` as `extensionStores`
    /// reports it: the engine canonicalizes on add (Keiyoushi's `index.min.json`
    /// becomes `…/raw/repo/index.pb`), so the typed-in form may not match.
    pub async fn remove_extension_store(&self, index_url: &str) -> Result<()> {
        let doc = "mutation R($url: String!) { \
            removeExtensionStore(input: { indexUrl: $url }) { extensionStore { indexUrl } } }";
        let _: Value = self.gql(doc, json!({ "url": index_url })).await?;
        Ok(())
    }

    /// Register an extension store (repo) by its index URL. Idempotent upstream:
    /// re-adding an existing store returns it unchanged (verified on v2.3.2243).
    /// Returns the store's display name.
    pub async fn add_extension_store(&self, index_url: &str) -> Result<String> {
        let doc = "mutation A($url: String!) { \
            addExtensionStore(input: { indexUrl: $url }) { extensionStore { name } } }";
        #[derive(Deserialize)]
        struct Store {
            name: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Payload {
            extension_store: Store,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Data {
            add_extension_store: Payload,
        }
        let d: Data = self.gql(doc, json!({ "url": index_url })).await?;
        Ok(d.add_extension_store.extension_store.name)
    }

    /// Refresh the available-extension list from every configured store
    /// (`fetchExtensions` — a network fetch of each store index). Returns how
    /// many extensions are now known.
    pub async fn refresh_extensions(&self) -> Result<i64> {
        let doc = "mutation { fetchExtensions(input: {}) { extensions { pkgName } } }";
        #[derive(Deserialize)]
        struct Payload {
            extensions: Vec<Value>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Data {
            fetch_extensions: Payload,
        }
        let d: Data = self.gql(doc, json!({})).await?;
        Ok(d.fetch_extensions.extensions.len() as i64)
    }

    /// List ALL extensions known from the configured stores (installed or not) —
    /// the admin management view. `fetch_extensions` above stays the installed-only
    /// catalogue view (§2.1 coordinates); this one is the EXT-1 surface.
    pub async fn list_extensions(&self) -> Result<Vec<ExtensionListEntry>> {
        let doc = "query { extensions { \
            nodes { pkgName name lang versionName isInstalled hasUpdate isNsfw iconUrl repo } } }";
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<ExtensionListEntry>,
        }
        #[derive(Deserialize)]
        struct Data {
            extensions: Nodes,
        }
        let d: Data = self.gql(doc, json!({})).await?;
        Ok(d.extensions.nodes)
    }

    /// Shared `updateExtension(input:{id, patch})` call — `id` is the pkgName and
    /// the patch carries exactly one of install/uninstall/update (§B3). Returns
    /// the extension's post-mutation state.
    async fn patch_extension(&self, pkg_name: &str, patch: Value) -> Result<ExtensionListEntry> {
        let doc = "mutation U($id: String!, $patch: UpdateExtensionPatchInput!) { \
            updateExtension(input: { id: $id, patch: $patch }) { \
              extension { pkgName name lang versionName isInstalled hasUpdate isNsfw iconUrl repo } } }";
        #[derive(Deserialize)]
        struct Payload {
            extension: Option<ExtensionListEntry>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Data {
            update_extension: Payload,
        }
        let d: Data = self
            .gql(doc, json!({ "id": pkg_name, "patch": patch }))
            .await?;
        d.update_extension
            .extension
            .ok_or_else(|| anyhow!("Suwayomi returned no extension for {pkg_name}"))
    }

    pub async fn install_extension(&self, pkg_name: &str) -> Result<ExtensionListEntry> {
        self.patch_extension(pkg_name, json!({ "install": true }))
            .await
    }

    pub async fn uninstall_extension(&self, pkg_name: &str) -> Result<ExtensionListEntry> {
        self.patch_extension(pkg_name, json!({ "uninstall": true }))
            .await
    }

    pub async fn update_extension(&self, pkg_name: &str) -> Result<ExtensionListEntry> {
        self.patch_extension(pkg_name, json!({ "update": true }))
            .await
    }

    /// One extension by pkgName (`extension(pkgName:)`, §B2) — used to check the
    /// NSFW flag before an install/update is allowed through the posture gate.
    pub async fn get_extension(&self, pkg_name: &str) -> Result<ExtensionListEntry> {
        let doc = "query E($pkg: String!) { extension(pkgName: $pkg) { \
            pkgName name lang versionName isInstalled hasUpdate isNsfw iconUrl repo } }";
        #[derive(Deserialize)]
        struct Data {
            extension: ExtensionListEntry,
        }
        let d: Data = self.gql(doc, json!({ "pkg": pkg_name })).await?;
        Ok(d.extension)
    }

    /// List the installed Suwayomi sources (`sources` query, §B4) — the admin
    /// picker that feeds `sourceBrowse(sourceId)`. Lenient on the nested
    /// extension so a shape mismatch degrades to `pkg_name: None`, not an error.
    pub async fn list_sources(&self) -> Result<Vec<SuwayomiSource>> {
        let doc = "query { sources { nodes { \
            id name displayName lang isNsfw iconUrl extension { pkgName } } } }";
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Ext {
            pkg_name: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Node {
            id: String,
            name: String,
            #[serde(default)]
            display_name: Option<String>,
            // `#[serde(default)]` (→ "") so a single source node with a null/absent
            // `lang` can't fail the whole `list_sources` deserialization — which, now
            // that `lang` gates enrolment, would otherwise abort the entire sync pass
            // and accrue a subscription failure. An empty lang simply isn't "en".
            #[serde(default)]
            lang: String,
            is_nsfw: bool,
            #[serde(default)]
            icon_url: Option<String>,
            #[serde(default)]
            extension: Option<Ext>,
        }
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<Node>,
        }
        #[derive(Deserialize)]
        struct Data {
            sources: Nodes,
        }
        let d: Data = self.gql(doc, json!({})).await?;
        Ok(d.sources
            .nodes
            .into_iter()
            .map(|n| SuwayomiSource {
                id: n.id,
                // displayName is the user-facing one (e.g. per-language variants);
                // fall back to the raw name when absent/blank.
                name: match n.display_name {
                    Some(d) if !d.is_empty() => d,
                    _ => n.name,
                },
                lang: n.lang,
                is_nsfw: n.is_nsfw,
                icon_url: n.icon_url,
                pkg_name: n.extension.and_then(|e| e.pkg_name),
            })
            .collect())
    }

    /// A source's display name + NSFW flag + language, for gating the admin browse /
    /// ingest surfaces (a source id is user input there): NSFW on the show_nsfw
    /// posture, and language on the English-only ingest policy.
    pub async fn source_meta(&self, source_id: &str) -> Result<(String, bool, String)> {
        let doc = "query S($id: LongString!) { source(id: $id) { displayName isNsfw lang } }";
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Src {
            display_name: String,
            is_nsfw: bool,
            #[serde(default)]
            lang: String,
        }
        #[derive(Deserialize)]
        struct Data {
            source: Src,
        }
        let d: Data = self.gql(doc, json!({ "id": source_id })).await?;
        Ok((d.source.display_name, d.source.is_nsfw, d.source.lang))
    }

    pub async fn set_in_library(&self, id: i64, in_library: bool) -> Result<()> {
        let doc = "mutation U($id: Int!, $inLibrary: Boolean!) { updateManga(input: { id: $id, patch: { inLibrary: $inLibrary } }) { manga { id } } }";
        let _: Value = self
            .gql(doc, json!({ "id": id, "inLibrary": in_library }))
            .await?;
        Ok(())
    }

    // NOTE: reading progress is no longer pushed back to Suwayomi — it is per-user and
    // lives in `suwayomi_progress` (see the `set_progress` GraphQL mutation). Suwayomi
    // is a content source only, so the old `updateChapter` progress mutation was removed.
}

/// A minimal in-process HTTP/1.1 origin, for exercising the cover-fetch path (bounded
/// pool, short timeout, truncation gate) end-to-end without a real Suwayomi engine.
/// Raw TCP rather than a mock-server crate so the test suite gains no dependency.
#[cfg(test)]
pub(crate) mod testsrv {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A running test origin: `base_url` to point a `SuwayomiClient` at, plus a live
    /// count of requests it has served.
    pub struct TestOrigin {
        pub base_url: String,
        pub hits: Arc<AtomicUsize>,
        /// The requests in `hits` that were not GraphQL (`POST /api/graphql`): image
        /// counts stay exact when a client also asks the engine for a manga's details.
        pub image_hits: Arc<AtomicUsize>,
        /// Highest number of requests that were in flight at the same instant — the
        /// observable that proves the client-side semaphore bounds concurrency.
        pub peak_concurrent: Arc<AtomicUsize>,
    }

    /// Serve `body` as `image/jpeg` to every request, after `delay`.
    pub async fn spawn(body: Vec<u8>, delay: Duration) -> TestOrigin {
        spawn_status(200, "image/jpeg", body, delay).await
    }

    /// Serve a fixed HTTP `status` (with `body` as `content_type`) to every request.
    /// Lets a test drive the cover pipeline's failure CLASSIFICATION — a 400 carrying
    /// Suwayomi's `data:` message is permanent, a 403/503 is transient.
    pub async fn spawn_error(status: u16, body: &str) -> TestOrigin {
        spawn_status(
            status,
            "text/plain",
            body.as_bytes().to_vec(),
            Duration::ZERO,
        )
        .await
    }

    pub async fn spawn_status(
        status: u16,
        content_type: &str,
        body: Vec<u8>,
        delay: Duration,
    ) -> TestOrigin {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let image_hits = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (h, g, i, p) = (
            hits.clone(),
            image_hits.clone(),
            inflight.clone(),
            peak.clone(),
        );
        let body = Arc::new(body);
        let content_type = Arc::new(content_type.to_string());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (h, g, i, p, body) = (h.clone(), g.clone(), i.clone(), p.clone(), body.clone());
                let ct = content_type.clone();
                tokio::spawn(async move {
                    // Drain the request head; only GraphQL is told apart.
                    let mut buf = [0u8; 2048];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let graphql = buf[..n].starts_with(b"POST /api/graphql");
                    h.fetch_add(1, Ordering::SeqCst);
                    if !graphql {
                        g.fetch_add(1, Ordering::SeqCst);
                    }
                    let now = i.fetch_add(1, Ordering::SeqCst) + 1;
                    p.fetch_max(now, Ordering::SeqCst);
                    if !delay.is_zero() && !graphql {
                        tokio::time::sleep(delay).await;
                    }
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.flush().await;
                    i.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        TestOrigin {
            base_url: format!("http://127.0.0.1:{port}"),
            hits,
            image_hits,
            peak_concurrent: peak,
        }
    }

    /// A real, complete JPEG of the given size (ends with the `FFD9` EOI marker).
    pub fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([
                ((x * 73 + y * 151) % 256) as u8,
                ((x * 199 + y * 37) % 256) as u8,
                ((x ^ (y.wrapping_mul(101))) % 256) as u8,
            ]);
        }
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut out),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        assert_eq!(
            &out[out.len() - 2..],
            &[0xFF, 0xD9],
            "test JPEG must be whole"
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn client(base: &str) -> SuwayomiClient {
        SuwayomiClient::new(base.to_string(), None, Some("1".into()))
    }

    #[tokio::test]
    async fn image_fetches_reject_empty_challenges_mismatched_and_truncated_bodies() {
        let jpeg = testsrv::jpeg(8, 8);
        let cases = [
            (Vec::new(), "image/jpeg"),
            (b"<html>challenge</html>".to_vec(), "image/jpeg"),
            (jpeg[..jpeg.len() - 2].to_vec(), "image/jpeg"),
            (jpeg.clone(), "image/png"),
            (jpeg, "text/html"),
        ];
        for (body, mime) in cases {
            let origin = testsrv::spawn_status(200, mime, body, Duration::ZERO).await;
            let c = client(&origin.base_url);
            assert!(
                matches!(
                    c.fetch_cover_now("/cover").await,
                    Err(CoverFetchError::Upstream(_))
                ),
                "{mime}"
            );
            assert!(
                matches!(
                    c.fetch_page_now("/page").await,
                    Err(PageFetchError::Upstream(_))
                ),
                "{mime}"
            );
            assert_eq!(c.page_sem.available_permits(), PAGE_FETCH_CONCURRENCY);
        }
    }

    #[tokio::test]
    async fn fetched_page_keeps_its_slot_until_every_byte_owner_is_dropped() {
        let jpeg = testsrv::jpeg(8, 8);
        let origin = testsrv::spawn_status(
            200,
            "application/octet-stream",
            jpeg.clone(),
            Duration::ZERO,
        )
        .await;
        let c = client(&origin.base_url);
        let (bytes, mime) = c.fetch_page_now("/page").await.unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(bytes.as_ref(), jpeg);
        let clone = bytes.clone();
        drop(bytes);
        assert_eq!(c.page_sem.available_permits(), PAGE_FETCH_CONCURRENCY - 1);
        drop(clone);
        assert_eq!(c.page_sem.available_permits(), PAGE_FETCH_CONCURRENCY);
    }

    #[tokio::test]
    async fn page_slot_telemetry_counts_wait_only_for_an_obtained_slot() {
        let stats = crate::telemetry::AdmissionStats::leak();
        let lane = stats.lane(crate::telemetry::Lane::PageFetch);
        let slots = Arc::new(Semaphore::new(1));
        let held = acquire_page_slot(lane, &slots, std::time::Duration::from_millis(50))
            .await
            .unwrap();
        let refused = acquire_page_slot(lane, &slots, std::time::Duration::from_millis(30)).await;
        assert!(matches!(refused, Err(PageFetchError::Busy)));
        // (admitted, refused, in flight, high water); fetch slots are not held here.
        assert_eq!(lane.snapshot(), (1, 1, 0, 0));
        assert!(
            lane.wait_micros() < 30_000,
            "a refused wait is not counted: {}us",
            lane.wait_micros()
        );
        let waiter = {
            let slots = slots.clone();
            tokio::spawn(async move {
                acquire_page_slot(lane, &slots, std::time::Duration::from_secs(5))
                    .await
                    .is_ok()
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        drop(held);
        assert!(waiter.await.unwrap());
        assert_eq!(lane.snapshot(), (2, 1, 0, 0));
        assert!(lane.wait_micros() >= 25_000, "{}us", lane.wait_micros());
    }

    #[tokio::test]
    async fn cancelled_page_fetch_releases_its_slot() {
        let origin = testsrv::spawn_status(
            200,
            "image/jpeg",
            testsrv::jpeg(8, 8),
            Duration::from_secs(30),
        )
        .await;
        let c = client(&origin.base_url);
        let fetch = {
            let c = c.clone();
            tokio::spawn(async move { c.fetch_page_now("/page").await })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while origin.hits.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("page fetch never reached the origin");
        assert_eq!(c.page_sem.available_permits(), PAGE_FETCH_CONCURRENCY - 1);
        // A client disconnect or the route deadline drops the handler future.
        fetch.abort();
        assert!(fetch.await.unwrap_err().is_cancelled());
        assert_eq!(c.page_sem.available_permits(), PAGE_FETCH_CONCURRENCY);
    }

    /// A garbled copy of `whole`: a chunk of scan data is missing, as when two
    /// downloads wrote into the engine's one temp file at once.
    fn garbled(whole: &[u8]) -> Vec<u8> {
        let cut = whole.len() / 2;
        let mut out = whole[..cut].to_vec();
        out.extend_from_slice(&whole[cut + 512..]);
        out
    }

    /// A fake engine: thumbnails answer `garbled` until a details fetch
    /// (`fetchMangaAndChapters`) drops the saved copy, as Suwayomi's `clearThumbnail`
    /// does. Returns the base URL and the counts of thumbnail and details requests,
    /// plus the highest number of thumbnail requests at once.
    async fn fake_engine(
        whole: Vec<u8>,
        details_reach_source: bool,
        delay: Duration,
    ) -> (String, Arc<[std::sync::atomic::AtomicUsize; 4]>) {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        // [thumbnails, details, in flight, peak in flight]
        let counts: Arc<[AtomicUsize; 4]> = Arc::new(Default::default());
        let fresh = Arc::new(AtomicBool::new(false));
        let bad = garbled(&whole);
        let app = axum::Router::new()
            .route(
                "/api/v1/manga/{id}/thumbnail",
                axum::routing::get({
                    let (counts, fresh) = (counts.clone(), fresh.clone());
                    move || {
                        let (counts, fresh) = (counts.clone(), fresh.clone());
                        let (whole, bad) = (whole.clone(), bad.clone());
                        async move {
                            counts[0].fetch_add(1, Ordering::SeqCst);
                            let now = counts[2].fetch_add(1, Ordering::SeqCst) + 1;
                            counts[3].fetch_max(now, Ordering::SeqCst);
                            tokio::time::sleep(delay).await;
                            counts[2].fetch_sub(1, Ordering::SeqCst);
                            let body = if fresh.load(Ordering::SeqCst) {
                                whole
                            } else {
                                bad
                            };
                            ([(axum::http::header::CONTENT_TYPE, "image/jpeg")], body)
                        }
                    }
                }),
            )
            .route(
                "/api/graphql",
                axum::routing::post({
                    let (counts, fresh) = (counts.clone(), fresh.clone());
                    move |axum::Json(request): axum::Json<Value>| {
                        let (counts, fresh) = (counts.clone(), fresh.clone());
                        async move {
                            let id = request["variables"]["id"].as_i64().unwrap_or(0);
                            let manga = json!({"id": id, "title": "Series", "url": "/manga/1",
                                "thumbnailUrl": format!("/api/v1/manga/{id}/thumbnail"), "status": "ONGOING",
                                "inLibrary": true, "sourceId": "1", "source": {"lang": "en"}});
                            let query = request["query"].as_str().unwrap_or("");
                            axum::Json(if !query.contains("fetchMangaAndChapters") {
                                json!({"data": {"manga": manga}})
                            } else if details_reach_source {
                                counts[1].fetch_add(1, Ordering::SeqCst);
                                fresh.store(true, Ordering::SeqCst);
                                json!({"data": {"fetchMangaAndChapters": {"manga": manga}}})
                            } else {
                                counts[1].fetch_add(1, Ordering::SeqCst);
                                json!({"errors": [{"message": "source unreachable"}]})
                            })
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, counts)
    }

    #[tokio::test]
    async fn garbled_saved_thumbnail_is_downloaded_again_once() {
        use std::sync::atomic::Ordering;
        let whole = testsrv::jpeg(200, 300);
        let (url, counts) = fake_engine(whole.clone(), true, Duration::ZERO).await;
        let c = client(&url);
        let (bytes, _) = c
            .fetch_cover_now("/api/v1/manga/5/thumbnail")
            .await
            .expect("the repaired thumbnail is served");
        assert_eq!(bytes, whole);
        assert_eq!(counts[0].load(Ordering::SeqCst), 2, "garbled, then fresh");
        assert_eq!(counts[1].load(Ordering::SeqCst), 1, "one details fetch");
        // The pHash path shares the turn and the check.
        assert_eq!(
            c.cover_bytes(Some("/api/v1/manga/5/thumbnail")).await,
            Some(whole)
        );
    }

    #[tokio::test]
    async fn garbled_thumbnail_is_refused_and_repair_is_rate_limited() {
        use std::sync::atomic::Ordering;
        let (url, counts) = fake_engine(testsrv::jpeg(200, 300), false, Duration::ZERO).await;
        let c = client(&url);
        let first = c.fetch_cover_background("/api/v1/manga/6/thumbnail").await;
        assert!(
            first.is_err(),
            "the source was unreachable, the copy is still garbled"
        );
        assert_eq!(counts[1].load(Ordering::SeqCst), 1);
        let second = c.fetch_cover_background("/api/v1/manga/6/thumbnail").await;
        let error = second
            .expect_err("garbled bytes are never served")
            .to_string();
        assert!(error.contains("garbled"), "{error}");
        assert_eq!(
            counts[1].load(Ordering::SeqCst),
            1,
            "no second repair within the hour"
        );
        assert_eq!(c.cover_bytes(Some("/api/v1/manga/6/thumbnail")).await, None);
        // An admin's explicit refresh is not rate limited, and reports the failure.
        assert!(c.refresh_thumbnail(6).await.is_err());
        assert_eq!(counts[1].load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn thumbnail_requests_for_one_manga_take_turns() {
        use std::sync::atomic::Ordering;
        let whole = testsrv::jpeg(64, 96);
        let (url, counts) = fake_engine(whole, true, Duration::from_millis(150)).await;
        let c = client(&url);
        c.refresh_thumbnail(8).await.unwrap(); // serve whole images from here on
        let mut handles = Vec::new();
        for i in 0..4 {
            let c = c.clone();
            handles.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    c.fetch_cover_background("/api/v1/manga/8/thumbnail")
                        .await
                        .is_ok()
                } else {
                    c.cover_bytes(Some("/api/v1/manga/8/thumbnail"))
                        .await
                        .is_some()
                }
            }));
        }
        for h in handles {
            assert!(h.await.unwrap());
        }
        assert_eq!(counts[0].load(Ordering::SeqCst), 4);
        assert_eq!(
            counts[3].load(Ordering::SeqCst),
            1,
            "never two at once for one manga"
        );
        // Different manga still run side by side.
        let (a, b) = tokio::join!(
            c.fetch_cover_background("/api/v1/manga/8/thumbnail"),
            c.fetch_cover_background("/api/v1/manga/9/thumbnail"),
        );
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(counts[3].load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn error_prefix_does_not_wait_for_an_unbounded_response_tail() {
        use futures::StreamExt;
        let app = axum::Router::new().fallback(axum::routing::get(|| async {
            let prefix = futures::stream::once(async {
                Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(vec![b'x'; 4096]))
            });
            (
                axum::http::StatusCode::BAD_GATEWAY,
                axum::body::Body::from_stream(prefix.chain(futures::stream::pending())),
            )
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let c = client(&format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let error = tokio::time::timeout(Duration::from_secs(2), c.fetch_page_now("/page"))
            .await
            .expect("page error tried to consume the stalled tail")
            .unwrap_err();
        assert!(error.to_string().contains(&"x".repeat(160)));
        let error = tokio::time::timeout(Duration::from_secs(2), c.fetch_cover_now("/cover"))
            .await
            .expect("cover error tried to consume the stalled tail")
            .unwrap_err();
        assert!(error.to_string().contains(&"x".repeat(160)));
        server.abort();
    }

    /// An engine that answers every GraphQL request with `body`.
    async fn engine_answering(body: &str) -> testsrv::TestOrigin {
        testsrv::spawn_status(
            200,
            "application/json",
            body.as_bytes().to_vec(),
            Duration::ZERO,
        )
        .await
    }

    #[test]
    fn old_cached_chapter_json_deserializes_without_source_keys() {
        let chapter: SuwayomiChapter = serde_json::from_str(
            r#"{
            "id": 6, "mangaId": 1, "name": "Chapter 1", "chapterNumber": 1,
            "scanlator": null, "uploadDate": null, "isRead": false,
            "isBookmarked": false, "isDownloaded": false, "lastPageRead": 0, "pageCount": 10
        }"#,
        )
        .unwrap();
        assert_eq!(chapter.url, None);
        assert_eq!(chapter.real_url, None);
        assert_eq!(chapter.source_order, None);
    }

    #[tokio::test]
    async fn reader_page_and_positive_owner_memos_are_shared_by_clones() {
        use std::sync::atomic::Ordering;
        let engine = engine_answering(
            r#"{"data":{"fetchChapterPages":{"pages":["/api/v1/manga/42/chapter/1/page/0"]},"chapters":{"nodes":[{"mangaId":42}]}}}"#,
        )
        .await;
        let original = client(&engine.base_url);
        let clone = original.clone();
        let (first, same) = tokio::join!(original.pages(1), clone.pages(1));
        assert_eq!(first.unwrap(), same.unwrap());
        let (first, same) = tokio::join!(original.chapter_manga_id(1), clone.chapter_manga_id(1));
        assert_eq!(first.unwrap(), Some(42));
        assert_eq!(same.unwrap(), Some(42));
        assert_eq!(engine.hits.load(Ordering::SeqCst), 2);
        assert_eq!(clone.pages(1).await.unwrap().len(), 1);
        assert_eq!(clone.chapter_manga_id(1).await.unwrap(), Some(42));
        assert_eq!(
            engine.hits.load(Ordering::SeqCst),
            2,
            "warm chapter reached engine"
        );
    }

    #[tokio::test]
    async fn unknown_chapter_owners_are_never_memoized() {
        use std::sync::atomic::Ordering;
        let engine = engine_answering(r#"{"data":{"chapters":{"nodes":[]}}}"#).await;
        let client = client(&engine.base_url);
        assert_eq!(client.chapter_manga_id(999).await.unwrap(), None);
        assert_eq!(client.chapter_manga_id(999).await.unwrap(), None);
        assert_eq!(engine.hits.load(Ordering::SeqCst), 2);
    }

    /// The body is what Suwayomi v2.3.2243 really answers for this document (captured by
    /// `apps/reader/src-tauri/e2e/native-read`, which also runs the document live). MANGA
    /// Plus on purpose: its keys are `#/…` fragments, nothing like a row id or a path.
    #[tokio::test]
    async fn chapter_origin_reads_the_sources_own_keys() {
        let engine = engine_answering(
            r##"{"data":{"chapters":{"nodes":[{"url":"#/viewer/1030381",
                "realUrl":"https://mangaplus.shueisha.co.jp/viewer/1030381","sourceOrder":5,"mangaId":1,
                "manga":{"url":"#/titles/100020",
                         "realUrl":"https://mangaplus.shueisha.co.jp/titles/100020",
                         "title":"One Piece","sourceId":"1998944621602463790"}}]}}}"##,
        )
        .await;
        let origin = client(&engine.base_url).chapter_origin(6).await.unwrap();
        assert_eq!(
            origin,
            Some(ChapterOrigin {
                manga_id: 1,
                source_id: "1998944621602463790".into(),
                manga_url: "#/titles/100020".into(),
                manga_title: "One Piece".into(),
                chapter_url: "#/viewer/1030381".into(),
                chapter_web_url: Some("https://mangaplus.shueisha.co.jp/viewer/1030381".into()),
                manga_web_url: Some("https://mangaplus.shueisha.co.jp/titles/100020".into()),
                source_order: 5,
            })
        );
    }

    /// A source with no website (the local source) answers `realUrl: null`, and an
    /// engine that omits the field altogether must read the same way.
    #[tokio::test]
    async fn chapter_origin_tolerates_a_missing_web_url() {
        let engine = engine_answering(
            r##"{"data":{"chapters":{"nodes":[{"url":"/ch/1","realUrl":null,"sourceOrder":0,"mangaId":1,
                "manga":{"url":"/m/1","title":"Local","sourceId":"0"}}]}}}"##,
        )
        .await;
        let origin = client(&engine.base_url)
            .chapter_origin(6)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(origin.chapter_web_url, None);
        assert_eq!(origin.manga_web_url, None);
    }

    #[tokio::test]
    async fn chapter_origin_is_none_for_an_unknown_chapter() {
        let engine = engine_answering(r#"{"data":{"chapters":{"nodes":[]}}}"#).await;
        let origin = client(&engine.base_url).chapter_origin(6).await.unwrap();
        assert_eq!(origin, None);
    }

    /// The self-host image sets `SUWAYOMI_PUBLIC_URL=/` so cover and page URLs stay
    /// relative to whatever address the browser used (localhost, a LAN IP, a domain),
    /// and the server's own `/api/v1/manga/...` proxy serves them.
    #[test]
    fn root_public_url_keeps_image_urls_relative() {
        let c = SuwayomiClient::new("http://suwayomi:4567".into(), Some("/".into()), None);
        assert_eq!(
            c.abs(Some("/api/v1/manga/7/thumbnail")),
            "/api/v1/manga/7/thumbnail"
        );
        assert_eq!(
            c.abs(Some("https://cdn.example/x.jpg")),
            "https://cdn.example/x.jpg"
        );
        let hosted = SuwayomiClient::new(
            "http://suwayomi:4567".into(),
            Some("https://api.komiq.cc".into()),
            None,
        );
        assert_eq!(
            hosted.abs(Some("/api/v1/manga/7/thumbnail")),
            "https://api.komiq.cc/api/v1/manga/7/thumbnail"
        );
    }

    /// The bounded pool is the fix for the 22 s p90: a burst larger than
    /// `COVER_FETCH_CONCURRENCY` must be REFUSED at the (COVER_FETCH_CONCURRENCY+1)-th
    /// request rather than queued behind the in-flight ones.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cover_fetch_semaphore_bounds_concurrency_and_fails_fast() {
        let body = testsrv::jpeg(64, 96);
        // Slow origin: every request is held long enough for the whole burst to overlap.
        let origin = testsrv::spawn(body, Duration::from_millis(1500)).await;
        let c = client(&origin.base_url);

        // Saturate: exactly COVER_FETCH_CONCURRENCY in-flight fetches.
        let mut handles = Vec::new();
        // One manga each: requests for the same manga take turns (`ThumbnailTurns`).
        for i in 0..COVER_FETCH_CONCURRENCY {
            let c = c.clone();
            handles.push(tokio::spawn(async move {
                let path = format!("/api/v1/manga/{}/thumbnail", 10 + i);
                c.fetch_cover_now(&path).await.is_ok()
            }));
        }
        // Wait for every permit to be taken (not for the fetches to finish).
        for _ in 0..200 {
            if c.cover_permits_available() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            c.cover_permits_available(),
            0,
            "burst should have taken every cover permit"
        );

        // The next on-demand request must be refused IMMEDIATELY, not queued.
        let t = std::time::Instant::now();
        let over = c.fetch_cover_now("/api/v1/manga/2/thumbnail").await;
        let waited = t.elapsed();
        assert!(
            matches!(over, Err(CoverFetchError::Busy)),
            "over-limit on-demand fetch must report Busy, got {over:?}"
        );
        assert!(
            waited < Duration::from_millis(250),
            "Busy must be returned without queueing, took {waited:?}"
        );

        for h in handles {
            assert!(h.await.unwrap(), "saturating fetches should still succeed");
        }
        // Permits are released on drop, so the pool is reusable afterwards.
        assert_eq!(c.cover_permits_available(), COVER_FETCH_CONCURRENCY);
        assert!(
            origin
                .peak_concurrent
                .load(std::sync::atomic::Ordering::SeqCst)
                <= COVER_FETCH_CONCURRENCY,
            "origin saw more concurrent requests than the pool allows"
        );
    }

    /// A well-formed HTTP 200 carrying a TRUNCATED image must be rejected at the fetch,
    /// so the partial bytes never reach `process_cover` and get frozen into the cache
    /// behind a one-year immutable TTL. `zune-jpeg` would decode them to `Ok`.
    #[tokio::test]
    async fn cover_fetch_rejects_truncated_jpeg() {
        let whole = testsrv::jpeg(200, 300);
        let truncated = whole[..whole.len() * 6 / 10].to_vec();
        assert!(
            image::load_from_memory(&truncated).is_ok(),
            "precondition: the decoder itself accepts this truncated JPEG"
        );
        let origin = testsrv::spawn(truncated, Duration::ZERO).await;
        let c = client(&origin.base_url);

        let res = c.fetch_cover_now("/api/v1/manga/1/thumbnail").await;
        match res {
            Err(CoverFetchError::Upstream(e)) => {
                assert!(
                    e.to_string().contains("truncated"),
                    "expected a truncation error, got: {e}"
                );
            }
            other => panic!("truncated source must be rejected, got {other:?}"),
        }
        // The `cover_bytes` (thumbnail-URL) entry point gates it too.
        assert!(
            c.cover_bytes(Some("/api/v1/manga/1/thumbnail"))
                .await
                .is_none(),
            "cover_bytes must also reject a truncated source"
        );
    }

    /// A whole image round-trips through the gated path unchanged.
    #[tokio::test]
    async fn cover_fetch_accepts_a_whole_jpeg() {
        let whole = testsrv::jpeg(120, 160);
        let origin = testsrv::spawn(whole.clone(), Duration::ZERO).await;
        let c = client(&origin.base_url);
        let (bytes, ct) = c
            .fetch_cover_now("/api/v1/manga/1/thumbnail")
            .await
            .expect("whole jpeg accepted");
        assert_eq!(bytes, whole, "bytes must be passed through exactly");
        assert_eq!(ct, "image/jpeg");
    }

    /// The background pool is bounded independently, so a burst of on-demand misses
    /// can't spawn unbounded detached materialization tasks.
    #[tokio::test]
    async fn background_slots_are_bounded() {
        let c = client("http://127.0.0.1:1");
        let slots: Vec<_> = (0..BG_MATERIALIZE_CONCURRENCY)
            .map(|_| c.try_background_slot().expect("slot available"))
            .collect();
        assert!(
            c.try_background_slot().is_none(),
            "background pool must refuse past its bound"
        );
        drop(slots);
        assert!(c.try_background_slot().is_some(), "slots free on drop");
    }
}
