//! Server-side federation client for a Suwayomi/Tachidesk server.
//!
//! Mirrors the TS `SuwayomiBackend` adapter: it talks Suwayomi's real GraphQL and
//! returns raw Suwayomi shapes, which `graphql` maps onto the Komika contract.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

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

const MANGA_FIELDS: &str = r#"
fragment MangaFields on MangaType {
    id title url thumbnailUrl author artist description genre status
    inLibrary inLibraryAt lastFetchedAt sourceId
    source { lang }
    chapters { totalCount }
}"#;

const CHAPTER_FIELDS: &str = r#"
fragment ChapterFields on ChapterType {
    id mangaId name chapterNumber scanlator uploadDate
    isRead isBookmarked isDownloaded lastPageRead pageCount
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
pub struct SuwayomiChapter {
    pub id: i64,
    pub manga_id: i64,
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

#[derive(Clone)]
pub struct SuwayomiClient {
    base_url: String,
    /// Public base used when building image URLs (covers/pages) handed to the
    /// browser. Defaults to `base_url` when no public URL is configured.
    image_base_url: String,
    http: reqwest::Client,
    /// Cached resolved source id; `Some` once resolved. `Arc<Mutex>` so cheap clones
    /// (e.g. into the cover drainer task) share the one resolved id rather than each
    /// re-resolving it against the engine.
    source_id: std::sync::Arc<Mutex<Option<String>>>,
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
    /// Separate HTTP client for PAGE fetches — its own timeout ([`PAGE_TIMEOUT_SECS`]),
    /// between the cover and scan clients.
    page_http: reqwest::Client,
    /// Global bound on in-flight upstream PAGE fetches (see [`PAGE_FETCH_CONCURRENCY`]).
    /// Shared by every clone of the client; separate from `cover_sem`.
    page_sem: Arc<Semaphore>,
}

impl SuwayomiClient {
    pub fn new(
        base_url: String,
        public_url: Option<String>,
        configured_source: Option<String>,
    ) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        let image_base_url = public_url
            .map(|u| u.trim_end_matches('/').to_string())
            .unwrap_or_else(|| base_url.clone());
        // Bound every request so a hung/slow upstream (Suwayomi proxies to source
        // sites, often via FlareSolverr, which routinely stalls) can't freeze the
        // scan scheduler or a reader cache-miss path forever. Mirrors
        // `MangaDexClient::new`. Falls back to the default client if the builder
        // somehow fails, so construction stays infallible.
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        // Cover client: same host, much less patience. A cover is on the critical path
        // of a page render, so a fast failure that the drainer/warmer can retry beats a
        // 29 s hang the browser renders as a broken image regardless.
        let cover_http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(COVER_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| http.clone());
        // Page client: full-resolution content, so more patient than covers but still
        // short of the scan client, and bounded by its own pool below.
        let page_http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(PAGE_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| http.clone());
        Self {
            base_url,
            image_base_url,
            http,
            source_id: std::sync::Arc::new(Mutex::new(configured_source.clone())),
            configured_source,
            cover_http,
            cover_sem: Arc::new(Semaphore::new(COVER_FETCH_CONCURRENCY)),
            bg_sem: Arc::new(Semaphore::new(BG_MATERIALIZE_CONCURRENCY)),
            page_http,
            page_sem: Arc::new(Semaphore::new(PAGE_FETCH_CONCURRENCY)),
        }
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

    /// Fetch a cover thumbnail's raw bytes for server-side pHash (dedup signal,
    /// DD1). Uses the INTERNAL `base_url` (this runs server-side, not in the
    /// browser, so the public image host may be unreachable). Best-effort: returns
    /// `None` on any missing URL / network / non-success — a missing hash just
    /// means one fewer dedup signal, never a failure.
    pub async fn cover_bytes(&self, thumbnail_url: Option<&str>) -> Option<Vec<u8>> {
        let raw = thumbnail_url?;
        if raw.is_empty() {
            return None;
        }
        let url = if raw.starts_with("http") {
            raw.to_string()
        } else {
            format!("{}{}", self.base_url, raw)
        };
        // Patient acquire. Two classes of caller, and only one is fan-out bounded:
        // the background crawl/warmer holds at most `WARM_COVER_CONCURRENCY` (enforced
        // by its `buffer_unordered`), but the pHash enrol paths in `graphql/mod.rs`
        // (`add_source_series`, `ingest_source_series`, `federated_ingest`) call this
        // directly and are bounded only by being strictly sequential — one permit per
        // in-flight request. They are NOT counted in the headroom arithmetic documented
        // at the top of this module; see it for the real on-demand floor. If enrolment
        // is ever parallelised it must take a background slot instead of competing here.
        let _permit = self.cover_sem.clone().acquire_owned().await.ok()?;
        let res = self.cover_http.get(url).send().await.ok()?;
        if !res.status().is_success() {
            return None;
        }
        let bytes = read_capped(res, MAX_COVER_SOURCE_BYTES).await.ok()?;
        // A well-formed HTTP response can still carry an INCOMPLETE image: the source
        // CDN (or Suwayomi's proxy of it) closes the body early and `zune-jpeg` decodes
        // the partial data to `Ok` with a flat decoder-fill tail. Reject it here so the
        // truncated bytes never reach `process_cover` and get frozen into the cache.
        if let Err(e) = crate::avatar::ensure_complete(&bytes) {
            tracing::warn!(error = %e, "suwayomi cover: rejected truncated source");
            return None;
        }
        Some(bytes)
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
    pub async fn fetch_cover_now(
        &self,
        path: &str,
    ) -> std::result::Result<(Vec<u8>, String), CoverFetchError> {
        let Ok(_permit) = self.cover_sem.clone().try_acquire_owned() else {
            return Err(CoverFetchError::Busy);
        };
        self.fetch_cover_inner(path)
            .await
            .map_err(CoverFetchError::Upstream)
    }

    /// Fetch a cover image from the internal Suwayomi REST path for BACKGROUND work
    /// (warmer / detached materialization). Waits for a permit rather than failing —
    /// background work is patient — but the caller must bound its own fan-out to
    /// [`WARM_COVER_CONCURRENCY`] so on-demand requests keep their headroom.
    pub async fn fetch_cover_background(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let _permit = self
            .cover_sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("cover fetch semaphore closed"))?;
        self.fetch_cover_inner(path).await
    }

    /// Shared body of the two cover fetchers. Assumes the caller holds a permit.
    async fn fetch_cover_inner(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let url = format!("{}{}", self.base_url, path);
        let res = self.cover_http.get(url).send().await?;
        if !res.status().is_success() {
            return Err(anyhow!("Suwayomi cover error {}", res.status()));
        }
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .filter(|ct| ct.starts_with("image/"))
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = read_capped(res, MAX_COVER_SOURCE_BYTES).await?;
        crate::avatar::ensure_complete(&bytes)?;
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
    ) -> std::result::Result<(Vec<u8>, String), PageFetchError> {
        // Held for the whole fetch below (bound to a named local, not `_`, so it is not
        // dropped early) — that is what makes the concurrency bound effective.
        let _permit = match tokio::time::timeout(
            std::time::Duration::from_secs(PAGE_ACQUIRE_WAIT_SECS),
            self.page_sem.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            // Semaphore closed (shutdown) — treat as an upstream failure, not a retry.
            Ok(Err(e)) => return Err(PageFetchError::Upstream(anyhow!(e))),
            // Waited past the cap without a slot — shed, retryably.
            Err(_) => return Err(PageFetchError::Busy),
        };
        self.fetch_page_inner(path)
            .await
            .map_err(PageFetchError::Upstream)
    }

    /// Shared body of the page fetcher. Assumes the caller holds a page-pool permit.
    async fn fetch_page_inner(&self, path: &str) -> Result<(Vec<u8>, String)> {
        let url = format!("{}{}", self.base_url, path);
        let res = self.page_http.get(url).send().await?;
        if !res.status().is_success() {
            return Err(anyhow!("Suwayomi page error {}", res.status()));
        }
        let content_type = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .filter(|ct| ct.starts_with("image/"))
            .unwrap_or("image/jpeg")
            .to_string();
        let bytes = read_capped(res, MAX_PAGE_SOURCE_BYTES).await?;
        Ok((bytes, content_type))
    }

    async fn gql<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T> {
        let res = self
            .http
            .post(self.endpoint())
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(anyhow!("Suwayomi error {}", res.status()));
        }
        let body: Value = res.json().await?;
        if let Some(errors) = body.get("errors").and_then(|e| e.as_array()) {
            if !errors.is_empty() {
                let msg = errors
                    .iter()
                    .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(anyhow!("{msg}"));
            }
        }
        let data = body
            .get("data")
            .cloned()
            .ok_or_else(|| anyhow!("Suwayomi returned no data"))?;
        Ok(serde_json::from_value(data)?)
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
        if let Some(id) = self.source_id.lock().await.as_ref() {
            return Ok(id.clone());
        }
        // A configured source needs no network round-trip; cache and return.
        if let Some(id) = &self.configured_source {
            let mut guard = self.source_id.lock().await;
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
        let mut guard = self.source_id.lock().await;
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
        let data: Data = self
            .gql(
                &doc,
                json!({ "source": source_id, "type": ty.as_str(), "page": page, "query": query }),
            )
            .await?;
        Ok((
            data.fetch_source_manga.has_next_page,
            parse_records::<SuwayomiManga>(data.fetch_source_manga.mangas, "browse_manga"),
        ))
    }

    /// Fetch full detail from the source (populates genres/status/description),
    /// falling back to the DB-cached manga if the source fetch fails.
    pub async fn series(&self, id: i64) -> Result<SuwayomiManga> {
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
            Ok(d) => Ok(d.f.manga),
            Err(_) => {
                let doc = format!(
                    "{MANGA_FIELDS}\nquery M($id: Int!) {{ manga(id: $id) {{ ...MangaFields }} }}"
                );
                #[derive(Deserialize)]
                struct Data {
                    manga: SuwayomiManga,
                }
                let d: Data = self.gql(&doc, json!({ "id": id })).await?;
                Ok(d.manga)
            }
        }
    }

    pub async fn chapters(&self, series_id: i64) -> Result<Vec<SuwayomiChapter>> {
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
        match self
            .gql::<FetchData>(&fetch, json!({ "id": series_id }))
            .await
        {
            Ok(d) => Ok(parse_records::<SuwayomiChapter>(
                d.f.chapters.unwrap_or_default(),
                "chapters",
            )),
            Err(e) => {
                if e.to_string().contains("No chapters") {
                    return Ok(vec![]);
                }
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
                Ok(parse_records::<SuwayomiChapter>(
                    d.chapters.nodes,
                    "chapters",
                ))
            }
        }
    }

    /// Fetch fresh manga detail AND its chapter list in ONE upstream round-trip
    /// (`fetchMangaAndChapters` with both flags). The DB-driven scanner uses this so a
    /// due series costs a single engine call for current status (pause re-check) + the
    /// chapter list, instead of two. Falls back to the separate `series` + `chapters`
    /// calls if the combined mutation fails (older engine / partial support).
    pub async fn series_and_chapters(
        &self,
        id: i64,
    ) -> Result<(SuwayomiManga, Vec<SuwayomiChapter>)> {
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
        match self.gql::<Data>(&doc, json!({ "id": id })).await {
            Ok(d) => Ok((
                d.f.manga,
                parse_records::<SuwayomiChapter>(d.f.chapters.unwrap_or_default(), "chapters"),
            )),
            Err(_) => {
                // Fallback: two calls (each carries its own older-engine fallback).
                let m = self.series(id).await?;
                let chapters = self.chapters(id).await?;
                Ok((m, chapters))
            }
        }
    }

    pub async fn pages(&self, chapter_id: i64) -> Result<Vec<String>> {
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

    /// Number of configured extension stores (repos). Used by the idempotent
    /// Keiyoushi default-store seeding: Suwayomi canonicalizes the index URL on
    /// add (min.json → index.pb), so presence can't be checked by URL equality.
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
        /// Highest number of requests that were in flight at the same instant — the
        /// observable that proves the client-side semaphore bounds concurrency.
        pub peak_concurrent: Arc<AtomicUsize>,
    }

    /// Serve `body` as `image/jpeg` to every request, after `delay`.
    pub async fn spawn(body: Vec<u8>, delay: Duration) -> TestOrigin {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (h, i, p) = (hits.clone(), inflight.clone(), peak.clone());
        let body = Arc::new(body);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (h, i, p, body) = (h.clone(), i.clone(), p.clone(), body.clone());
                tokio::spawn(async move {
                    // Drain the request head; we don't care what it asks for.
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    h.fetch_add(1, Ordering::SeqCst);
                    let now = i.fetch_add(1, Ordering::SeqCst) + 1;
                    p.fetch_max(now, Ordering::SeqCst);
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
        for _ in 0..COVER_FETCH_CONCURRENCY {
            let c = c.clone();
            handles.push(tokio::spawn(async move {
                c.fetch_cover_now("/api/v1/manga/1/thumbnail").await.is_ok()
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
