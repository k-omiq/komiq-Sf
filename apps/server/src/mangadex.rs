//! Direct MangaDex API client + catalogue sync (CATALOGUE.md §5).
//!
//! Talks to `api.mangadex.org` directly (NOT via Suwayomi) — only the direct API
//! exposes `createdAt` windowing, the external-ID `links` field, and full
//! `altTitles`. Writes the canonical spine through `crate::catalog`. A global token
//! bucket keeps the crawl under MangaDex's ~5 req/s per-IP ceiling (the egress IP is
//! shared fleet-wide, so this is a fleet budget). The whole subsystem is gated off
//! by default (`CATALOGUE_SYNC`); nothing here runs unless explicitly enabled.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::catalog::{self, Alias, Cover, WorkInput};

const API_BASE: &str = "https://api.mangadex.org";
const COVERS_BASE: &str = "https://uploads.mangadex.org/covers";
const PAGE_LIMIT: i64 = 100; // MangaDex list max for /manga
/// MangaDex's documented `ids[]` ceiling on `/chapter` (and the endpoint's `limit` max).
/// Asking for more is rejected by the API, not truncated by it.
const CHAPTER_IDS_LIMIT: usize = 100;
/// Stop paging a window before the hard `offset + limit <= 10_000` cap and slide
/// the `since` window instead.
const WINDOW_OFFSET_CAP: i64 = 9_900;
/// How many times to re-fetch the same offset when the API returns an empty page
/// *before* `offset` has reached the window `total`. A genuine end returns empty at
/// `offset >= total`; an empty page short of that is a transient blip (rate-limit,
/// 5xx that slipped the retry layer, momentary index lag), so we retry rather than
/// truncate the seed. Retries are naturally paced by the 4/s token bucket.
const EMPTY_PAGE_RETRIES: u32 = 3;
/// How many times to retry a single work upsert that failed with a transient SQLite
/// "database is locked" (BUSY). During a full seed the cover drainer + scan scheduler
/// also write the main DB, and even with `busy_timeout` a burst can return BUSY; a
/// dropped work stays missing until the next full seed, so it's worth a few retries.
const UPSERT_LOCK_RETRIES: u32 = 4;
/// Cap on a cover-thumbnail body read. The 512px thumbnails are tens of KB, so this is
/// purely a safety bound against an unbounded/hostile body (`Response::bytes` is
/// otherwise limited only by the 30s client timeout). Matches the 8 MB pre-decode cap
/// the upload paths use (`media::MAX_UPLOAD_BYTES`, `avatar::MAX_UPLOAD_BYTES`).
const MAX_COVER_FETCH_BYTES: usize = 8 * 1024 * 1024;

/// Transient SQLite lock/BUSY predicate. Single definition in `db` so the scanner's
/// retry loop and this one agree on what "transient" means.
use crate::db::is_locked_error;

/// Which timestamp a sweep windows on. The full seed walks the whole catalogue by
/// `createdAt`; recurring incremental refreshes walk only recently-changed records by
/// `updatedAt` (CATALOGUE.md §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncWindow {
    Created,
    Updated,
}

impl SyncWindow {
    /// The `...Since` query parameter name.
    fn since_param(self) -> &'static str {
        match self {
            SyncWindow::Created => "createdAtSince",
            SyncWindow::Updated => "updatedAtSince",
        }
    }
    /// The `order[...]` query parameter key (always ascending, so the window slides forward).
    fn order_key(self) -> &'static str {
        match self {
            SyncWindow::Created => "order[createdAt]",
            SyncWindow::Updated => "order[updatedAt]",
        }
    }
}

/// A simple async token bucket. `capacity` tokens, refilled at `refill_per_sec`.
struct TokenBucket {
    inner: Mutex<BucketState>,
    capacity: f64,
    refill_per_sec: f64,
}

struct BucketState {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate_per_sec: f64) -> Self {
        let rate = rate_per_sec.max(0.1);
        // Capacity must be at least one token: `acquire` needs a whole token, and
        // refill is capped at `capacity`, so a sub-1/s rate (e.g. the 40/min =
        // 0.67/s at-home bucket) with capacity < 1 could never accumulate a token
        // and would block forever. Flooring at 1 is a no-op for rates >= 1/s.
        let capacity = rate.max(1.0);
        Self {
            inner: Mutex::new(BucketState {
                tokens: capacity,
                last: Instant::now(),
            }),
            capacity,
            refill_per_sec: rate,
        }
    }

    /// Block until one token is available, then consume it.
    async fn acquire(&self) {
        loop {
            let wait = {
                let mut st = self.inner.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(st.last).as_secs_f64();
                st.tokens = (st.tokens + elapsed * self.refill_per_sec).min(self.capacity);
                st.last = now;
                if st.tokens >= 1.0 {
                    st.tokens -= 1.0;
                    return;
                }
                (1.0 - st.tokens) / self.refill_per_sec
            };
            tokio::time::sleep(std::time::Duration::from_secs_f64(wait.max(0.001))).await;
        }
    }
}

/// Parse a `Retry-After` header (integer seconds form, which MangaDex uses),
/// clamped to a sane ceiling so a hostile/broken value can't stall the crawl.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs.min(60)))
}

/// Exponential backoff for retry attempt `n` (0-based): 0.5s, 1s, 2s, 4s, …
fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64 << attempt.min(6))
}

/// Read a response body into memory, streamed, refusing anything past `cap` bytes.
/// `Response::bytes()` is unbounded (a 30s timeout is the only ceiling), so a huge or
/// hostile body could otherwise balloon the process; chunked reading lets us bail on
/// the first chunk that crosses the cap. `None` on transport failure or overflow.
async fn read_capped(mut res: reqwest::Response, cap: usize, label: &str) -> Option<Vec<u8>> {
    // Cheap pre-check when the server declares a length; the streaming guard below
    // still applies for chunked responses that declare nothing.
    if res.content_length().is_some_and(|len| len > cap as u64) {
        tracing::warn!(
            label,
            cap,
            "mangadex: response body exceeds cap (declared length)"
        );
        return None;
    }
    let mut out: Vec<u8> = Vec::new();
    loop {
        match res.chunk().await {
            Ok(Some(chunk)) => {
                if out.len() + chunk.len() > cap {
                    tracing::warn!(label, cap, "mangadex: response body exceeds cap");
                    return None;
                }
                out.extend_from_slice(&chunk);
            }
            Ok(None) => return Some(out),
            Err(e) => {
                tracing::debug!(label, error = %e, "mangadex: response body read failed");
                return None;
            }
        }
    }
}

/// The direct MangaDex API client. Cheap to clone-share via `Arc`.
pub struct MangaDexClient {
    http: reqwest::Client,
    /// Global budget shared by every MangaDex call (~5 req/s per-IP ceiling).
    limiter: TokenBucket,
    /// Dedicated budget for `/at-home`, whose own limit (~40/min) is far tighter
    /// than the global one. Acquired *in addition to* `limiter` in `at_home`.
    athome_limiter: TokenBucket,
}

impl MangaDexClient {
    pub fn new(user_agent: &str, rate_per_sec: f64, athome_per_min: f64) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(user_agent.to_string())
            // Bound every request so a hung/slow MangaDex connection can't stall a
            // sweep or a reader page-load indefinitely (M5). A timeout surfaces as
            // a request error (it aborts the current page rather than being
            // retried like a 429/5xx status); an incremental cycle self-heals next
            // pass since the cursor only advances on success.
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builds");
        Self {
            http,
            limiter: TokenBucket::new(rate_per_sec),
            athome_limiter: TokenBucket::new(athome_per_min / 60.0),
        }
    }

    /// Send a rate-limited GET with bounded retries on 429 / 5xx, honoring an
    /// upstream `Retry-After` header when present, else exponential backoff. This
    /// keeps a single transient rate-limit/blip from aborting a whole sweep or a
    /// page-load. The rate-limit token(s) are acquired fresh on every attempt, so
    /// a retry still counts against the budget. `athome` also acquires the tighter
    /// at-home bucket. `label` is only for error/log messages.
    async fn get_with_retry(
        &self,
        url: &str,
        params: &[(String, String)],
        athome: bool,
        label: &str,
    ) -> Result<reqwest::Response> {
        const MAX_RETRIES: u32 = 4;
        const MAX_TRANSPORT_RETRIES: u32 = 2;
        let mut attempt: u32 = 0;
        let mut transport_attempt: u32 = 0;
        loop {
            if athome {
                self.athome_limiter.acquire().await;
            }
            self.limiter.acquire().await;
            let res = match self.http.get(url).query(params).send().await {
                Ok(res) => res,
                Err(e) => {
                    // Transport-level failures (connection reset, DNS blip, timeout)
                    // never surface as an HTTP status but are just as transient as a
                    // 5xx. Retry a bounded number of times with the existing backoff
                    // before giving up, so a single connection reset doesn't fail a
                    // reader-facing at-home page load. Non-transient errors (e.g. a
                    // bad URL / decode) abort immediately, as before.
                    let transient = e.is_connect() || e.is_timeout();
                    if !transient || transport_attempt >= MAX_TRANSPORT_RETRIES {
                        return Err(anyhow!("MangaDex {label} request failed: {e}"));
                    }
                    let wait = backoff(transport_attempt);
                    transport_attempt += 1;
                    tracing::warn!(
                        error = %e,
                        attempt = transport_attempt,
                        wait_ms = wait.as_millis() as u64,
                        "mangadex {label}: retrying after transport error",
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
            };
            let status = res.status();
            if status.is_success() {
                return Ok(res);
            }
            // 429 (rate limited) and 5xx (transient upstream) are worth retrying.
            // MangaDex ALSO returns sporadic 400s under load that succeed on a plain
            // retry with identical params (confirmed: the same `/manga` offset 400s
            // then 200s seconds later) — a single one used to abort a whole 113k seed.
            // Treat 400 as transient too; MAX_RETRIES bounds it, so a genuinely
            // malformed request still fails fast. Other 4xx (401/403/404) are real.
            let retryable =
                status.as_u16() == 429 || status.as_u16() == 400 || status.is_server_error();
            if !retryable || attempt >= MAX_RETRIES {
                return Err(anyhow!("MangaDex {label} error {status}"));
            }
            let wait = retry_after(res.headers()).unwrap_or_else(|| backoff(attempt));
            attempt += 1;
            tracing::warn!(
                status = %status,
                attempt,
                wait_ms = wait.as_millis() as u64,
                "mangadex {label}: retrying after error",
            );
            tokio::time::sleep(wait).await;
        }
    }

    /// One page of `/manga`, ordered by `createdAt` asc, cover expanded. Bad individual
    /// records are skipped rather than failing the page, but they are COUNTED and
    /// returned (`MangaPage::dropped`) — see `MangaPage`.
    pub async fn list_manga(
        &self,
        window: SyncWindow,
        since: Option<&str>,
        offset: i64,
    ) -> Result<MangaPage> {
        let mut params: Vec<(String, String)> = vec![
            ("limit".into(), PAGE_LIMIT.to_string()),
            ("offset".into(), offset.to_string()),
            ("includes[]".into(), "cover_art".into()),
            ("includes[]".into(), "author".into()),
            ("includes[]".into(), "artist".into()),
            (window.order_key().into(), "asc".into()),
            // Skip nothing on content rating — we store the flag and gate at query time.
            ("contentRating[]".into(), "safe".into()),
            ("contentRating[]".into(), "suggestive".into()),
            ("contentRating[]".into(), "erotica".into()),
            ("contentRating[]".into(), "pornographic".into()),
        ];
        if let Some(since) = since {
            params.push((window.since_param().into(), since.to_string()));
        }
        let res = self
            .get_with_retry(&format!("{API_BASE}/manga"), &params, false, "/manga")
            .await?;
        let body: RawList = res.json().await?;
        let raw_len = body.data.len();
        let (mangas, drops) = parse_manga_page(body.data);
        log_record_drops(&drops, "manga", "/manga");
        Ok(MangaPage {
            mangas,
            total: body.total,
            raw_len,
            dropped: drops.len() as u64,
        })
    }

    /// Fetch specific manga by MangaDex ids (max 100 per call — the API's ids[]
    /// ceiling), author/artist/cover expanded. Powers the metadata backfill (S2).
    pub async fn get_manga_by_ids(&self, ids: &[String]) -> Result<Vec<MdManga>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // The `ids[]` filter is capped at 100 per call; chunk and concatenate so a
        // caller passing >100 (backfill/S2) fetches ALL of them instead of silently
        // dropping everything past the 100th.
        let mut mangas = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(100) {
            let mut params: Vec<(String, String)> = vec![
                ("limit".into(), chunk.len().to_string()),
                ("includes[]".into(), "cover_art".into()),
                ("includes[]".into(), "author".into()),
                ("includes[]".into(), "artist".into()),
                ("contentRating[]".into(), "safe".into()),
                ("contentRating[]".into(), "suggestive".into()),
                ("contentRating[]".into(), "erotica".into()),
                ("contentRating[]".into(), "pornographic".into()),
            ];
            for id in chunk {
                params.push(("ids[]".into(), id.clone()));
            }
            let res = self
                .get_with_retry(&format!("{API_BASE}/manga"), &params, false, "/manga?ids")
                .await?;
            let body: RawList = res.json().await?;
            // Same parse path (and the same loud, attributable drop log) as the sweep:
            // this enrichment path silently swallowed the null-`links` cohort too, so an
            // enrich of one of those 4,493 ids returned "nothing" with no explanation.
            //
            // The signature stays `Vec<MdManga>` rather than gaining a drop count: the
            // only caller lives in `graphql/mod.rs` (`enrich_works`), which this change
            // is not allowed to touch.
            //
            // WHAT A DROP HERE ACTUALLY COSTS (checked, not assumed): `enrich_works`
            // calls `mark_metadata_synced` / `mark_covers_synced` over EVERY id in the
            // chunk — deliberately, "including ones MangaDex didn't return", so the drain
            // terminates. So a dropped record is NOT re-offered by the enrichment drain;
            // it keeps the catalogue row the sweep already wrote and simply never gets its
            // S2 metadata / F2 cover set until something re-marks it. That is a much
            // smaller loss than the seed's (the work itself is present either way), but it
            // is not self-healing, which is why these drops are logged at ERROR too.
            let (parsed, drops) = parse_manga_page(body.data);
            log_record_drops(&drops, "manga", "/manga?ids");
            mangas.extend(parsed);
        }
        Ok(mangas)
    }

    /// The full cover set for a manga via `/cover?manga[]=` (F2), volume-ordered.
    /// Returns `(file_name, locale, volume)` per cover, up to `limit` (100 max —
    /// plenty for a cover gallery; very long series are truncated). Rate-limited
    /// like the other calls.
    pub async fn list_covers(
        &self,
        manga_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, Option<String>, Option<String>)>> {
        let params: Vec<(String, String)> = vec![
            ("manga[]".into(), manga_id.to_string()),
            ("limit".into(), limit.clamp(1, 100).to_string()),
            ("order[volume]".into(), "asc".into()),
        ];
        let res = self
            .get_with_retry(&format!("{API_BASE}/cover"), &params, false, "/cover")
            .await?;
        let body: RawList = res.json().await?;
        let mut out = Vec::with_capacity(body.data.len());
        for raw in body.data {
            let cover_id = raw
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("<no-id>")
                .to_string();
            match serde_json::from_value::<MdCover>(raw) {
                Ok(c) => {
                    if let Some(fname) = c.attributes.file_name {
                        if !fname.is_empty() {
                            out.push((fname, c.attributes.locale, c.attributes.volume));
                        }
                    }
                }
                // Never `Err(_) => ()`: that exact shape is what hid the 4,493-record
                // parse failure for months. WARN rather than ERROR because the blast
                // radius really is one gallery entry — the work and its primary cover
                // come from the sweep, not from here — but it is no longer silent.
                Err(e) => tracing::warn!(
                    manga = %manga_id,
                    cover = %cover_id,
                    error = %e,
                    "mangadex: DROPPED an unparseable cover record"
                ),
            }
        }
        Ok(out)
    }

    /// One page of the global `/chapter` firehose, ordered by `createdAt` asc. Like
    /// `list_manga`, bad individual records are skipped but COUNTED and returned
    /// (`ChapterPage::raw_len` / `dropped`) — see `ChapterPage`.
    pub async fn list_chapters(
        &self,
        window: SyncWindow,
        since: Option<&str>,
        offset: i64,
    ) -> Result<ChapterPage> {
        let params = chapter_list_params(window, since, offset);
        let res = self
            .get_with_retry(&format!("{API_BASE}/chapter"), &params, false, "/chapter")
            .await?;
        let body: RawChapterList = res.json().await?;
        let raw_len = body.data.len();
        let (chapters, drops) = parse_chapter_page(body.data);
        log_record_drops(&drops, "chapter", "/chapter");
        Ok(ChapterPage {
            chapters,
            total: body.total,
            raw_len,
            dropped: drops.len() as u64,
        })
    }

    /// Fetch specific chapters by MangaDex id (`GET /chapter?ids[]=…`). Powers the A1b
    /// `readable_at` / `external_url` backfill, which re-asks upstream about chapters the
    /// mirror already holds rather than re-walking the firehose.
    ///
    /// Same conventions as `list_chapters`, and for the same reasons: `get_with_retry` so
    /// the shared in-process token bucket and the 429/400/5xx ladder apply, and
    /// `parse_chapter_page` so an unparseable record is COUNTED and attributed rather than
    /// silently swallowed. `raw_len` is reported separately from `chapters.len()` because
    /// the caller compares what it ASKED for against what came back — a parse-drop there
    /// must read as "upstream did not answer for this id", not as "this id does not exist".
    ///
    /// ONE REQUEST, NO CHUNKING — deliberately unlike `get_manga_by_ids`, which loops over
    /// `chunks(100)`. The only caller is a paced drain that sleeps between batches; chunking
    /// in here would fire N requests back-to-back inside one "batch" and defeat that pacing.
    /// More than 100 ids is therefore a caller bug and fails loudly: silently truncating to
    /// the API's `ids[]` ceiling would leave the tail unanswered, and an unanswered row that
    /// is never written stays in the work-list forever — the one failure mode a cursorless
    /// drain cannot survive.
    pub async fn get_chapters_by_ids(&self, ids: &[String]) -> Result<ChapterPage> {
        if ids.is_empty() {
            return Ok(ChapterPage {
                chapters: Vec::new(),
                total: 0,
                raw_len: 0,
                dropped: 0,
            });
        }
        if ids.len() > CHAPTER_IDS_LIMIT {
            return Err(anyhow!(
                "MangaDex /chapter?ids accepts at most {CHAPTER_IDS_LIMIT} ids, got {}",
                ids.len()
            ));
        }
        let params = chapter_ids_params(ids);
        let res = self
            .get_with_retry(
                &format!("{API_BASE}/chapter"),
                &params,
                false,
                "/chapter?ids",
            )
            .await?;
        let body: RawChapterList = res.json().await?;
        let raw_len = body.data.len();
        let (chapters, drops) = parse_chapter_page(body.data);
        log_record_drops(&drops, "chapter", "/chapter?ids");
        Ok(ChapterPage {
            chapters,
            total: body.total,
            raw_len,
            dropped: drops.len() as u64,
        })
    }

    /// Download a work's cover thumbnail and compute its perceptual hash. Uses the
    /// 512px thumbnail (smaller download, always JPEG). Best-effort: returns `None`
    /// on any network/decode failure — a missing hash just means one fewer dedup
    /// signal, never a failed sync. Rate-limited like the API calls.
    pub async fn cover_phash(&self, manga_id: &str, file_name: &str) -> Option<String> {
        let bytes = self.fetch_cover_thumb(manga_id, file_name).await?;
        crate::phash::dhash(&bytes)
    }

    /// Shared 512px cover-thumbnail fetch behind `cover_phash` / `cover_thumb_bytes`.
    /// Goes through `get_with_retry` like the API calls, so a 429 from the cover CDN
    /// backs off (honoring `Retry-After`) instead of being silently read as "this work
    /// has no cover" — the old direct `http.get(...).send()` turned every rate-limit
    /// into a permanent miss. The body is read with a hard length cap. `None` on any
    /// failure: a missing cover is one fewer dedup signal, never a failed sync.
    async fn fetch_cover_thumb(&self, manga_id: &str, file_name: &str) -> Option<Vec<u8>> {
        let url = format!("{}.512.jpg", cover_url(manga_id, file_name));
        let res = match self.get_with_retry(&url, &[], false, "cover").await {
            Ok(res) => res,
            Err(e) => {
                tracing::debug!(manga = %manga_id, error = %e, "mangadex: cover fetch failed");
                return None;
            }
        };
        read_capped(res, MAX_COVER_FETCH_BYTES, manga_id).await
    }

    /// Download a work's 512px cover thumbnail as raw bytes, for the DB-backed
    /// cover cache (`cover::crawl_uncached_covers`). Same source URL + rate limit
    /// as `cover_phash`, but returns the bytes instead of hashing them.
    /// Best-effort: `None` on any network / non-success failure so a single bad
    /// cover never aborts the crawl.
    pub async fn cover_thumb_bytes(&self, manga_id: &str, file_name: &str) -> Option<Vec<u8>> {
        self.fetch_cover_thumb(manga_id, file_name).await
    }

    /// Resolve a chapter's ordered page image URLs via MangaDex@Home
    /// (`GET /at-home/server/{chapterId}` → `{ baseUrl, chapter: { hash, data[] } }`).
    /// Each page URL is `{baseUrl}/data/{hash}/{filename}`. These are dynamic
    /// `*.mangadex.network` hosts and MUST be proxied by the Worker (hotlinks get a
    /// wrong response). This endpoint is capped at ~40/min — far below the global
    /// ~5 req/s (300/min) budget — so it takes a dedicated `athome_limiter` in
    /// addition to the global one. CATALOGUE.md §5, §9.
    pub async fn at_home(&self, chapter_id: &str) -> Result<Vec<String>> {
        let res = self
            .get_with_retry(
                &format!("{API_BASE}/at-home/server/{chapter_id}"),
                &[],
                true,
                "/at-home",
            )
            .await?;
        let body: AtHome = res.json().await?;
        let base = body.base_url.trim_end_matches('/');
        let hash = &body.chapter.hash;
        Ok(body
            .chapter
            .data
            .into_iter()
            .map(|filename| format!("{base}/data/{hash}/{filename}"))
            .collect())
    }
}

/// The query params for one page of the `/chapter` firehose. Factored out of
/// `list_chapters` so the content-rating set is unit-testable without a live request —
/// the omission fixed below survived the mirror's entire life partly because nothing
/// could assert on it.
///
/// ALL FOUR CONTENT RATINGS, mirroring `list_manga`. MangaDex defaults every list
/// endpoint to `safe,suggestive,erotica`, so for as long as this call omitted
/// `contentRating[]` the whole `pornographic` slice of the firehose was invisible to us.
/// Measured upstream 2026-07-26: `/chapter?translatedLanguage[]=en` reports a `total` of
/// 810,292 with the defaults against 876,433 with all four — a 66,141-chapter hole. The
/// mirror shows the same shape from the inside: of the MangaDex works we carry, exactly
/// **4** `pornographic` ones have any chapter at all versus 19,923 with none, while
/// `erotica` (which the defaults DO include) sits at 4,742 with chapters vs 4,197
/// without.
///
/// THIS FIXES THE FORWARD PATH ONLY AND RECOVERS NO BACKLOG BY ITSELF. Once
/// `chapters.seed_done` is latched the sweep runs the `updatedAtSince` window, which
/// never revisits an old `createdAt`, so the ~66k historical chapters stay unreachable
/// until the seed is walked again — that is what migration 0067 forces, exactly once.
fn chapter_list_params(
    window: SyncWindow,
    since: Option<&str>,
    offset: i64,
) -> Vec<(String, String)> {
    let mut params: Vec<(String, String)> = vec![
        ("limit".into(), PAGE_LIMIT.to_string()),
        ("offset".into(), offset.to_string()),
        (window.order_key().into(), "asc".into()),
        ("includes[]".into(), "manga".into()),
        // English-only: Komika serves only English chapters, so filter the firehose
        // at the source (smaller pages, no non-English rows to mirror).
        ("translatedLanguage[]".into(), "en".into()),
        // Skip nothing on content rating — the work carries the flag and every surface
        // gates at query time, same contract as the catalogue sweep.
        ("contentRating[]".into(), "safe".into()),
        ("contentRating[]".into(), "suggestive".into()),
        ("contentRating[]".into(), "erotica".into()),
        ("contentRating[]".into(), "pornographic".into()),
    ];
    if let Some(since) = since {
        params.push((window.since_param().into(), since.to_string()));
    }
    params
}

/// The query params for one `/chapter?ids[]=` lookup. Factored out of
/// `get_chapters_by_ids` for exactly the reason `chapter_list_params` was: the
/// content-rating omission below is invisible from the outside and cost the firehose the
/// whole `pornographic` slice for the mirror's entire life.
///
/// ALL FOUR CONTENT RATINGS, and this is not cargo-culted from the firehose — `/chapter`
/// applies its `safe,suggestive,erotica` default to an `ids[]` query too, so without them
/// every chapter of a `pornographic` work would come back MISSING rather than filtered.
/// For this caller "missing" is not a no-op: it writes the fallback clock and takes the row
/// off the work-list permanently, so the omission would burn the one chance those rows get
/// at a real `readableAt`/`externalUrl`.
///
/// NO `translatedLanguage[]`, unlike the firehose. The ids are already exact — every one of
/// them names a row we mirror — so a language filter can only subtract, and subtracting
/// here means the same permanent fallback write. Filter nothing that the id has not already
/// decided.
fn chapter_ids_params(ids: &[String]) -> Vec<(String, String)> {
    let mut params: Vec<(String, String)> = vec![
        ("limit".into(), ids.len().to_string()),
        ("contentRating[]".into(), "safe".into()),
        ("contentRating[]".into(), "suggestive".into()),
        ("contentRating[]".into(), "erotica".into()),
        ("contentRating[]".into(), "pornographic".into()),
    ];
    for id in ids {
        params.push(("ids[]".into(), id.clone()));
    }
    params
}

/// MangaDex@Home server response for a chapter (the fields we build page URLs from).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AtHome {
    base_url: String,
    chapter: AtHomeChapter,
}

#[derive(Deserialize)]
struct AtHomeChapter {
    hash: String,
    /// Strict in both directions (like `RawList::total`, and unlike every `data` on a
    /// COLLECTION envelope). This one is the page list of a single chapter: decoding a
    /// missing/null `data` as `[]` would hand the reader a chapter with zero pages —
    /// indistinguishable from a successful fetch, and a blank screen with nothing in
    /// the logs. Failing the decode surfaces a retryable error to the resolver instead.
    /// `data` is always present live (verified 2026-07-26), so this only ever fires on
    /// a genuinely broken response.
    data: Vec<String>,
}

/// Proxy-ready cover URL for a MangaDex work. Callers must route this through the
/// Worker proxy — MangaDex serves a wrong response to hotlinks. Used by cover
/// pHash ingest and canonical-model serving (CATALOGUE.md §5–6).
pub fn cover_url(manga_id: &str, file_name: &str) -> String {
    format!("{COVERS_BASE}/{manga_id}/{file_name}")
}

/// A smaller (512px) cover thumbnail URL — same proxy rules as `cover_url`. Used for
/// reader browse/list surfaces where the full-resolution cover is wasteful.
pub fn cover_thumb_url(manga_id: &str, file_name: &str) -> String {
    format!("{COVERS_BASE}/{manga_id}/{file_name}.512.jpg")
}

// ---- Response shapes -------------------------------------------------------

/// Deserialize a field that may be an explicit JSON `null` as `T::default()`.
///
/// THE 4,493-RECORD BUG (2026-07-26). `#[serde(default)]` covers only an ABSENT
/// field: when the key is present with value `null`, serde still invokes `T`'s
/// `Deserialize`, and `HashMap`/`Vec` reject a `Null` token with
/// `invalid type: null, expected a map`. Because the fields below live inside
/// `MdManga`, ONE null container failed the WHOLE record — and `list_manga` used to
/// throw the error away (`Err(_) => skipped += 1`).
///
/// MangaDex emits `"links": null` on a closed legacy cohort (2018-02 → 2021, zero
/// records from 2022 on); exemplar `c6a8967b-2b61-4e14-8aca-1525a37b63f7` ("Yururira",
/// createdAt 2018-02-12). That cohort is exactly 4,493 records, 100% of the catalogue
/// gap (109,266 stored = 113,759 upstream − 4,493), and every one of them was fetched,
/// parsed-failed, and dropped on every sweep since the seed — while the sweep reported
/// a clean completion, because a dropped record shrinks neither `raw_len` nor `total`.
///
/// Applied to every non-`Option` container in the response shapes whose absence is
/// SURVIVABLE, not just `links`, so a second cohort with a different null field can never
/// reproduce this. The four that are deliberately strict instead — `RawList::total`,
/// `RawChapterList::total`, `MdManga::attributes`, `MdCover::attributes` (plus
/// `AtHomeChapter::data`, which is a single chapter's page list rather than a collection
/// envelope) — each say so at their definition, and "strict" there means strict in BOTH
/// directions: no `#[serde(default)]` either, because an ABSENT field decoding to
/// `0`/empty is the same silent lie as a null one.
///
/// Measured against the live API on 2026-07-26 (3,000 records sampled over `/manga`
/// windows from 2018-01 to 2026-07, plus `/chapter` and `/cover`): `attributes.links` is
/// null on ~4.8% of records; `title`, `altTitles`, `description`, `attributes`,
/// `relationships`, `id` and `type` were never null and never absent; every `altTitles`
/// element and every `relationships` element was an object. The remaining theoretical
/// hole is a null ELEMENT inside one of those arrays (`"altTitles": [null]`), which would
/// still fail the whole record — unobserved, and not worth a custom seq visitor, but
/// worth knowing about if a future cohort ever drops with a `expected a map` error
/// pointing at an array index.
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct RawList {
    /// Hardened: a null `data` would otherwise fail the whole BODY, costing 100
    /// records instead of one. An empty page is handled safely downstream
    /// (`classify_page` → `RetryEmpty`, then `truncated`, which blocks completion).
    #[serde(default, deserialize_with = "null_as_default")]
    data: Vec<Value>,
    /// DELIBERATELY STRICT, IN BOTH DIRECTIONS — no `#[serde(default)]`, no
    /// `null_as_default`. A `total` of `0` makes `classify_page` / `offset >= total`
    /// read "window exhausted", which ends the window, reports a clean+complete sweep
    /// and can latch a one-shot marker (or `seed_done`) over an unwalked catalogue.
    ///
    /// `#[serde(default)]` used to sit here, which left that guard HALF-APPLIED: it
    /// covered a null `total` (which fails, correctly) but silently turned an ABSENT
    /// `total` into exactly the `0` the comment was warning about. Every MangaDex
    /// collection envelope carries `total` — verified live on 2026-07-26 across 30
    /// `/manga` pages spanning 2018→2026 plus `/chapter` and `/cover`, all with the
    /// identical key set `{result, response, data, limit, offset, total}` — so
    /// requiring it costs nothing real, and a body without it fails the decode. That
    /// is the safe direction: a decode failure leaves the cursor untouched and is not
    /// a tolerable page error, so it aborts the sweep instead of faking its end.
    total: i64,
}

#[derive(Deserialize)]
struct RawChapterList {
    #[serde(default, deserialize_with = "null_as_default")]
    data: Vec<Value>,
    /// Strict in both directions, for the same reason as `RawList::total`.
    total: i64,
}

#[derive(Debug, Deserialize)]
pub struct MdManga {
    pub id: String,
    /// Left REQUIRED on purpose: a manga with no attributes has no title, and
    /// upserting a titleless work into the canonical spine is worse than dropping it.
    /// Such a record now fails loudly (counted in `dropped`, logged with its uuid).
    pub attributes: MdAttrs,
    #[serde(default, deserialize_with = "null_as_default")]
    pub relationships: Vec<MdRel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MdAttrs {
    #[serde(default, deserialize_with = "null_as_default")]
    pub title: HashMap<String, String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub alt_titles: Vec<HashMap<String, String>>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub description: HashMap<String, String>,
    pub original_language: Option<String>,
    pub status: Option<String>,
    pub year: Option<i64>,
    pub publication_demographic: Option<String>,
    pub content_rating: Option<String>,
    /// The field that dropped 4,493 works — see `null_as_default`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub links: HashMap<String, String>,
    /// Genre/theme/format/content tags. THE source of genres for canonical works: until
    /// this field existed, `work_tag` was empty and the ~101k MangaDex-only works had no
    /// genre at all (see migration 0066). Rides along in the `/manga` payload the sweep
    /// already fetches, so reading it costs no extra request.
    #[serde(default, deserialize_with = "null_as_default")]
    pub tags: Vec<MdTag>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// One entry of `attributes.tags[]`.
///
/// `attributes` is OPTIONAL on purpose. A tag object the API returns stripped down (or
/// null) must contribute nothing rather than fail the whole manga record — the exact
/// failure mode `null_as_default` exists to prevent, and a genre is far less valuable
/// than the work it hangs off.
#[derive(Debug, Deserialize)]
pub struct MdTag {
    #[serde(default)]
    pub attributes: Option<MdTagAttrs>,
}

#[derive(Debug, Deserialize)]
pub struct MdTagAttrs {
    /// Localized tag names, e.g. `{"en": "Isekai", "ja": "異世界"}`.
    #[serde(default, deserialize_with = "null_as_default")]
    pub name: HashMap<String, String>,
    /// `genre` | `theme` | `format` | `content` — drives the stored ordering, see
    /// `TAG_GROUP_ORDER`.
    pub group: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MdRel {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub attributes: Option<MdRelAttrs>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MdRelAttrs {
    pub file_name: Option<String>,
    pub name: Option<String>,
    /// Cover locale (F2) — `cover_art` relationships carry `locale` + `volume`.
    pub locale: Option<String>,
    pub volume: Option<String>,
}

/// One page of `/manga`, as the sweeps consume it.
///
/// A struct rather than a 4-tuple: `raw_len` and `dropped` are both "counts of records"
/// with completely different meanings, and the pagination invariant below is far easier
/// to state (and to not get wrong at a call site) against named fields.
pub struct MangaPage {
    /// Records that parsed. May be SHORTER than `raw_len` — see `dropped`.
    pub mangas: Vec<MdManga>,
    /// `total` for the whole query, as reported by the API.
    pub total: i64,
    /// Records the API returned, BEFORE parse-drops. This — never `mangas.len()` — is
    /// what drives `classify_page` and `offset` advancement, so a page shortened only by
    /// unparseable records is never mistaken for the end of the window and the cursor
    /// stays aligned with the API's own pagination.
    pub raw_len: usize,
    /// Records that were fetched and then failed to parse: `raw_len - mangas.len()`.
    /// Fetched-but-unpersisted is the same class of loss as a failed upsert, so callers
    /// must treat a non-zero value as blocking completion — 4,493 works hid here for
    /// months precisely because nobody returned this number.
    pub dropped: u64,
}

/// The per-record parse step of `list_manga`, pure and HTTP-free so it is testable.
/// Returns the parsed records plus `(uuid, serde error)` for every record dropped —
/// the uuid is read straight off the raw `Value` before the parse attempt, so a drop is
/// always attributable. The old code did `Err(_) => skipped += 1`, discarding the one
/// string (`invalid type: null, expected a map`) that would have named `links` on day one.
fn parse_manga_page(raw: Vec<Value>) -> (Vec<MdManga>, Vec<(String, String)>) {
    let mut mangas = Vec::with_capacity(raw.len());
    let mut drops = Vec::new();
    for v in raw {
        let id = v
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("<no-id>")
            .to_string();
        match serde_json::from_value::<MdManga>(v) {
            Ok(m) => mangas.push(m),
            Err(e) => drops.push((id, e.to_string())),
        }
    }
    (mangas, drops)
}

/// How many parse-drops this process logs INDIVIDUALLY (uuid + serde message) before
/// falling back to one aggregate line per page.
///
/// Unbounded per-record logging was the other half of this fix's risk: a recurrence of
/// the `links: null` shape drops ~4,493 records per full walk, and a dirty backfill pass
/// re-walks the catalogue up to `BACKFILL_MAX_PASSES` times — >100k ERROR lines per boot,
/// which is how a log pipeline gets dropped on the floor exactly when it matters. 200
/// individual lines is far more than a human needs to spot a cohort (they carry the uuid,
/// so `/manga/{uuid}` reproduces it), and the aggregate line that follows still carries a
/// per-page count plus one exemplar uuid, so nothing becomes invisible.
const MAX_DETAILED_DROP_LOGS: u64 = 200;

/// Individually-logged drops so far this process. Only ever feeds the log-verbosity
/// decision — never a completion predicate (`dropped` counts are exact and unsampled).
static DETAILED_DROP_LOGS: AtomicU64 = AtomicU64::new(0);

/// How many of a page's `page_drops` still fit under the per-process detail budget,
/// given `already_logged`. Pure so the sampling rule is testable.
fn detailed_drop_count(already_logged: u64, page_drops: usize) -> usize {
    // `budget <= MAX_DETAILED_DROP_LOGS`, so the cast can never truncate.
    let budget = MAX_DETAILED_DROP_LOGS.saturating_sub(already_logged) as usize;
    page_drops.min(budget)
}

/// Log parse-drops at ERROR with the offending uuid and the serde message. ERROR, not
/// warn: a fetched record that never reaches the spine is data loss, and the previous
/// aggregate `warn!(skipped)` (no id, no reason) is why this survived a full audit.
/// Past `MAX_DETAILED_DROP_LOGS` the per-record detail is replaced by one aggregate
/// ERROR per page — still loud, still attributable, but bounded. `kind` is `manga` or
/// `chapter`; both share the one budget, since a systematic drop hits both endpoints.
fn log_record_drops(drops: &[(String, String)], kind: &str, endpoint: &str) {
    if drops.is_empty() {
        return;
    }
    let already = DETAILED_DROP_LOGS.fetch_add(drops.len() as u64, Ordering::Relaxed);
    let detailed = detailed_drop_count(already, drops.len());
    for (id, err) in &drops[..detailed] {
        tracing::error!(
            id = %id,
            kind,
            error = %err,
            endpoint,
            "mangadex: DROPPED an unparseable record — it will not reach the catalogue"
        );
    }
    if detailed < drops.len() {
        let (id, err) = &drops[detailed];
        tracing::error!(
            kind,
            endpoint,
            suppressed = drops.len() - detailed,
            example = %id,
            error = %err,
            detail_cap = MAX_DETAILED_DROP_LOGS,
            "mangadex: DROPPED more unparseable records on this page — per-record logging \
             is capped for this process; the counts in the sweep/backfill summary are exact"
        );
    }
}

/// A `/cover` record (F2). Only the fields F2 stores.
#[derive(Debug, Deserialize)]
struct MdCover {
    /// Required, like `MdManga::attributes`: a cover record with no attributes has no
    /// `fileName`, so there is nothing to store — dropping it is the right outcome.
    attributes: MdCoverAttrs,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MdCoverAttrs {
    file_name: Option<String>,
    locale: Option<String>,
    volume: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MdChapter {
    pub id: String,
    pub attributes: MdChapterAttrs,
    /// Hardened for symmetry with `MdManga::relationships`: the chapter firehose
    /// resolves its work through this list, so a null here would silently drop the
    /// chapter exactly the way a null `links` dropped a whole work.
    #[serde(default, deserialize_with = "null_as_default")]
    pub relationships: Vec<MdRel>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MdChapterAttrs {
    pub chapter: Option<String>,
    pub volume: Option<String>,
    pub title: Option<String>,
    pub translated_language: Option<String>,
    /// SCHEDULING METADATA, NOT A RELEASE CLOCK. MangaDex stamps external chapters
    /// `2037-12-31T15:00:00` here as a sentinel, and it can post-date [`Self::readable_at`]
    /// by weeks on chapters that are perfectly readable now. Never bound or sort a feed on
    /// it alone — see migration 0073.
    pub publish_at: Option<String>,
    /// When the chapter actually became readable — the clock MangaDex's own
    /// latest-updates surface orders by, and the one every feed query here should use.
    pub readable_at: Option<String>,
    /// Set when the chapter is hosted somewhere else (MangaPlus, Comikey, NamiComi,
    /// BiliBili) and has no pages to serve. **The only valid test for an external
    /// chapter**: `pages` is not — sampled bilibilicomics externals report `pages: 45`.
    pub external_url: Option<String>,
    /// Page count as MangaDex reports it. Carried for diagnostics only, precisely because
    /// it is NOT a usable external-chapter discriminator.
    pub pages: Option<i64>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// One page of `/chapter`, as `sync_chapters` consumes it. The mirror image of
/// `MangaPage`, and for the same reason: the firehose loop used to page on
/// `chapters.len()` — the PARSED count — so ONE unparseable chapter shortened the page
/// to 99, tripped the `page_len < PAGE_LIMIT` end-of-window test, and ended the window
/// early. The cursor then advanced to the cycle's start time anyway, so every chapter
/// after the truncation point in that window was never mirrored and (the window being
/// forward-only) never re-offered. Same bug class as the 4,493-record catalogue gap,
/// same fix: raw counts drive pagination, parsed records drive nothing but the work.
pub struct ChapterPage {
    /// Records that parsed. May be SHORTER than `raw_len` — see `dropped`.
    pub chapters: Vec<MdChapter>,
    /// `total` for the whole query, as reported by the API.
    pub total: i64,
    /// Records the API returned, BEFORE parse-drops. Drives `classify_page`, the
    /// end-of-window test and the `offset` advance — never `chapters.len()`.
    pub raw_len: usize,
    /// Records fetched and then failed to parse: `raw_len - chapters.len()`.
    pub dropped: u64,
}

/// The per-record parse step of `list_chapters`, pure and HTTP-free so it is testable.
/// Mirrors `parse_manga_page`, including reading the uuid off the raw `Value` first so
/// every drop is attributable.
fn parse_chapter_page(raw: Vec<Value>) -> (Vec<MdChapter>, Vec<(String, String)>) {
    let mut chapters = Vec::with_capacity(raw.len());
    let mut drops = Vec::new();
    for v in raw {
        let id = v
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("<no-id>")
            .to_string();
        match serde_json::from_value::<MdChapter>(v) {
            Ok(c) => chapters.push(c),
            Err(e) => drops.push((id, e.to_string())),
        }
    }
    (chapters, drops)
}

// ---- Mapping ---------------------------------------------------------------

const EXTERNAL_LINK_KEYS: &[&str] = &["al", "mal", "mu", "kt", "ap"];

/// Tag groups in the order they are STORED, so `work_tag.ord` puts the tags a reader
/// actually browses by first and the noisier axes last.
///
/// All four groups are kept, deliberately:
///   * `genre`  — "Action", "Romance": the browsable axis the genre filter exists for.
///   * `theme`  — "Isekai", "Martial Arts": likewise, and what MangaDex users search by.
///   * `format` — "Long Strip", "Oneshot": kept because `resolve_comic_type` reads genre
///                STRINGS to tell a manhwa from a manga, so dropping these would make
///                canonical format detection worse than it already is.
///   * `content`— "Gore", "Sexual Violence": kept because MangaDex exposes them as
///                filterable tags and omitting them would silently narrow what the
///                catalogue can express. They are rare, so they sink in a
///                frequency-ordered facet list on their own.
/// An unrecognised or missing group sorts last rather than being dropped.
const TAG_GROUP_ORDER: &[&str] = &["genre", "theme", "format", "content"];

/// English tag names for a manga, ordered by [`TAG_GROUP_ORDER`] and then by the order
/// the API listed them within a group.
///
/// English only. A tag with no `en` name is unusable as a label on an English-language
/// surface, and there is no sensible fallback: the other locales are localized values of
/// the SAME tag, so taking one would drop a Japanese string into an English facet list.
///
/// Deduped by name, keeping the first (highest-precedence group) occurrence.
/// `work_tag`'s primary key is (work_id, tag), so a duplicate would be silently dropped
/// at insert anyway — doing it here keeps `ord` contiguous and the count honest.
fn tag_names(m: &MdManga) -> Vec<String> {
    let mut ranked: Vec<(usize, usize, String)> = Vec::new();
    for (i, t) in m.attributes.tags.iter().enumerate() {
        let Some(a) = t.attributes.as_ref() else {
            continue;
        };
        let Some(name) = a.name.get("en") else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let group = a.group.as_deref().unwrap_or("");
        let rank = TAG_GROUP_ORDER
            .iter()
            .position(|g| *g == group)
            .unwrap_or(TAG_GROUP_ORDER.len());
        ranked.push((rank, i, name.to_string()));
    }
    ranked.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let mut seen = std::collections::HashSet::new();
    ranked
        .into_iter()
        .filter(|(_, _, n)| seen.insert(n.clone()))
        .map(|(_, _, n)| n)
        .collect()
}

/// Map a MangaDex manga to `(mangadex_id, WorkInput)` for the canonical spine.
pub fn to_work_input(m: &MdManga) -> (String, WorkInput) {
    let a = &m.attributes;

    let (primary_lang, primary_title) = pick_localized(&a.title);
    let description = pick_localized(&a.description).1;

    // Aliases: every localized primary title + every altTitle entry.
    let mut aliases: Vec<Alias> = Vec::new();
    for (lang, val) in &a.title {
        aliases.push(Alias {
            raw: val.clone(),
            lang: Some(lang.clone()),
        });
    }
    for alt in &a.alt_titles {
        for (lang, val) in alt {
            aliases.push(Alias {
                raw: val.clone(),
                lang: Some(lang.clone()),
            });
        }
    }

    let mut external_ids: Vec<(String, String)> = Vec::new();
    for key in EXTERNAL_LINK_KEYS {
        if let Some(v) = a.links.get(*key) {
            if !v.is_empty() {
                external_ids.push(((*key).to_string(), v.clone()));
            }
        }
    }

    let content_rating = a.content_rating.clone();
    let is_nsfw = matches!(
        content_rating.as_deref(),
        Some("erotica") | Some("pornographic")
    );

    // S2 enrichment: every localized description, and the FULL credit list (a
    // work can have several authors/artists; the singular fields keep the first).
    let mut descriptions: Vec<(String, String)> = a
        .description
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    descriptions.sort();
    let mut credits: Vec<(String, String)> = Vec::new();
    for role in ["author", "artist"] {
        for name in rel_names(m, role) {
            credits.push((role.to_string(), name));
        }
    }

    (
        m.id.clone(),
        WorkInput {
            primary_title,
            primary_lang,
            description,
            year: a.year,
            original_language: a.original_language.clone(),
            status: map_status(a.status.as_deref()),
            demographic: a.publication_demographic.clone(),
            content_rating,
            is_nsfw,
            author: rel_name(m, "author"),
            artist: rel_name(m, "artist"),
            cover_phash: None, // filled by cover pHash ingest in sync_catalogue when enabled
            // From the already-expanded cover_art relationship — no extra request. Powers
            // the reader cover URL for canonical works (independent of the pHash ingest).
            cover_file_name: cover_file_name(m),
            aliases,
            external_ids,
            descriptions,
            credits,
            // Upstream genres (migration 0066). Written as the `source = 'mangadex'`
            // half of `work_tag`, so admin curation still wins outright.
            tags: tag_names(m),
            // ALL cover_art relationships the manga response carries (F2). In
            // practice the manga endpoint expands only the primary (one entry) —
            // the full per-volume set is fetched separately via `/cover` in the
            // enrichment path. The first cover_art is the primary.
            covers: relationship_covers(m),
        },
    )
}

/// Every `cover_art` relationship on a manga, mapped to `Cover` (F2). The first is
/// marked primary (the manga endpoint expands only the primary in practice).
fn relationship_covers(m: &MdManga) -> Vec<Cover> {
    let mut out = Vec::new();
    for r in m.relationships.iter().filter(|r| r.kind == "cover_art") {
        if let Some(a) = r.attributes.as_ref() {
            if let Some(fname) = a.file_name.clone() {
                if !fname.is_empty() {
                    out.push(Cover {
                        file_name: fname,
                        lang: a.locale.clone(),
                        volume: a.volume.clone(),
                        is_primary: out.is_empty(),
                    });
                }
            }
        }
    }
    out
}

/// Build the full cover set for a work from a `/cover` fetch (F2), marking the one
/// matching `primary_file_name` as primary (falling back to the first). Used by
/// the enrichment/backfill path to complete a work's cover gallery.
pub fn covers_from_fetch(
    fetched: Vec<(String, Option<String>, Option<String>)>,
    primary_file_name: Option<&str>,
) -> Vec<Cover> {
    let mut out: Vec<Cover> = fetched
        .into_iter()
        .map(|(file_name, lang, volume)| {
            let is_primary = primary_file_name == Some(file_name.as_str());
            Cover {
                file_name,
                lang,
                volume,
                is_primary,
            }
        })
        .collect();
    // Ensure exactly one primary: if none matched, promote the first.
    if !out.iter().any(|c| c.is_primary) {
        if let Some(first) = out.first_mut() {
            first.is_primary = true;
        }
    }
    out
}

/// Prefer an English value, then a romanized Japanese one, then any entry.
fn pick_localized(map: &HashMap<String, String>) -> (Option<String>, Option<String>) {
    for key in ["en", "ja-ro", "ja"] {
        if let Some(v) = map.get(key) {
            if !v.is_empty() {
                return (Some(key.to_string()), Some(v.clone()));
            }
        }
    }
    map.iter()
        .find(|(_, v)| !v.is_empty())
        .map(|(k, v)| (Some(k.clone()), Some(v.clone())))
        .unwrap_or((None, None))
}

fn rel_name(m: &MdManga, kind: &str) -> Option<String> {
    m.relationships
        .iter()
        .find(|r| r.kind == kind)
        .and_then(|r| r.attributes.as_ref())
        .and_then(|a| a.name.clone())
        .filter(|s| !s.is_empty())
}

/// EVERY relationship name of a kind, deduped in order (S2 full credit list).
fn rel_names(m: &MdManga, kind: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in m.relationships.iter().filter(|r| r.kind == kind) {
        if let Some(name) = r.attributes.as_ref().and_then(|a| a.name.clone()) {
            if !name.is_empty() && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// MangaDex cover fileName for a manga (first cover_art relationship), if expanded.
/// Feeds cover pHash ingest (CATALOGUE.md §5).
pub fn cover_file_name(m: &MdManga) -> Option<String> {
    m.relationships
        .iter()
        .find(|r| r.kind == "cover_art")
        .and_then(|r| r.attributes.as_ref())
        .and_then(|a| a.file_name.clone())
}

fn map_status(s: Option<&str>) -> Option<String> {
    Some(
        match s {
            Some("ongoing") => "ONGOING",
            Some("completed") => "COMPLETED",
            Some("hiatus") => "HIATUS",
            Some("cancelled") => "CANCELLED",
            _ => return None,
        }
        .to_string(),
    )
}

/// The timestamp a manga sweep slides its window on (per `SyncWindow`).
fn manga_window_ts(m: &MdManga, window: SyncWindow) -> Option<String> {
    match window {
        SyncWindow::Created => m.attributes.created_at.clone(),
        SyncWindow::Updated => m.attributes.updated_at.clone(),
    }
}

/// The timestamp a chapter sweep slides its window on (per `SyncWindow`).
fn chapter_window_ts(c: &MdChapter, window: SyncWindow) -> Option<String> {
    match window {
        SyncWindow::Created => c.attributes.created_at.clone(),
        SyncWindow::Updated => c.attributes.updated_at.clone(),
    }
}

/// The `manga` relationship id on a chapter (which work the chapter belongs to).
pub fn chapter_manga_id(c: &MdChapter) -> Option<String> {
    c.relationships
        .iter()
        .find(|r| r.kind == "manga")
        .map(|r| r.id.clone())
}

/// Coerce an ISO-8601 timestamp (e.g. `2018-10-04T22:16:00+00:00`) into the
/// `createdAtSince` form MangaDex expects (`YYYY-MM-DDTHH:MM:SS`, no offset).
pub fn to_since(ts: &str) -> Option<String> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    Some(dt.format("%Y-%m-%dT%H:%M:%S").to_string())
}

/// The `since` cursor one second AFTER `ts`. Used to step past a boundary second
/// that holds more than `WINDOW_OFFSET_CAP` records (the window can't advance
/// otherwise): stepping loses only the tail of that single tied second instead of
/// stalling the whole sweep on it forever.
pub fn to_since_next_second(ts: &str) -> Option<String> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    let stepped = dt + chrono::Duration::seconds(1);
    Some(stepped.format("%Y-%m-%dT%H:%M:%S").to_string())
}

// ---- Sync loops ------------------------------------------------------------

/// What the catalogue page loop does after a fetch, decided purely from the RAW page
/// length, the current `offset`, and the window `total`. Extracted from `sync_catalogue`
/// so the premature-truncation fix is unit-testable without a live MangaDex: the whole
/// point is that a page shortened *only* by unparseable-record skips (which shrink the
/// PARSED count but not `raw_len`) is still `Process`, never `EndWindow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageStep {
    /// Non-empty page — process the records, then advance `offset` by `raw_len`.
    Process,
    /// Empty page at/after `total` (or an empty window) — genuinely exhausted.
    EndWindow,
    /// Empty page *before* `total` — a transient blip; re-fetch the same offset.
    RetryEmpty,
}

fn classify_page(raw_len: usize, offset: i64, total: i64) -> PageStep {
    if raw_len > 0 {
        PageStep::Process
    } else if total == 0 || offset >= total {
        PageStep::EndWindow
    } else {
        PageStep::RetryEmpty
    }
}

/// The next page offset, advanced by the RAW page length.
///
/// A one-line function so the single most dangerous mistake in this file has one
/// definition and one test: advancing by the PARSED count instead would step the cursor
/// short of the API's own pagination on every page that dropped a record, silently
/// re-fetching some records and — combined with a `< PAGE_LIMIT` end test — skipping
/// others. `saturating_add` because `offset` is a plain `i64` fed by an upstream length.
fn next_offset(offset: i64, raw_len: usize) -> i64 {
    offset.saturating_add(raw_len as i64)
}

/// The HTTP status embedded in a `get_with_retry` error message
/// (`"MangaDex /manga error 400 Bad Request"` → `400`), if any.
fn page_error_status(msg: &str) -> Option<u16> {
    msg.rsplit_once(" error ")?
        .1
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Whether a page fetch that ALREADY exhausted `get_with_retry`'s own four attempts is
/// one the caller should absorb (cut the window short, keep the cursor) rather than let
/// abort the whole sweep. MangaDex emits sporadic 400s under fleet-shared-IP load — the
/// 2026-07-24 backfill boot logged twenty of them, three of which outlasted every retry
/// — and none of those means "this window is malformed"; the same is true of 429/5xx and
/// of transport failures (timeout, connection reset). A real 401/403/404, or a JSON
/// decode failure, still propagates.
fn is_tolerable_page_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    if msg.contains("request failed") {
        return true; // transport-level: timeout / connect reset, already retried twice
    }
    matches!(
        page_error_status(&msg),
        Some(400) | Some(408) | Some(429) | Some(500..=599)
    )
}

/// The result of one `sync_catalogue` or `sync_chapters` sweep.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepOutcome {
    /// Records upserted: works for the catalogue sweep, chapters for the firehose.
    pub upserted: u64,
    /// Records fetched from `/manga` (or `/chapter`) that failed to parse and so never
    /// reached the spine. NON-ZERO MUST NOT LATCH `seed_done`: the cursor is forward-only
    /// (`updatedAtSince` never revisits an old `createdAt`), so a dropped record is lost
    /// until a full re-seed — which is exactly how 4,493 works went missing while the
    /// seed reported success.
    pub dropped: u64,
    /// Records that reached the write and whose upsert ERRORED (SQLITE_BUSY under the
    /// scanner/cover/Litestream writers, a constraint violation, a closed pool). Same
    /// class of loss as `dropped` — the record was fetched and is not in the mirror — and
    /// gated the same way: NON-ZERO MUST NOT LATCH `seed_done`, or the forward-only
    /// `updatedAtSince` window makes the miss permanent.
    ///
    /// Populated by BOTH sweeps. The catalogue sweep's upsert error arm used to only
    /// `warn!`, so this counter was always zero for it and `seed_done` could latch straight
    /// over a lost work — half of F9. `catalogue_seed_may_latch` now reads it.
    pub failed: u64,
    /// Chapters the firehose offered whose WORK is not catalogued yet, so there was nowhere
    /// to attach them (F9).
    ///
    /// This is not a loss and must not be treated as one — but it must not be treated as
    /// success either. The window is forward-only `updatedAtSince`, so the fix is that a
    /// skipped chapter DOES NOT ADVANCE THE CURSOR: it stays inside the next window and is
    /// re-offered once the catalogue sweep (which runs first) has caught up. The counter
    /// exists so that "the mirror is behind because the catalogue is behind" is visible in
    /// the log line rather than being indistinguishable from an idle pass.
    pub skipped: u64,
    /// Chapters already mirrored with every mutable column identical, so no write was
    /// issued (`chapter_row_unchanged`). Counted so the seed's log line can say "876k
    /// examined, 71k written, 805k already correct" instead of reporting a near-zero
    /// `stored` that reads like a broken sweep.
    pub unchanged: u64,
    /// Works whose merged-feed row this sweep published or refreshed incrementally
    /// ([`flush_touched_feed_rows`]). Not a loss counter and it gates nothing — it is the
    /// only visible evidence that the mirror half's incremental writer ran at all, which is
    /// how it went a whole phase never creating a row without anyone noticing (§8h).
    pub feed: u64,
}

/// The MangaDex half's incremental feed writer — one pass over the `source_series` this
/// firehose PAGE wrote a chapter to.
///
/// EXTRACTED FROM `sync_chapters`, and that is the point. It was an inline block whose only
/// caller was production and which no test could reach (§8h); it was consequently shipped
/// with a defect that made it a no-op on exactly the case it existed for — see
/// [`catalog::publish_mirror_feed_row`], which is now the second half of it.
///
/// PER PAGE, NOT PER CHAPTER. A page is <=100 chapters and typically touches only a handful
/// of distinct series, so this collapses ~100 potential writes into a few — while still
/// bounding staleness by one page (seconds), not by one 6-hourly rebuild (F5, and the direct
/// cause of the owner's "the moment a new chapter is found by the scanner, it will be sent to
/// the updates page"). Deduping through the caller's `HashSet` is what makes a re-seed —
/// which re-offers a series' whole backlog — cost one feed write per series rather than one
/// per chapter. The set is DRAINED, so the next page starts empty.
///
/// BEST-EFFORT, DELIBERATELY, and unchanged from the original: the mirror is the durable
/// record and the feed is a projection of it that the periodic rebuild reconciles anyway.
/// Failing a firehose window because a feed row could not be refreshed would trade a
/// freshness problem for a data-loss one, and `SweepOutcome::failed` is reserved for chapters
/// that did not get mirrored. What is NOT unchanged is the silence: every arm now counts, and
/// the return value is the number of works that reached the feed.
async fn flush_touched_feed_rows(
    pool: &sqlx::SqlitePool,
    touched: &mut std::collections::HashSet<String>,
) -> u64 {
    let mut published = 0u64;
    for ssid in touched.drain() {
        // The ledger first: `publish_mirror_feed_row`'s last pass projects the release clock
        // FROM it, so recording this page's chapters afterwards would publish the row with
        // the previous clock and leave the new one for the next rebuild.
        if let Err(e) = catalog::ledger::record_source_series(pool, &ssid).await {
            tracing::warn!(error = %e, "mangadex: release-ledger write failed");
            continue;
        }
        match catalog::work_id_for_source_series(pool, &ssid).await {
            Ok(Some(work_id)) => match catalog::publish_mirror_feed_row(pool, &work_id).await {
                Ok(true) => published += 1,
                // Not an error: the work's every English chapter is still unreleased (a
                // far-future `publishAt`), so it is correctly absent from the feed.
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "mangadex: feed publish failed"),
            },
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "mangadex: feed work lookup failed"),
        }
    }
    published
}

/// Full/incremental catalogue sweep. Pages `/manga` ordered by `createdAt`, sliding
/// the `createdAtSince` window past the 10k offset cap, upserting each work. A
/// failed *record* upsert is logged and skipped, but a page that still errors
/// after the retry layer (`get_with_retry`) aborts the sweep with `Err`; the cursor
/// is left unchanged, so the next cycle retries the same window. Pass
/// `initial_since` to resume/incrementally refresh from a known timestamp.
pub async fn sync_catalogue(
    pool: &sqlx::SqlitePool,
    client: &MangaDexClient,
    window: SyncWindow,
    initial_since: Option<String>,
    cover_phash: bool,
    job: &str,
) -> Result<SweepOutcome> {
    let mut since = initial_since;
    let mut out = SweepOutcome::default();
    loop {
        let mut offset = 0i64;
        let mut last_created: Option<String> = None;
        let mut done = false;
        let mut empty_retries = 0u32;
        loop {
            let MangaPage {
                mangas,
                total,
                raw_len,
                dropped,
            } = client.list_manga(window, since.as_deref(), offset).await?;
            out.dropped += dropped;
            // Completion is decided by the RAW page length + `total`, NEVER by the
            // parsed count: a page trimmed only by unparseable records (list_manga skips
            // them) must not be mistaken for the last page — that single misread at
            // offset ~1198 is what truncated the original seed and latched seed_done.
            match classify_page(raw_len, offset, total) {
                PageStep::EndWindow => {
                    done = true; // consumed the whole window — genuine end
                    break;
                }
                PageStep::RetryEmpty => {
                    if empty_retries < EMPTY_PAGE_RETRIES {
                        empty_retries += 1;
                        tracing::warn!(
                            offset,
                            total,
                            empty_retries,
                            "mangadex: empty catalogue page before reaching total — retrying"
                        );
                        continue; // transient blip: re-fetch the same offset
                    }
                    tracing::error!(
                        offset,
                        total,
                        "mangadex: repeated empty catalogue page before total — ending window"
                    );
                    done = true;
                    break;
                }
                PageStep::Process => {}
            }
            empty_retries = 0;
            for m in &mangas {
                let (id, mut input) = to_work_input(m);
                // Cover pHash ingest (opt-in): one extra cover download per work,
                // hashed for the dedup cover signal. Best-effort — a failure leaves
                // cover_phash None and COALESCE keeps any prior hash on re-sync.
                if cover_phash {
                    if let Some(fname) = cover_file_name(m) {
                        input.cover_phash = client.cover_phash(&id, &fname).await;
                    }
                }
                // Retry a lock-contended upsert a few times (the cover drainer +
                // scanner write the main DB during the seed) so transient BUSY doesn't
                // silently drop a work from the spine.
                let mut result = catalog::upsert_work_from_mangadex(pool, &id, &input).await;
                let mut lock_retry = 0u32;
                while let Err(e) = &result {
                    if lock_retry >= UPSERT_LOCK_RETRIES || !is_locked_error(e) {
                        break;
                    }
                    lock_retry += 1;
                    tokio::time::sleep(Duration::from_millis(150 * lock_retry as u64)).await;
                    result = catalog::upsert_work_from_mangadex(pool, &id, &input).await;
                }
                match result {
                    Ok(_) => out.upserted += 1,
                    // COUNTED, not just warned (F9). `out.failed` is what
                    // `catalogue_seed_may_latch` consults, and a warn-and-forget here meant
                    // a seed could lose works to a persistent write error and still latch
                    // `seed_done` — after which the forward-only `updatedAtSince` window
                    // never revisits an old `createdAt` and the loss is permanent without a
                    // manual re-seed. That is exactly how the catalogue's 4,493-record gap
                    // became permanent, and the chapter half already counts its failures
                    // for the same reason; this arm was the remaining asymmetry.
                    Err(e) => {
                        out.failed += 1;
                        tracing::warn!(manga = %id, error = %e, "mangadex: upsert failed");
                    }
                }
                if let Some(ts) = manga_window_ts(m, window) {
                    last_created = Some(ts);
                }
            }
            // Advance by the RAW page size so `offset` stays aligned with the API's
            // pagination and `total`; parse-skips must not desync the cursor.
            offset = next_offset(offset, raw_len);
            tracing::info!(
                offset,
                total,
                upserted = out.upserted,
                dropped = out.dropped,
                "mangadex: catalogue page"
            );
            // Checkpoint the seed cursor after EVERY page (not only on window-slide) so a
            // mid-window crash/restart resumes near here instead of re-walking the window
            // from its start. Only during the createdAt seed; incremental cursors are
            // written once, at cycle end.
            if window == SyncWindow::Created {
                if let Some(s) = last_created.as_deref().and_then(to_since) {
                    if let Err(e) = catalog::set_seed_progress(pool, job, &s).await {
                        tracing::warn!(error = %e, "mangadex: failed to checkpoint catalogue seed");
                    }
                }
            }
            if offset >= total {
                done = true; // consumed the whole window
                break;
            }
            if offset >= WINDOW_OFFSET_CAP {
                break; // slide the window past the 10k offset cap
            }
        }
        if done {
            break;
        }
        // Slide the window forward. If it can't advance (all records share the
        // boundary second), stop rather than loop forever.
        let next_since = last_created.as_deref().and_then(to_since);
        if next_since.is_none() || next_since == since {
            // The window can't advance because >9,900 records share this boundary
            // second (createdAt has 1s resolution). Rather than stop the whole sweep
            // here (which stalls the seed on this second every cycle), step the cursor
            // to the NEXT second: this loses only the tail of records past offset
            // 9,900 in the tied second and lets the sweep continue past it.
            match last_created.as_deref().and_then(to_since_next_second) {
                Some(stepped) if Some(&stepped) != since.as_ref() => {
                    tracing::warn!(
                        since = since.as_deref().unwrap_or("<none>"),
                        resume = %stepped,
                        "mangadex: >9900 records share a catalogue boundary second — \
                         skipping the tail past offset 9900 and resuming at the next second"
                    );
                    since = Some(stepped);
                }
                _ => {
                    tracing::error!(
                        since = since.as_deref().unwrap_or("<none>"),
                        "mangadex: catalogue window stuck on a boundary second and the \
                         cursor cannot be stepped — records past offset 9900 are dropped"
                    );
                    break;
                }
            }
        } else {
            since = next_since;
        }
        // Checkpoint the seed's progress so an abort resumes from this window
        // rather than restarting at createdAt=0 (M6). Only during the createdAt
        // seed; incremental cursors are written once, at cycle end.
        if window == SyncWindow::Created {
            if let Some(ref s) = since {
                if let Err(e) = catalog::set_seed_progress(pool, job, s).await {
                    tracing::warn!(error = %e, "mangadex: failed to checkpoint catalogue seed");
                }
            }
        }
    }
    if out.dropped > 0 {
        // Deliberately not "completion will be withheld": that is only true of a sweep
        // that is still seeding. On an already-seeded (incremental) sweep the cursor
        // advances regardless — see `run_one_cycle` — because refusing to advance it
        // would freeze catalogue updates entirely on one permanently-malformed record.
        tracing::error!(
            upserted = out.upserted,
            dropped = out.dropped,
            "mangadex: catalogue sweep DROPPED records — they were fetched and never \
             reached the spine; the gap backfill is what repairs them"
        );
    }
    tracing::info!(
        upserted = out.upserted,
        dropped = out.dropped,
        "mangadex: catalogue sweep complete"
    );
    Ok(out)
}

// ---- DEPLOY NOTE: the v2 catalogue backfill (2026-07-26) -------------------------
//
// `CATALOGUE_SYNC=on` and `COVER_PHASH=off` in production, so the corrected walk starts
// itself ~45s after the next server restart. It is a one-shot; these are the numbers that
// tell success apart from another silent no-op.
//
// EXPECT, on the pass that reports `complete=true`:
//   backfill: complete  scanned≈113,760  ingested≈4,493  dropped=0  failed=0  truncated=0
// and per-pass `dropped=0` throughout. `scanned` summed over the passes is the giveaway:
// v1 summed to 109,261 (parsed records only) — v2 must sum to ≈113,760, the upstream
// `total` measured live on 2026-07-26. The catalogue grows a few dozen records a day, so
// treat ≥113,759 as the floor, not an exact equality.
//
// THE CRISP DB CHECK (exact, independent of dedup folding):
//   SELECT COUNT(DISTINCT source_key) FROM source_series WHERE source_type='mangadex';
//   109,266 (before)  ->  ~113,760 (after)
// Net new `work` rows will be LOWER than +4,493, because `run_post_ingest_dedup_ex` folds
// backfilled works into existing Suwayomi-anchored ones via shared aliases. A crude
// normalized-title match puts the fold at somewhere between ~140 and ~557 of the 4,493
// (the loose upper bound is mine, measured 2026-07-26; Tier-2 dedup rejects many of those
// on other signals), so expect roughly +3,950…+4,350 `work` rows. Do NOT treat that as a
// pass/fail number — use the `source_series` count above.
//
// TIMING: v1's enumeration took ~25 min of wall clock at 4 req/s (pass 1 spent its full
// 20-min budget at scanned=83,805; pass 2 finished the tail in ~5 min). v2 adds ~4,493
// presence-checks-plus-upserts to that, so expect completion on PASS 2 — boot +45s, 20 min
// of pass 1, a 30 min gap, then a few minutes — i.e. ~55-60 min after restart, possibly
// pass 3 if the DB is busy. Not a regression: only `is_clean_completion()` latches.
//
// ~2,032 of the 4,493 arrive `is_nsfw=1` (1,964 `pornographic` + 68 `erotica`, measured
// over the cohort's own `/manga` bodies), so they are hidden from anonymous surfaces on
// arrival. EXPECTED, not a regression — the catalogue stores the flag and gates at query
// time. (Separately pre-existing: ~2,500 mainstream works are wrongly flagged NSFW.)
//
// THE SAME DEPLOY ALSO CHANGES THE CHAPTER FIREHOSE (2026-07-26 review). `sync_chapters`
// paged on the PARSED count, so one unparseable chapter shortened a page to 99, tripped
// the `< PAGE_LIMIT` end-of-window test and ended the window while the cursor still jumped
// to the cycle's start — silently unmirrored chapters, forever. It now pages on the raw
// length and retries a transient empty page. Watch for `chapter cycle done  dropped=N`:
// N should be 0. The chapter mirror is at 805,307 rows and both seeds are already done, so
// this only affects the incremental sweep.
//
// ANOTHER SILENT NO-OP LOOKS LIKE: `scanned≈109,2xx` with `ingested=0` and the marker
// latched — i.e. the whole catalogue enumerated, nothing added. With this change that
// cannot latch, because those same records now count into `dropped`, which blocks
// `is_clean_completion()` and rewinds the cursor; and each one is logged at ERROR with its
// uuid and the serde message. If it somehow recurs, `docker logs | grep DROPPED` names the
// records and the reason directly.
//
// -----------------------------------------------------------------------------------

/// The maintenance key for the one-time gap backfill (migration 0055).
///
/// VERSIONED TO `_v2` ON 2026-07-26. `_v1` ran to a "clean completion" on 2026-07-26
/// 08:44 and latched its marker having ingested nothing, because every one of the 4,493
/// records it was meant to repair failed to parse (`links: null`) and the drop was
/// discarded — see `null_as_default`. Nothing ever clears `maintenance_flag`, so the only
/// way to re-arm the corrected walk is a new key.
///
/// WHY A CONSTANT BUMP AND NOT A MIGRATION OR AN ADMIN MUTATION: the code deploy is
/// mandatory regardless (the parse fix lives in the same file), so the bump costs nothing
/// extra; a migration whose semantics are "un-do a done-marker" is not repeatable and
/// reads oddly in the migration history forever; and versioning KEEPS v1's rows as audit
/// history of the no-op instead of rewriting them.
/// VERSIONED TO `_v3` ON 2026-07-26, hours after `_v2` completed. `_v2` did its job (the
/// 4,493-record cohort is in), so it latched its marker and will never run again — but the
/// walk it performed predates migration 0066, so it wrote no `work_tag` rows. `work_tag`
/// is filled only by ingest, and the recurring sweep only revisits works whose upstream
/// record CHANGED, so without a new generation the ~113k works catalogued before 0066
/// would wait years to acquire a genre. `_v3` re-walks the catalogue with the genre
/// top-up branch active (see `refresh_source_tags` at the presence check).
const BACKFILL_FLAG: &str = "mangadex_catalogue_backfill_v3";

/// The `catalogue_sync_state` job key holding the backfill's window cursor. A key of its
/// own so it never disturbs the `catalogue`/`chapters` rows the recurring sweep reads;
/// only `last_synced_at` is used (`seed_done` stays 0 — completion is the
/// `maintenance_flag` marker, which is the one-shot the boot task keys on).
///
/// VERSIONED TO `_v2` ALONGSIDE `BACKFILL_FLAG`, AND BOTH BUMPS ARE MANDATORY. `_v1`
/// finished its walk, so `catalogue_sync_state('catalogue_backfill').last_synced_at` sits
/// at 2026-07-25T22:16:20 — the newest end of the catalogue. `backfill_missing_catalogue`
/// RESUMES from that cursor, so renaming only the flag would restart the walk in mid-2026
/// and skip the entire 2018–2021 window where 100% of the 4,493-record cohort lives,
/// ingesting zero again. A fresh job key has no row, so `get_sync_state` returns `None`,
/// `since` stays `None`, and the walk starts from the beginning of the catalogue (2018-01).
/// VERSIONED TO `_v3` ALONGSIDE `BACKFILL_FLAG` — the same "BOTH BUMPS ARE MANDATORY"
/// rule the `_v2` note above spells out, and for the identical reason: `_v2` ran to
/// completion, so its cursor sits at the NEWEST end of the catalogue, and renaming only
/// the flag would resume there and skip every work that needs genres. A fresh job key has
/// no row, so `since` stays `None` and the walk restarts from 2018-01.
const BACKFILL_JOB: &str = "catalogue_backfill_v3";

/// How many extra times a backfill page fetch that already survived `get_with_retry`'s
/// own four attempts is re-tried, after a much longer cool-down, before the window is
/// cut short. `get_with_retry`'s ladder tops out at 4s, which the 2026-07-24 400 burst
/// outlasted; a 30s pause clears bursts of that shape.
const PAGE_ERROR_RETRIES: u32 = 2;
const PAGE_ERROR_COOLDOWN: Duration = Duration::from_secs(30);

/// How long the spawned backfill waits before touching the DB or MangaDex at all. The
/// boot write burst — the scanner's first tick, `refresh_feed_updates` / `refresh_work_fts`
/// (themselves staggered ~20s in `main.rs`) and Litestream's checkpoint — ran from
/// t+0 to roughly t+30s on the 2026-07-24 boot and cost four works to SQLITE_BUSY.
/// 45s clears that window with margin. `main.rs` isn't ours to stagger, so the delay
/// lives here, inside the spawned task, and needs no call-site change.
const BACKFILL_BOOT_DELAY: Duration = Duration::from_secs(45);

/// Wall-clock budget for ONE backfill pass. A pass holds the catalogue single-flight
/// lock, and a full `/manga` enumeration is ~1,140 pages at 4 req/s; without a budget
/// the backfill would own the lock (and so suppress the recurring sweep's tick) for the
/// whole run. Each pass stops on the budget with its cursor persisted and the next pass
/// resumes exactly there.
const BACKFILL_PASS_BUDGET: Duration = Duration::from_secs(20 * 60);

/// Idle gap between passes, so the recurring sweep gets the lock in between.
const BACKFILL_PASS_GAP: Duration = Duration::from_secs(30 * 60);

/// Consecutive zero-progress passes (lock permanently busy, or a pass that scans
/// nothing) after which this process gives up and leaves the rest to a later boot.
const BACKFILL_IDLE_PASSES: u32 = 3;

/// Hard ceiling on passes per process, so a pathological "sweeps fine but always loses a
/// record to BUSY" state re-walks the catalogue a bounded number of times rather than
/// forever. 24 passes × 30min ≈ half a day of attempts.
///
/// THE FAILURE MODE THIS BOUNDS, stated plainly, because `dropped` now feeds `is_dirty()`
/// and so widens it: if ONE upstream record is permanently unparseable, every pass is
/// dirty, every dirty pass rewinds the cursor, and the process re-walks from 2018 until
/// this ceiling — ~20 hours of passes, repeated on every boot. That is deliberate (a
/// record we fetch and cannot store is a real gap, and the alternative is latching a
/// marker over it that nothing can ever clear) but it is NOT free: it burns the shared
/// 4 req/s MangaDex budget. The ceiling is the bound, and hitting it logs at ERROR with
/// the last pass's loss counters so the operator can fix the record (or widen the parse)
/// rather than watch it spin. A rewind costs nothing on the DB side — present ids are
/// skipped — so the cost is purely upstream requests.
const BACKFILL_MAX_PASSES: u32 = 24;

/// The result of one bounded backfill pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BackfillOutcome {
    /// Records enumerated from `/manga` this pass AND successfully parsed. Records that
    /// were fetched and failed to parse land in `dropped`, not here — so `scanned +
    /// dropped` is what compares against the upstream `total`, and a `scanned` short of
    /// it with `dropped == 0` means the walk did not cover the catalogue.
    pub scanned: u64,
    /// Records absent from `source_series` that were upserted.
    pub ingested: u64,
    /// Records ALREADY in the spine whose `work_tag` upstream half was (re)written from
    /// the same `/manga` page (migration 0066).
    ///
    /// Deliberately NOT counted in `ingested`: no work was added, and conflating the two
    /// would make "did the gap close?" unanswerable from the log line. It is also not a
    /// completion criterion — a tag write is a metadata top-up, whereas `ingested` is the
    /// catalogue itself.
    pub tagged: u64,
    /// Records this pass could not persist (SQLITE_BUSY outlasting `UPSERT_LOCK_RETRIES`,
    /// or a failed presence check). NON-ZERO BLOCKS THE ONE-SHOT COMPLETION MARKER — a
    /// partially-complete backfill must never mark itself done, or those ids are lost
    /// for good (four were, on 2026-07-24).
    pub failed: u64,
    /// Whether this pass reached the end of the catalogue. False when the pass stopped
    /// on its wall-clock budget or on a window that could not advance.
    pub complete: bool,
    /// Windows cut short by a page that stayed empty past `EMPTY_PAGE_RETRIES` while
    /// `offset < total`, i.e. records the API said existed but never handed over.
    ///
    /// Tracked SEPARATELY from `complete` because a truncated window still lets the
    /// loop reach the end of the catalogue, and an exhausted-empty page is EXACTLY the
    /// blip that truncated the original seed and latched `seed_done` — the gap this
    /// backfill exists to repair. Latching the one-shot marker after one would make the
    /// gap permanent and unrepairable, since nothing ever clears `maintenance_flag`.
    pub truncated: u64,
    /// Records this pass FETCHED from `/manga` and then failed to parse, so they never
    /// reached the presence check, let alone the spine (`MangaPage::dropped`).
    ///
    /// This field exists because of 2026-07-26: the v1 backfill enumerated the whole
    /// catalogue, dropped all 4,493 records it existed to repair on the `links: null`
    /// parse, reported `scanned = 109,266 / ingested = 0 / failed = 0 / truncated = 0`,
    /// passed `is_clean_completion()`, and latched a permanent marker over a total no-op.
    /// A fetched-but-unpersisted record is the same class of loss as `failed`/`truncated`,
    /// so it blocks the marker AND forces the cursor rewind.
    pub dropped: u64,
}

impl BackfillOutcome {
    /// The ONLY condition under which the one-shot completion marker may be latched:
    /// the sweep reached the end of the catalogue having lost nothing and skipped
    /// nothing. Anything less resumes on a later pass or a later boot.
    pub fn is_clean_completion(&self) -> bool {
        self.complete && self.failed == 0 && self.truncated == 0 && self.dropped == 0
    }

    /// Whether this pass left the catalogue in a state that must not be resumed from —
    /// the cursor has advanced past records that were never persisted, so completing
    /// from here would skip them for good.
    pub fn is_dirty(&self) -> bool {
        self.failed > 0 || self.truncated > 0 || self.dropped > 0
    }
}

/// Fill catalogue gaps the original `createdAt` seed missed (transient empty-page /
/// parse blips truncated specific windows; the forward-only `updatedAtSince`
/// incremental never revisits them). Enumerates the WHOLE `/manga` catalogue by
/// `createdAt` asc (all content ratings) exactly like `sync_catalogue`, but upserts
/// ONLY ids absent from `source_series` — so it's a lightweight top-up, not a full
/// re-seed, and it does not touch the chapter firehose. Returns `(scanned, ingested)`.
///
/// The window-sliding is the seed's: slide `since` to the last record's `createdAt`
/// (overlap is harmless — present ids are skipped), and step +1s only on the
/// pathological >9,900-in-one-second boundary.
///
/// RESUMABILITY: the window cursor is checkpointed to `catalogue_sync_state` under
/// `BACKFILL_JOB` after every completed window, and the pass RESUMES from it. Previously
/// the cursor lived in a local and every pass restarted at `since = NULL`, so a failure
/// threw away all progress and re-burned ~3 minutes of the shared MangaDex budget
/// re-enumerating the same first window on every boot.
///
/// `budget`, when set, stops the pass (cleanly, cursor persisted, `complete = false`)
/// once that much wall-clock has elapsed, so the pass can release the catalogue
/// single-flight lock and resume later.
pub async fn backfill_missing_catalogue(
    pool: &sqlx::SqlitePool,
    client: &MangaDexClient,
    cover_phash: bool,
    budget: Option<Duration>,
) -> Result<BackfillOutcome> {
    let started = Instant::now();
    let mut since: Option<String> = catalog::get_sync_state(pool, BACKFILL_JOB)
        .await
        .ok()
        .flatten()
        .map(|s| s.cursor);
    if let Some(ref s) = since {
        tracing::info!(resume = %s, "backfill: resuming from the stored window cursor");
    }
    let mut out = BackfillOutcome::default();
    loop {
        let mut offset = 0i64;
        let mut last_created: Option<String> = None;
        let mut done = false;
        let mut empty_retries = 0u32;
        let mut page_errors = 0u32;
        loop {
            let page = client
                .list_manga(SyncWindow::Created, since.as_deref(), offset)
                .await;
            let MangaPage {
                mangas,
                total,
                raw_len,
                dropped,
            } = match page {
                Ok(p) => p,
                // A page error that outlived `get_with_retry` must NOT kill the pass:
                // MangaDex's sporadic 400s are transient, and the `?` that used to be
                // here is exactly what ended the 2026-07-24 run with 3 of ~4,500 series
                // ingested. Cool off and re-fetch; if it persists, end the window here
                // (the cursor slides on, so the pass advances instead of dying).
                Err(e) if is_tolerable_page_error(&e) => {
                    if page_errors < PAGE_ERROR_RETRIES {
                        page_errors += 1;
                        tracing::warn!(
                            error = %e, offset, page_errors,
                            "backfill: page fetch failed after retries — cooling down"
                        );
                        tokio::time::sleep(PAGE_ERROR_COOLDOWN).await;
                        continue;
                    }
                    tracing::warn!(
                        error = %e, offset,
                        "backfill: page still failing — ending this window and sliding on"
                    );
                    break; // done stays false → slide the window below
                }
                Err(e) => return Err(e),
            };
            page_errors = 0;
            // Fetched-but-unparseable records: counted BEFORE `classify_page`, because a
            // page that is 100% drops still has to advance `offset` by `raw_len` and must
            // still block completion. This is the counter that was missing on 2026-07-26,
            // when a whole 4,493-record cohort came back and went nowhere in silence.
            out.dropped += dropped;
            match classify_page(raw_len, offset, total) {
                PageStep::EndWindow => {
                    done = true;
                    break;
                }
                PageStep::RetryEmpty => {
                    if empty_retries < EMPTY_PAGE_RETRIES {
                        empty_retries += 1;
                        continue;
                    }
                    // `offset < total`: MangaDex told us more records exist in this
                    // window and then kept handing back an empty page. This is the
                    // ORIGINAL truncation blip — the one that ended the seed early and
                    // latched `seed_done`. Do NOT set `done`: that would mark the pass
                    // `complete`, and a complete+clean pass latches a one-shot
                    // `maintenance_flag` that nothing ever clears, freezing the very gap
                    // this backfill exists to close. Record it and slide the window so
                    // the pass still makes progress, but keep completion blocked.
                    out.truncated += 1;
                    tracing::error!(
                        offset,
                        total,
                        "backfill: repeated empty page before total — window truncated; \
                         completion withheld"
                    );
                    break;
                }
                PageStep::Process => {}
            }
            empty_retries = 0;
            for m in &mangas {
                out.scanned += 1;
                if let Some(ts) = manga_window_ts(m, SyncWindow::Created) {
                    last_created = Some(ts);
                }
                let (id, mut input) = to_work_input(m);
                // Skip ids we already carry — this is a top-up, not a re-seed. A failed
                // presence check (BUSY on the read side) counts as a lost record rather
                // than aborting the pass: we can't tell whether it needs ingesting.
                // Selects `work_id`, not a `1` sentinel: a present work still needs its
                // genres topped up below, and that write is keyed by work id.
                let present: Option<String> = match sqlx::query_scalar(
                    "SELECT work_id FROM source_series \
                     WHERE source_type = 'mangadex' AND source_key = ? LIMIT 1",
                )
                .bind(&id)
                .fetch_optional(pool)
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        out.failed += 1;
                        tracing::warn!(manga = %id, error = %e, "backfill: presence check failed");
                        continue;
                    }
                };
                if let Some(work_id) = present {
                    // GENRE TOP-UP (migration 0066). The full upsert is still skipped —
                    // this is a top-up, not a re-seed — but `work_tag` was introduced
                    // EMPTY and is only written by ingest from this deploy onward, so the
                    // ~113k works catalogued before it each need exactly one visit to
                    // gain the tags this very `/manga` page is already carrying. Without
                    // this branch a generation bump would walk the whole catalogue, skip
                    // every record, and write zero genres.
                    //
                    // Costs no extra upstream request and one small transaction. Skipped
                    // entirely when upstream sent no tags, so a work with no tags is not
                    // rewritten on every generation for nothing.
                    if !input.tags.is_empty() {
                        let mut res =
                            catalog::refresh_source_tags(pool, &work_id, &input.tags).await;
                        let mut lock_retry = 0u32;
                        while let Err(e) = &res {
                            if lock_retry >= UPSERT_LOCK_RETRIES || !is_locked_error(e) {
                                break;
                            }
                            lock_retry += 1;
                            tokio::time::sleep(Duration::from_millis(150 * lock_retry as u64))
                                .await;
                            res = catalog::refresh_source_tags(pool, &work_id, &input.tags).await;
                        }
                        match res {
                            Ok(()) => out.tagged += 1,
                            Err(e) => {
                                // Counted into `failed` for the same reason an upsert
                                // failure is: it withholds the one-shot completion
                                // marker, so the record gets another pass rather than
                                // being silently left genreless forever.
                                out.failed += 1;
                                tracing::warn!(
                                    manga = %id, work = %work_id, error = %e,
                                    "backfill: tag refresh failed"
                                );
                            }
                        }
                    }
                    continue;
                }
                if cover_phash {
                    if let Some(fname) = cover_file_name(m) {
                        input.cover_phash = client.cover_phash(&id, &fname).await;
                    }
                }
                let mut result = catalog::upsert_work_from_mangadex(pool, &id, &input).await;
                let mut lock_retry = 0u32;
                while let Err(e) = &result {
                    if lock_retry >= UPSERT_LOCK_RETRIES || !is_locked_error(e) {
                        break;
                    }
                    lock_retry += 1;
                    tokio::time::sleep(Duration::from_millis(150 * lock_retry as u64)).await;
                    result = catalog::upsert_work_from_mangadex(pool, &id, &input).await;
                }
                match result {
                    Ok(_) => out.ingested += 1,
                    Err(e) => {
                        // Counted, not just logged: the caller refuses to latch the
                        // one-shot completion marker while `failed > 0`, so these ids
                        // get another pass instead of being silently abandoned.
                        out.failed += 1;
                        tracing::warn!(manga = %id, error = %e, "backfill: upsert failed");
                    }
                }
            }
            // Checkpoint after EVERY page, exactly as the seed does, so a crash or a
            // budget stop mid-window resumes near here instead of at the window's start.
            // Resuming from an already-processed record's `createdAt` only re-enumerates
            // a little, and present ids are skipped, so the overlap costs nothing.
            if let Some(s) = last_created.as_deref().and_then(to_since) {
                if let Err(e) = catalog::set_seed_progress(pool, BACKFILL_JOB, &s).await {
                    tracing::warn!(error = %e, "backfill: failed to checkpoint the window cursor");
                }
            }
            // RAW length, never `mangas.len()` — see `next_offset`.
            offset = next_offset(offset, raw_len);
            if offset >= total {
                done = true;
                break;
            }
            if offset >= WINDOW_OFFSET_CAP {
                break;
            }
            // Budget is checked here too, not just at window boundaries: a window is up
            // to 9,900 records, so a boundary-only check could overrun it many-fold.
            // Breaking with `done = false` falls through to the slide + outer check.
            if budget.is_some_and(|b| started.elapsed() >= b) {
                break;
            }
        }
        if done {
            out.complete = true;
            break;
        }
        // Slide the window. `slid == false` means the cursor cannot advance at all (no
        // record seen this window — e.g. the very first page kept failing); stop rather
        // than spin, leaving the stored cursor for the next pass.
        let next_since = last_created.as_deref().and_then(to_since);
        let slid = if next_since.is_none() || next_since == since {
            match last_created.as_deref().and_then(to_since_next_second) {
                Some(stepped) if Some(&stepped) != since.as_ref() => {
                    since = Some(stepped);
                    true
                }
                _ => false,
            }
        } else {
            since = next_since;
            true
        };
        if !slid {
            tracing::warn!(
                since = since.as_deref().unwrap_or("<none>"),
                "backfill: window could not advance — keeping the cursor for the next pass"
            );
            break;
        }
        // Checkpoint after every completed window so a crash/failure resumes here.
        if let Some(ref s) = since {
            if let Err(e) = catalog::set_seed_progress(pool, BACKFILL_JOB, s).await {
                tracing::warn!(error = %e, "backfill: failed to checkpoint the window cursor");
            }
        }
        tracing::info!(
            scanned = out.scanned,
            ingested = out.ingested,
            tagged = out.tagged,
            failed = out.failed,
            dropped = out.dropped,
            since = %since.as_deref().unwrap_or("<none>"),
            "backfill: catalogue window done"
        );
        if budget.is_some_and(|b| started.elapsed() >= b) {
            tracing::info!(
                elapsed_secs = started.elapsed().as_secs(),
                "backfill: pass budget spent — pausing here (cursor persisted)"
            );
            break;
        }
    }
    tracing::info!(
        scanned = out.scanned,
        ingested = out.ingested,
        tagged = out.tagged,
        failed = out.failed,
        dropped = out.dropped,
        truncated = out.truncated,
        complete = out.complete,
        "backfill: catalogue gap backfill pass finished"
    );
    Ok(out)
}

/// What the pass scheduler should do next.
enum PassResult {
    /// Finished, or must not run at all — stop scheduling passes in this process.
    Stop,
    /// Run another pass after `BACKFILL_PASS_GAP`. `scanned` drives the idle guard;
    /// `last` is the pass's outcome when it actually ran (absent when the pass never
    /// started — flag read failed, lock busy), so the scheduler can say WHY it gave up.
    Retry {
        scanned: u64,
        last: Option<BackfillOutcome>,
    },
}

/// One bounded backfill pass, under the catalogue single-flight lock.
async fn run_backfill_pass(
    pool: &sqlx::SqlitePool,
    covers: &sqlx::SqlitePool,
    client: &MangaDexClient,
    cover_phash: bool,
) -> PassResult {
    // Already done, or the catalogue seed hasn't finished yet → nothing to do now.
    match catalog::maintenance_flag_present(pool, BACKFILL_FLAG).await {
        Ok(true) => return PassResult::Stop,
        // A read failure here is almost always transient SQLITE_BUSY against the boot
        // write burst. `Stop` would abandon the backfill for the whole process lifetime
        // on a 15s lock timeout, so retry after the gap instead — the flag re-read is
        // free and the pass is a no-op once it really is set.
        Err(e) => {
            tracing::warn!(error = %e, "backfill: flag read failed; retrying after the pass gap");
            return PassResult::Retry {
                scanned: 0,
                last: None,
            };
        }
        Ok(false) => {}
    }
    let seeded = catalog::get_sync_state(pool, "catalogue")
        .await
        .ok()
        .flatten()
        .map(|s| s.seed_done)
        .unwrap_or(false);
    if !seeded {
        tracing::info!("backfill: catalogue seed not complete yet; deferring to a later boot");
        return PassResult::Stop;
    }
    // Don't race the seed / recurring sync / resync for the single writer + rate limit.
    if CATALOGUE_SYNC_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        tracing::info!("backfill: a catalogue sync is running; retrying after the pass gap");
        return PassResult::Retry {
            scanned: 0,
            last: None,
        };
    }
    let _guard = SyncGuard; // releases the lock on completion or panic
    tracing::info!("backfill: starting a catalogue gap backfill pass");
    match backfill_missing_catalogue(pool, client, cover_phash, Some(BACKFILL_PASS_BUDGET)).await {
        Ok(o) if o.is_clean_completion() => {
            tracing::info!(
                scanned = o.scanned,
                ingested = o.ingested,
                tagged = o.tagged,
                "backfill: complete"
            );
            if let Err(e) = catalog::set_maintenance_flag(pool, BACKFILL_FLAG).await {
                tracing::warn!(error = %e, "backfill: failed to persist completion marker");
            }
            // The backfill keys on the MangaDex UUID only, so a backfilled series that
            // the catalogue already had as a Suwayomi-only work becomes a second
            // canonical work. Fold those together now (aliases include the MangaDex
            // altTitles, so older entries under a different name match). Still inside
            // the SyncGuard — nothing else is writing the catalogue.
            //
            // `_ex` with the covers pool: every fold here retires a work whose cached
            // cover blob lives in the separate (un-cascaded) covers DB, so without the
            // pool this one-time backfill leaked one orphan per merge.
            crate::graphql::run_post_ingest_dedup_ex(pool, Some(covers)).await;
            PassResult::Stop
        }
        Ok(o) => {
            // Rewind on ANY dirty pass, not just one that also reached the end.
            //
            // `failed`/`truncated` are PER-PASS, but the completion marker is process-
            // wide and permanent. A budget-truncated pass that lost records used to keep
            // its cursor, so the NEXT pass resumed past those records, could reach the
            // end reporting `failed == 0`, and latched the marker — silently abandoning
            // them. Worse, the cursor survives a restart, so a fresh process would do the
            // same with no memory of the loss at all. Rewinding here makes "reached the
            // end cleanly" mean "re-walked the whole catalogue cleanly", independent of
            // pass and process boundaries. The re-walk is cheap on the DB side (present
            // ids are skipped) and bounded by `BACKFILL_MAX_PASSES`.
            if o.is_dirty() {
                tracing::warn!(
                    failed = o.failed,
                    truncated = o.truncated,
                    dropped = o.dropped,
                    complete = o.complete,
                    "backfill: pass lost or skipped records — NOT marking done; rewinding \
                     the cursor so the next pass re-walks from the start"
                );
                if let Err(e) = catalog::reset_sync_state(pool, BACKFILL_JOB).await {
                    tracing::warn!(error = %e, "backfill: failed to rewind the window cursor");
                }
            }
            PassResult::Retry {
                scanned: o.scanned,
                last: Some(o),
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "backfill: pass failed; cursor kept for the next pass");
            PassResult::Retry {
                scanned: 0,
                last: None,
            }
        }
    }
}

/// Spawn the one-time catalogue gap backfill in the background if it hasn't run yet
/// (marker in `maintenance_flag`, migration 0055). Gated on the catalogue seed being
/// complete (a top-up only makes sense against a finished spine) and on the shared
/// catalogue single-flight lock (so it never races the seed/resync). Non-blocking.
///
/// Runs as a series of budgeted passes rather than one long sweep: each pass checkpoints
/// its window cursor, then releases the single-flight lock for `BACKFILL_PASS_GAP` so the
/// recurring sweep can run. The completion marker is set only when a pass reaches the end
/// of the catalogue with ZERO lost records; anything else simply resumes next pass/boot.
///
/// Takes the shutdown watch so the schedule is INTERRUPTIBLE. The pass ceiling spans up
/// to 24 × (20 min work + 30 min gap) ≈ 20 hours, so without this a redeploy would leave
/// the task sleeping through the drain and still hitting MangaDex — and, because the
/// single-flight lock is taken per pass, still able to start a fresh 20-minute pass while
/// the server is shutting down. Every await point that can block for minutes selects on
/// the signal; the in-flight pass itself is not cancelled mid-record (its cursor is
/// checkpointed per page, so the next boot resumes within one page).
pub fn spawn_backfill_if_needed(
    pool: sqlx::SqlitePool,
    covers: sqlx::SqlitePool,
    client: Arc<MangaDexClient>,
    cover_phash: bool,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        /// Sleep, unless shutdown fires first. `true` => shut down, stop scheduling.
        async fn nap(d: Duration, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> bool {
            tokio::select! {
                _ = tokio::time::sleep(d) => false,
                // A dropped sender means the process is going away and `changed()` will
                // return `Err` INSTANTLY, forever. Reading `*borrow()` there (still
                // `false`) collapsed every nap in this task to a no-op: the 45s boot
                // stagger and all 24 pass gaps would vanish and the passes would run
                // back to back. Treat it as shutdown.
                r = shutdown.changed() => r.is_err() || *shutdown.borrow(),
            }
        }
        // Stagger behind the boot write burst (scanner tick + feed/FTS rebuilds +
        // Litestream checkpoint) — starting at t+0 is what cost four works to
        // SQLITE_BUSY. `main.rs` isn't ours to stagger, so the delay lives here.
        if nap(BACKFILL_BOOT_DELAY, &mut shutdown).await {
            return;
        }
        let mut idle_passes = 0u32;
        let mut last_outcome: Option<BackfillOutcome> = None;
        for pass in 1..=BACKFILL_MAX_PASSES {
            if *shutdown.borrow() {
                tracing::info!(pass, "backfill: shutting down before the next pass");
                return;
            }
            match run_backfill_pass(&pool, &covers, &client, cover_phash).await {
                PassResult::Stop => return,
                PassResult::Retry { scanned, last } => {
                    if last.is_some() {
                        last_outcome = last;
                    }
                    if scanned == 0 {
                        idle_passes += 1;
                        if idle_passes >= BACKFILL_IDLE_PASSES {
                            tracing::warn!(
                                pass,
                                "backfill: no progress in {BACKFILL_IDLE_PASSES} passes; \
                                 leaving the rest to a later boot"
                            );
                            return;
                        }
                    } else {
                        idle_passes = 0;
                    }
                }
            }
            if nap(BACKFILL_PASS_GAP, &mut shutdown).await {
                tracing::info!(pass, "backfill: shutting down during the pass gap");
                return;
            }
        }
        // ERROR, not warn, and with the last pass's counters. Reaching this means ~20
        // hours of passes each re-walked the whole catalogue and none came back clean —
        // i.e. something upstream is permanently unparseable/unwritable and the walk will
        // burn the same budget again on every boot until a human looks. The counters say
        // which of the three loss classes it is; the matching `DROPPED` lines name the
        // records.
        tracing::error!(
            passes = BACKFILL_MAX_PASSES,
            dropped = last_outcome.map(|o| o.dropped).unwrap_or(0),
            failed = last_outcome.map(|o| o.failed).unwrap_or(0),
            truncated = last_outcome.map(|o| o.truncated).unwrap_or(0),
            complete = last_outcome.map(|o| o.complete).unwrap_or(false),
            "backfill: pass ceiling reached without a clean completion — the same walk \
             will be retried on the next boot; check the DROPPED/failed lines above"
        );
    });
}

// ---- A1b: the `readable_at` / `external_url` backfill --------------------------------
//
// Migration 0073 added both columns and deliberately moved no data, on the promise that
// "existing rows are filled by the resumable backfill in
// `mangadex::backfill_chapter_external_urls`". This is that backfill; until it was written
// the promise was false and §7z says so.
//
// WHAT IS STUCK WITHOUT IT (measured on the 2026-07-30 21:39 production snapshot, after the
// C+D deploy):
//
//   * 877,868 `chapter` rows have `readable_at IS NULL`, and ALL of them are MangaDex —
//     0 Suwayomi rows, because the spine writes `readable_at` on the way in. So the
//     work-list below genuinely drains to empty rather than stalling on a residue it can
//     never resolve.
//   * `external_url IS NOT NULL` is at **0 rows**. The firehose's self-heal
//     (`chapter_row_unchanged` compares both columns) only ever re-offers chapters whose
//     upstream record CHANGED, so it reaches the head of the feed and nothing else. The
//     ~35,000-chapter F1 cohort is entirely in the tail.
//   * 236 chapters across 67 works carry the `publishAt = 2037-12-31` sentinel. 46 of those
//     works have nothing else, so they are absent from /updates entirely; the other 21 show
//     a stale older chapter as "latest". Upstream HAS a real `readableAt` for them
//     (exemplar: *Magical Girl Tsubame* ch. 1, `publishAt 2037-12-31` / `readableAt
//     2023-10-07`), which is the only thing that can return them to the feed.
//
// 877,868 rows at 100 per request is ~8,779 requests against a third party, so it is paced
// and it never runs on the hot path.

/// Rows per batch, which is also ids per request: MangaDex's `ids[]` ceiling. The two are
/// deliberately the same number — one work-list query, one upstream request, one
/// transaction — so there is no partial-batch state to get wrong.
const READABLE_BACKFILL_BATCH: i64 = 100;

/// Gap between batches, and the whole pacing argument. 877,868 rows ÷ 100 ≈ 8,779 requests;
/// a 2-second gap is 0.5 req/s against the ~5 req/s per-IP ceiling this process shares with
/// the firehose, the catalogue sweep and the cover crawler, and spreads the drain over
/// ~5 hours of background trickle. There is no deadline here worth spending a bigger share
/// of a fleet-wide budget on: the columns have been NULL since the mirror was built.
/// It also bounds the write side to ~50 rows/s, gentler than the spine drain's ~1,000/s,
/// on a database Litestream is replicating and the scanner is writing concurrently.
const READABLE_BACKFILL_GAP: Duration = Duration::from_secs(2);

/// Stagger behind the boot write burst AND behind the other two background drains, so a
/// restart does not start three of them at t+0 against one SQLite writer:
/// `spawn_backfill_if_needed` waits 45s, `catalog::spine::spawn` waits 90s, this waits 120s.
const READABLE_BACKFILL_BOOT_DELAY: Duration = Duration::from_secs(120);

/// How often to re-check once drained. New MangaDex chapters arrive from the firehose with
/// `readableAt` already parsed, so a drained work-list only refills if a source starts
/// producing rows with no release clock at all — rare, and never urgent. The check itself is
/// a scan of migration 0073's partial index, which is by then empty.
const READABLE_BACKFILL_IDLE_RECHECK: Duration = Duration::from_secs(60 * 60);

/// Batches between progress lines. ~8,779 batches makes per-batch logging useless noise;
/// every 500 is ~18 lines across the whole drain, one per ~17 minutes.
const READABLE_BACKFILL_PROGRESS_EVERY: u64 = 500;

/// One mirrored MangaDex chapter that has no release clock yet.
///
/// `published_at` and `created_at` come along because they are the fallback rungs — see
/// [`resolve_readable_at`]. Reading them here costs nothing (the row is already being
/// fetched) and saves a second query per row on the termination path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingChapter {
    /// `chapter.id`. The UPDATE is keyed on this, not on `external_id`: a MangaDex chapter
    /// uuid can legitimately appear under two `source_series` rows (works that have been
    /// merged), and the work-list already named the exact rows to write.
    id: String,
    /// The MangaDex chapter uuid, i.e. what goes in `ids[]`.
    external_id: String,
    published_at: Option<String>,
    created_at: String,
}

/// What one row's backfill decided to write.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedChapter {
    /// NEVER `Option`. See [`resolve_readable_at`] — a row that leaves here with no value
    /// stays in the work-list forever.
    readable_at: String,
    external_url: Option<String>,
    /// Whether upstream actually returned this id. Drives whether `external_url` is written
    /// at all: when upstream said nothing, writing `NULL` would CLOBBER a URL the firehose
    /// may already have healed onto the row.
    answered: bool,
}

/// What one batch moved. `filled == 0` is the drain's only completion signal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReadableFill {
    /// Rows taken off the work-list. Equals the batch's size by construction — every
    /// pending row is written, whatever upstream said — which is what makes the drain
    /// terminate.
    pub filled: u64,
    /// …of which upstream supplied a real `readableAt`. The rest took a fallback.
    pub from_upstream: u64,
    /// …of which carry an `externalUrl`: the F1 cohort, ~4% of the mirror.
    pub external: u64,
    /// Ids upstream did not answer for at all — deleted chapters, unparseable records, or
    /// anything else that keeps a record out of the response. Counted separately because a
    /// non-trivial number means the request is being filtered (a missing `contentRating[]`
    /// would look exactly like this), not that MangaDex forgot 8,000 chapters.
    pub unanswered: u64,
}

/// The rule that guarantees termination, and the reason it is a free function with no
/// `Option` in its return type.
///
/// THE BUG THIS EXISTS TO PREVENT: this drain's work-list is `readable_at IS NULL` and it
/// keeps no cursor, so a row it declines to write is a row it will be offered again, and
/// again, forever. The ledger seed hit exactly this — its work-list asked "does this chapter
/// lack an event?" while its insert additionally refused future-dated chapters, so a work
/// holding a 2037-sentinel chapter was offered, inserted nothing, and was re-offered until a
/// test caught the loop (see `catalog::ledger::seedable_where`). The fix there and here is
/// the same: the work-list and the write must share one predicate. Every row this function
/// is handed gets a value, so every row leaves the work-list.
///
/// THE LADDER, which is §6.4's `released_at := readable_at when present := published_at
/// otherwise` made concrete:
///   1. upstream `readableAt` — the real answer, and the point of the whole backfill.
///   2. upstream `publishAt` — upstream answered but has no readable clock for this chapter.
///   3. the row's own `published_at` — upstream did not answer at all.
///   4. the row's own `created_at` — `NOT NULL`, so this rung always terminates.
///
/// Rungs 3 and 4 write exactly what every reader already computes: the read path spells the
/// clock `COALESCE(readable_at, published_at, created_at)`, so materialising that COALESCE
/// changes no behaviour whatsoever — it only takes the row off the work-list. (Rung 4 was
/// unreachable on the 2026-07-30 snapshot: 0 of 877,868 MangaDex chapters have a NULL
/// `published_at`. It is here so that fact does not have to stay true for the drain to end.)
///
/// Nothing is lost by taking a fallback. If upstream later publishes a real `readableAt` the
/// chapter's `updatedAt` moves with it, the firehose re-offers it, and `chapter_row_unchanged`
/// sees the difference and writes the truth.
fn resolve_readable_at(row: &PendingChapter, upstream: Option<&MdChapterAttrs>) -> ResolvedChapter {
    /// MangaDex sends `""` for some absent strings; an empty timestamp is not a timestamp,
    /// and an empty `externalUrl` would badge a perfectly readable chapter as off-site.
    fn present(v: Option<&String>) -> Option<String> {
        v.map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
    let answered = upstream.is_some();
    let readable_at = present(upstream.and_then(|a| a.readable_at.as_ref()))
        .or_else(|| present(upstream.and_then(|a| a.publish_at.as_ref())))
        .or_else(|| present(row.published_at.as_ref()))
        .unwrap_or_else(|| row.created_at.clone());
    ResolvedChapter {
        readable_at,
        external_url: present(upstream.and_then(|a| a.external_url.as_ref())),
        answered,
    }
}

/// One batch of the work-list.
///
/// SCOPED TO MANGADEX, and it has to be: `chapter` is the unified spine since Phase B, so
/// an unscoped `readable_at IS NULL` would hand Suwayomi rows (numeric `external_id`) to
/// `/chapter?ids[]=`, which cannot resolve them — and a row that can never be resolved is a
/// row that is offered forever.
///
/// The `EXISTS` form is not stylistic. Written as a JOIN, SQLite drives from `source_series`
/// (`SEARCH ss USING idx_source_series_suwayomi_nsfw (source_type=?)`) and probes the
/// partial index once per mangadex `source_series` — 113,876 probes per batch, and worse,
/// each batch drains the front of that iteration order so the next one must skip further:
/// quadratic across ~8,779 batches. With `EXISTS` the plan is
/// `SCAN c USING INDEX idx_chapter_needs_readable_at` + a primary-key probe per row, i.e.
/// linear in the rows STILL TO DO, which shrinks as the drain runs. That is precisely what
/// migration 0073's partial index was created for. (Both plans verified with
/// `EXPLAIN QUERY PLAN` against the migrated schema.)
async fn chapters_needing_readable_at(
    pool: &sqlx::SqlitePool,
    limit: i64,
) -> Result<Vec<PendingChapter>> {
    let rows: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT c.id, c.external_id, c.published_at, c.created_at \
           FROM chapter c \
          WHERE c.readable_at IS NULL \
            AND EXISTS (SELECT 1 FROM source_series ss \
                         WHERE ss.id = c.source_series_id AND ss.source_type = 'mangadex') \
          LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, external_id, published_at, created_at)| PendingChapter {
                id,
                external_id,
                published_at,
                created_at,
            },
        )
        .collect())
}

/// How many rows are still to do, for the completion log line. Same predicate as the
/// work-list — a `COUNT` that disagreed with the batch query would report "drained" over
/// rows the drain can still see, or spin over rows it cannot.
async fn readable_backfill_remaining(pool: &sqlx::SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM chapter c \
          WHERE c.readable_at IS NULL \
            AND EXISTS (SELECT 1 FROM source_series ss \
                         WHERE ss.id = c.source_series_id AND ss.source_type = 'mangadex')",
    )
    .fetch_one(pool)
    .await?)
}

/// Write one batch's resolutions, in one transaction.
///
/// Split out of [`backfill_chapter_external_urls`] with no client in its signature so the
/// termination and resumability properties are testable without touching MangaDex — this
/// drain is ~8,779 requests against a third party and a test must never be one of them.
async fn write_readable_backfill(
    pool: &sqlx::SqlitePool,
    pending: &[PendingChapter],
    upstream: &[MdChapter],
) -> Result<ReadableFill> {
    let by_id: HashMap<&str, &MdChapterAttrs> = upstream
        .iter()
        .map(|c| (c.id.as_str(), &c.attributes))
        .collect();
    let mut out = ReadableFill::default();
    let mut tx = pool.begin().await?;
    for row in pending {
        let resolved = resolve_readable_at(row, by_id.get(row.external_id.as_str()).copied());
        if resolved.answered {
            sqlx::query("UPDATE chapter SET readable_at = ?, external_url = ? WHERE id = ?")
                .bind(&resolved.readable_at)
                .bind(&resolved.external_url)
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
            if by_id[row.external_id.as_str()].readable_at.is_some() {
                out.from_upstream += 1;
            }
            if resolved.external_url.is_some() {
                out.external += 1;
            }
        } else {
            // `external_url` is left ALONE, not set to NULL. Upstream told us nothing about
            // this chapter, and the firehose's self-heal may already have written a real URL
            // onto the row; a blind NULL here would undo it and re-blank the reader.
            sqlx::query("UPDATE chapter SET readable_at = ? WHERE id = ?")
                .bind(&resolved.readable_at)
                .bind(&row.id)
                .execute(&mut *tx)
                .await?;
            out.unanswered += 1;
        }
        out.filled += 1;
    }
    tx.commit().await?;
    Ok(out)
}

/// One batch of the A1b backfill: fill `readable_at` / `external_url` on mirrored MangaDex
/// chapters that have neither. Returns what moved; `filled == 0` means the backfill is
/// complete. Call repeatedly — [`spawn_readable_backfill`] is what does.
///
/// RESUMABLE WITH NO CURSOR, ON PURPOSE, exactly like `catalog::spine`'s drains. The
/// work-list is the data itself (`readable_at IS NULL`), so an interrupted run resumes by
/// asking again, a row that becomes eligible tomorrow is picked up tomorrow, and there is no
/// second piece of state to go stale across a restart or to reset by hand when the
/// definition of "done" changes. The cost is one indexed query per batch against migration
/// 0073's partial index — which exists for this, and which shrinks to empty as the drain
/// runs. There is deliberately no completion marker: "done" is an empty work-list, and a
/// marker could latch over rows that were never filled.
pub async fn backfill_chapter_external_urls(
    pool: &sqlx::SqlitePool,
    client: &MangaDexClient,
) -> Result<ReadableFill> {
    let pending = chapters_needing_readable_at(pool, READABLE_BACKFILL_BATCH).await?;
    if pending.is_empty() {
        return Ok(ReadableFill::default());
    }
    let ids: Vec<String> = pending.iter().map(|p| p.external_id.clone()).collect();
    // The fetch is allowed to fail the whole batch: nothing has been written yet, the rows
    // stay in the work-list, and the next pass re-offers exactly them. That is the safe
    // direction — the unsafe one would be writing fallbacks over a transient 503.
    let page = client.get_chapters_by_ids(&ids).await?;
    // RAW length vs parsed count, the `ChapterPage` discipline: a record that came back and
    // failed to parse is not a record upstream lacks. It still gets the fallback (it must,
    // or it never leaves the work-list), but it must not be counted as though MangaDex had
    // no answer — that counter is the signal for a wrongly-filtered request.
    if page.raw_len != page.chapters.len() {
        tracing::warn!(
            asked = ids.len(),
            raw_len = page.raw_len,
            parsed = page.chapters.len(),
            "mangadex: readable_at backfill lost records to parse drops"
        );
    }
    write_readable_backfill(pool, &pending, &page.chapters).await
}

/// Run the A1b backfill to completion in the background, then idle cheaply.
///
/// Structured like `catalog::spine::spawn`: boot stagger, interruptible naps, sparse
/// logging, and an idle re-check rather than an exit — so a row that becomes eligible after
/// the drain finishes is still picked up, without a restart.
pub fn spawn_readable_backfill(
    pool: sqlx::SqlitePool,
    client: Arc<MangaDexClient>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        /// Sleep unless shutdown fires first. `true` => stop.
        ///
        /// A dropped sender makes `changed()` return `Err` instantly and forever; treating
        /// that as "keep going" would collapse every gap in this loop to a no-op and turn
        /// the throttle off — i.e. it would fire ~8,779 MangaDex requests as fast as the
        /// token bucket allows, during shutdown. Both `spawn_backfill_if_needed` and
        /// `catalog::spine::spawn` document having made exactly that mistake.
        async fn nap(d: Duration, shutdown: &mut tokio::sync::watch::Receiver<bool>) -> bool {
            tokio::select! {
                _ = tokio::time::sleep(d) => false,
                r = shutdown.changed() => r.is_err() || *shutdown.borrow(),
            }
        }
        if nap(READABLE_BACKFILL_BOOT_DELAY, &mut shutdown).await {
            return;
        }
        let mut total = ReadableFill::default();
        let mut batches = 0u64;
        let mut announced = false;
        loop {
            if *shutdown.borrow() {
                tracing::info!(
                    batches,
                    filled = total.filled,
                    "readable backfill: shutting down mid-drain — the work-list resumes it"
                );
                return;
            }
            match backfill_chapter_external_urls(&pool, &client).await {
                Ok(fill) if fill.filled == 0 => {
                    if !announced {
                        match readable_backfill_remaining(&pool).await {
                            // The success line, and the one that says A1b is done: the 46
                            // sentinel works can now reach /updates and the F1 cohort is
                            // identifiable by `external_url IS NOT NULL`.
                            Ok(0) => tracing::info!(
                                batches,
                                filled = total.filled,
                                from_upstream = total.from_upstream,
                                external = total.external,
                                unanswered = total.unanswered,
                                "readable backfill: drained — every mirrored MangaDex \
                                 chapter has a release clock"
                            ),
                            // WARN, not info: "nothing to do but rows remain" means rows are
                            // unreachable rather than pending, which is a bug in the
                            // work-list, not a state to wait out.
                            Ok(remaining) => tracing::warn!(
                                remaining,
                                batches,
                                "readable backfill: reported idle but rows remain — \
                                 they are unreachable, not merely pending"
                            ),
                            Err(e) => {
                                tracing::warn!(error = %e, "readable backfill: remaining check failed")
                            }
                        }
                        announced = true;
                    }
                    if nap(READABLE_BACKFILL_IDLE_RECHECK, &mut shutdown).await {
                        return;
                    }
                }
                Ok(fill) => {
                    total.filled += fill.filled;
                    total.from_upstream += fill.from_upstream;
                    total.external += fill.external;
                    total.unanswered += fill.unanswered;
                    batches += 1;
                    announced = false;
                    // Sparse by construction: ~8,779 batches at 2s apart is ~5 hours, so a
                    // line every 500 batches is roughly one per 17 minutes.
                    if batches.is_multiple_of(READABLE_BACKFILL_PROGRESS_EVERY) {
                        tracing::info!(
                            batches,
                            filled = total.filled,
                            from_upstream = total.from_upstream,
                            external = total.external,
                            unanswered = total.unanswered,
                            "readable backfill: progress"
                        );
                    }
                    if nap(READABLE_BACKFILL_GAP, &mut shutdown).await {
                        return;
                    }
                }
                Err(e) => {
                    // Nothing was written (the fetch is the only thing that can fail before
                    // the transaction), so the batch is simply re-offered. Back off to the
                    // idle interval rather than the batch gap: an error here is usually
                    // upstream being unhappy, and retrying it every 2s is how a rate-limit
                    // becomes a ban.
                    tracing::warn!(error = %e, batches, "readable backfill: batch failed — retrying later");
                    if nap(READABLE_BACKFILL_IDLE_RECHECK, &mut shutdown).await {
                        return;
                    }
                }
            }
        }
    });
}

/// Whether the `chapter` row for `(source_series_id, ch.external_id)` already exists AND
/// carries every mutable column exactly as `ch` would set it — i.e. `upsert_chapter`
/// would spend a write transaction to change nothing.
///
/// WHY THE FIREHOSE ASKS BEFORE WRITING (the same "skip ids we already carry" shape as
/// `backfill_missing_catalogue`, for the same reason): the forced re-seed of migration
/// 0067 re-walks all ~876k English chapters, of which the ~805,307 already in the mirror
/// are byte-identical. Handing every one of them to `upsert_chapter` is ~876k autocommit
/// transactions against SQLite's single writer, ~92% of them no-op UPDATEs, each one
/// contending with the scanner, the cover crawler and Litestream's checkpointer — for
/// hours. The read is nearly free by comparison: `UNIQUE (source_series_id, external_id)`
/// backs the lookup with `sqlite_autoindex_chapter_2` (verified against the live schema),
/// so this trades a write transaction for one index seek.
///
/// A FAILED READ RETURNS `false`, i.e. "go write it". This is an optimisation, never a
/// filter: "I could not tell" must degrade to mirroring the chapter (where a real failure
/// is then counted into `SweepOutcome::failed`), not to silently skipping it.
async fn chapter_row_unchanged(
    pool: &sqlx::SqlitePool,
    source_series_id: &str,
    ch: &catalog::ChapterInput,
) -> bool {
    type MutableCols = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let row: Option<MutableCols> = match sqlx::query_as(
        "SELECT number, volume, lang, title, published_at, readable_at, external_url \
         FROM chapter WHERE source_series_id = ? AND external_id = ?",
    )
    .bind(source_series_id)
    .bind(&ch.external_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(e) => {
            tracing::debug!(error = %e, "mangadex: chapter presence check failed; writing anyway");
            return false;
        }
    };
    match row {
        // Exactly the columns `upsert_chapter`'s DO UPDATE SET touches, compared as
        // `Option<String>` so NULL == absent: if this list ever grows there, it must grow
        // here too, or the re-seed will skip a genuine change.
        Some((number, volume, lang, title, published_at, readable_at, external_url)) => {
            number == ch.number
                && volume == ch.volume
                && lang == ch.lang
                && title == ch.title
                && published_at == ch.published_at
                // Added with migration 0073. Comparing them here is what makes the
                // firehose SELF-HEAL: every existing row has both NULL, so the first pass
                // that re-offers a chapter now sees a difference and writes the real
                // `readableAt`/`externalUrl` instead of skipping it as unchanged. (The
                // firehose only re-offers UPDATED chapters, so the long tail still needs
                // `backfill_chapter_external_urls`.)
                && readable_at == ch.readable_at
                && external_url == ch.external_url
        }
        None => false,
    }
}

/// Global `/chapter` firehose → mirrored `chapter` rows. Each chapter is attached to
/// the `mangadex` source_series of its `manga` relationship; chapters whose work
/// hasn't been catalogued yet are skipped (a later catalogue sweep + re-run picks
/// them up). Same `createdAt` windowing as the catalogue sweep.
///
/// Pagination is driven by the RAW page length exactly as the catalogue sweep's is —
/// see `ChapterPage` for the truncation bug that cost.
pub async fn sync_chapters(
    pool: &sqlx::SqlitePool,
    client: &MangaDexClient,
    window: SyncWindow,
    initial_since: Option<String>,
    job: &str,
) -> Result<SweepOutcome> {
    let mut since = initial_since;
    let mut out = SweepOutcome::default();
    loop {
        let mut offset = 0i64;
        // Series this window's pages have written a chapter for, drained at the end of
        // each page by the incremental feed writer below.
        let mut touched: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut last_created: Option<String> = None;
        let mut done = false;
        let mut empty_retries = 0u32;
        loop {
            let ChapterPage {
                chapters,
                total,
                raw_len,
                dropped,
            } = client
                .list_chapters(window, since.as_deref(), offset)
                .await?;
            out.dropped += dropped;
            // RAW length + `total`, never `chapters.len()`: a page trimmed only by
            // unparseable chapters must not read as the end of the window.
            match classify_page(raw_len, offset, total) {
                PageStep::EndWindow => {
                    done = true;
                    break;
                }
                PageStep::RetryEmpty => {
                    if empty_retries < EMPTY_PAGE_RETRIES {
                        empty_retries += 1;
                        tracing::warn!(
                            offset,
                            total,
                            empty_retries,
                            "mangadex: empty chapter page before reaching total — retrying"
                        );
                        continue;
                    }
                    tracing::error!(
                        offset,
                        total,
                        "mangadex: repeated empty chapter page before total — ending window"
                    );
                    done = true;
                    break;
                }
                PageStep::Process => {}
            }
            empty_retries = 0;
            for c in &chapters {
                // THE CURSOR IS ADVANCED AT THE END OF THIS BODY, NOT HERE (F9).
                //
                // It used to be advanced right here, before the skips below — and the
                // window is forward-only `updatedAtSince`, so a chapter skipped because its
                // work is not catalogued yet had the cursor step PAST it and was never
                // offered again. The chapter was silently and permanently absent from the
                // mirror, with no counter and no log line to show for it: the same shape as
                // the catalogue's 4,493-record gap.
                //
                // Deliberately not "advance for skipped rows too, but count them": the
                // point is that the cursor must never move past a record this pass did not
                // land. A record we skip stays inside the next window, and the catalogue
                // sweep that runs first will have catalogued its work by then.
                let window_ts = chapter_window_ts(c, window);

                // English-only mirror: the firehose is already filtered to `en`, but
                // guard defensively so a stray non-English row is never stored.
                if c.attributes.translated_language.as_deref() != Some("en") {
                    // A non-English row is not a loss — we never wanted it — so it may
                    // advance the cursor.
                    if let Some(ts) = window_ts {
                        last_created = Some(ts);
                    }
                    continue;
                }
                let Some(manga_id) = chapter_manga_id(c) else {
                    // No manga relationship at all: unusable, and re-offering it would
                    // stall the window forever. Count it as dropped and move on.
                    out.dropped += 1;
                    if let Some(ts) = window_ts {
                        last_created = Some(ts);
                    }
                    continue;
                };
                let ssid = match catalog::find_source_series_id(
                    pool, "mangadex", "mangadex", &manga_id,
                )
                .await
                {
                    Ok(Some(id)) => id,
                    // Work not catalogued yet — DO NOT advance the cursor past it. This is
                    // the F9 loss. `skipped` holds the window open so the next pass, after
                    // the catalogue sweep has caught up, sees this chapter again.
                    Ok(None) => {
                        out.skipped += 1;
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "mangadex: chapter source_series lookup failed");
                        out.failed += 1;
                        continue;
                    }
                };
                let ch = catalog::ChapterInput {
                    external_id: c.id.clone(),
                    number: c.attributes.chapter.clone(),
                    volume: c.attributes.volume.clone(),
                    lang: c.attributes.translated_language.clone(),
                    title: c.attributes.title.clone(),
                    published_at: c.attributes.publish_at.clone(),
                    readable_at: c.attributes.readable_at.clone(),
                    external_url: c.attributes.external_url.clone(),
                    // MangaDex models translation groups as a relationship, not a string
                    // on the chapter — the mirror has never carried one and the spine
                    // leaves it NULL for this half.
                    scanlator: None,
                };
                // Already mirrored and identical → no write. See `chapter_row_unchanged`
                // for the cost this avoids on the 0067 re-seed.
                if chapter_row_unchanged(pool, &ssid, &ch).await {
                    out.unchanged += 1;
                    // Already in the mirror, so nothing is lost by moving past it.
                    if let Some(ts) = window_ts {
                        last_created = Some(ts);
                    }
                    continue;
                }
                match catalog::upsert_chapter(pool, &ssid, &ch).await {
                    Ok(_) => {
                        out.upserted += 1;
                        // Landed — the cursor may now move past it (F9).
                        if let Some(ts) = window_ts {
                            last_created = Some(ts);
                        }
                        // Phase C2: this page's writes reach /updates at the end of the
                        // page, not at the next 6-hourly rebuild.
                        touched.insert(ssid.clone());
                    }
                    // COUNTED, not just logged. A warn-and-forget here is how a seed could
                    // lose chapters to SQLITE_BUSY and still flip `seed_done`, after which
                    // the forward-only `updatedAtSince` window never re-offers them —
                    // the same failure mode that made the catalogue's 4,493-record gap
                    // permanent. `run_chapter_cycle` withholds `seed_done` while this is
                    // non-zero, so the seed re-runs instead.
                    Err(e) => {
                        out.failed += 1;
                        tracing::warn!(error = %e, "mangadex: chapter upsert failed");
                    }
                }
            }
            out.feed += flush_touched_feed_rows(pool, &mut touched).await;
            // RAW length, never the parsed count — see `next_offset` / `ChapterPage`.
            offset = next_offset(offset, raw_len);
            // A short page means the API ran out of records for this window. Short of
            // the RAW limit, not of the parsed count: that distinction is the whole bug.
            // Deliberately NOT also `offset >= total` (which is how `sync_catalogue`
            // terminates): adding it here would end the window earlier than the old code
            // ever did if `total` under-reported, and this change is meant to remove a
            // truncation, not trade it for a new one. An exhausted window still ends
            // promptly — the next fetch is empty, which `classify_page` calls `EndWindow`.
            if (raw_len as i64) < PAGE_LIMIT {
                done = true;
                break;
            }
            if offset >= WINDOW_OFFSET_CAP {
                break;
            }
        }
        if done {
            break;
        }
        let next_since = last_created.as_deref().and_then(to_since);
        if next_since.is_none() || next_since == since {
            // As in the catalogue sweep: >9,900 chapters sharing one boundary second
            // stalls the window (more likely on the high-volume /chapter firehose).
            // Step to the next second so only that tied second's tail past offset
            // 9,900 is lost and the sweep continues, instead of stalling here (M7).
            match last_created.as_deref().and_then(to_since_next_second) {
                Some(stepped) if Some(&stepped) != since.as_ref() => {
                    tracing::warn!(
                        since = since.as_deref().unwrap_or("<none>"),
                        resume = %stepped,
                        "mangadex: >9900 records share a chapter boundary second — \
                         skipping the tail past offset 9900 and resuming at the next second"
                    );
                    since = Some(stepped);
                }
                _ => {
                    tracing::error!(
                        since = since.as_deref().unwrap_or("<none>"),
                        "mangadex: chapter window stuck on a boundary second and the \
                         cursor cannot be stepped — records past offset 9900 are dropped"
                    );
                    break;
                }
            }
        } else {
            since = next_since;
        }
        // Checkpoint the chapter seed's progress so an abort resumes here (M6).
        if window == SyncWindow::Created {
            if let Some(ref s) = since {
                if let Err(e) = catalog::set_seed_progress(pool, job, s).await {
                    tracing::warn!(error = %e, "mangadex: failed to checkpoint chapter seed");
                }
            }
        }
    }
    if out.dropped > 0 || out.failed > 0 {
        tracing::error!(
            stored = out.upserted,
            dropped = out.dropped,
            failed = out.failed,
            "mangadex: chapter sweep LOST records — they were fetched and never mirrored \
             (dropped = unparseable, failed = the write errored)"
        );
    }
    tracing::info!(
        stored = out.upserted,
        unchanged = out.unchanged,
        dropped = out.dropped,
        failed = out.failed,
        // How many works this sweep actually pushed to /updates. It reads as noise next to
        // the loss counters and it is not one — it is the only way to tell "the incremental
        // feed writer ran" from "the incremental feed writer is inert", which is precisely
        // the distinction the un-extracted version hid for a whole phase (§8h).
        feed = out.feed,
        "mangadex: chapter sweep complete"
    );
    Ok(out)
}

/// Single-flight lock over `run_one_cycle`: only one catalogue sync cycle runs at a
/// time across the whole process, whether triggered by the recurring loop or an admin
/// `resyncCatalogue`. This keeps a manual re-seed (which can run for hours) from racing
/// a recurring tick — overlapping sweeps would double-fetch and could interleave cursor
/// writes into an inconsistent seed state.
static CATALOGUE_SYNC_RUNNING: AtomicBool = AtomicBool::new(false);

/// Releases the single-flight lock on drop (so a panic mid-cycle can't wedge it — the
/// binary builds with `panic = "unwind"`, so Drop still runs).
struct SyncGuard;
impl Drop for SyncGuard {
    fn drop(&mut self) {
        CATALOGUE_SYNC_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Guarded entry point for the recurring loop: run one cycle unless one is already in
/// flight, in which case skip this tick.
async fn sync_cycle(pool: &sqlx::SqlitePool, client: &MangaDexClient, cover_phash: bool) {
    if CATALOGUE_SYNC_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        tracing::info!("mangadex: a catalogue sync cycle is already running; skipping this tick");
        return;
    }
    let _guard = SyncGuard;
    run_one_cycle(pool, client, cover_phash).await;
}

/// Admin-triggered full re-seed: clear the catalogue + chapter sync cursors so the next
/// cycle does a fresh `createdAt` seed from scratch, then run one cycle in the
/// background. Returns `false` (doing nothing) if a sync cycle is already running so the
/// caller can surface "busy, retry" — the single-flight lock is acquired up front and
/// held for the whole re-seed so a recurring tick can't interleave its own cursor write
/// between the reset and the seed.
pub fn spawn_resync(
    pool: sqlx::SqlitePool,
    client: Arc<MangaDexClient>,
    cover_phash: bool,
) -> bool {
    if CATALOGUE_SYNC_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }
    tokio::spawn(async move {
        let _guard = SyncGuard; // releases the lock on completion or panic
        if let Err(e) = catalog::reset_sync_state(&pool, "catalogue").await {
            tracing::error!(error = %e, "resync: failed to reset catalogue sync state; aborting");
            return;
        }
        if let Err(e) = catalog::reset_sync_state(&pool, "chapters").await {
            tracing::error!(error = %e, "resync: failed to reset chapter sync state; aborting");
            return;
        }
        tracing::info!("resync: sync state cleared — starting fresh catalogue seed");
        run_one_cycle(&pool, &client, cover_phash).await;
        tracing::info!("resync: fresh seed cycle complete");
    });
    true
}

/// Run one catalogue + chapter sync cycle. A job with no stored cursor does a full
/// `createdAt` seed; a job with a cursor does an incremental `updatedAtSince` refresh
/// (CATALOGUE.md §5). The cursor advances to the cycle's start time only on success,
/// so a failed or interrupted cycle safely retries the same window next tick. Catalogue
/// runs before chapters (chapters attach to already-catalogued works). Call via
/// `sync_cycle` (recurring) or `spawn_resync` (admin) — both hold the single-flight lock.
async fn run_one_cycle(pool: &sqlx::SqlitePool, client: &MangaDexClient, cover_phash: bool) {
    // Wall-clock at cycle start, in MangaDex `since` form. Anything updated during the
    // cycle is >= this, so it's caught by the next cycle rather than missed.
    let run_start = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();

    // --- Catalogue -----------------------------------------------------------
    // seed_done == false → (re)run/resume the full createdAt seed from the
    // provisional cursor; true → incremental updatedAtSince refresh.
    let catalogue_state = match catalog::get_sync_state(pool, "catalogue").await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "mangadex: catalogue state read failed; skipping cycle");
            return;
        }
    };
    let catalogue_seeded = catalogue_state
        .as_ref()
        .map(|s| s.seed_done)
        .unwrap_or(false);
    let (window, since) = match &catalogue_state {
        Some(s) if s.seed_done => (SyncWindow::Updated, Some(s.cursor.clone())),
        Some(s) => (SyncWindow::Created, Some(s.cursor.clone())), // resume seed
        None => (SyncWindow::Created, None),                      // fresh seed
    };
    match sync_catalogue(pool, client, window, since, cover_phash, "catalogue").await {
        Ok(o) => {
            tracing::info!(
                upserted = o.upserted,
                dropped = o.dropped,
                incremental = catalogue_seeded,
                "mangadex: catalogue cycle done"
            );
            // Completing a fresh/resumed seed flips seed_done and sets the
            // incremental cursor; an already-seeded run just advances the cursor.
            let res = if catalogue_seeded {
                catalog::set_sync_cursor(pool, "catalogue", &run_start).await
            } else if !catalogue_seed_may_latch(&o) {
                // A seed that FETCHED records and did not land them must not latch
                // `seed_done` — whether they failed to PARSE (`dropped`) or failed to WRITE
                // (`failed`; F9): doing so switches the sweep to the forward-only
                // `updatedAtSince` window, which never revisits an old `createdAt`, so
                // those records become unreachable without a full manual re-seed. That is
                // precisely how the 4,493-record gap became permanent.
                //
                // BE CLEAR ABOUT WHAT THIS DOES AND DOESN'T BUY. The per-page cursor is
                // kept (`set_seed_progress` already wrote it, advanced to the last PARSED
                // record — which is normally past the dropped ones), so the next cycle
                // resumes AHEAD of the drops and, finding none of its own, marks the seed
                // done. This is therefore a one-cycle delay plus a loud, attributable
                // ERROR — not a recovery mechanism. Recovering the dropped records is the
                // gap backfill's job (`backfill_missing_catalogue`), which rewinds its
                // cursor and re-walks from 2018 on any dirty pass. The alternative here —
                // rewinding the seed cursor — would re-walk 113k records every cycle
                // forever on a single permanently-malformed upstream record.
                tracing::error!(
                    dropped = o.dropped,
                    failed = o.failed,
                    "mangadex: catalogue seed lost records — withholding seed_done so \
                     the seed keeps resuming instead of switching to the forward-only \
                     incremental window"
                );
                Ok(())
            } else {
                catalog::mark_seed_done(pool, "catalogue", &run_start).await
            };
            if let Err(e) = res {
                tracing::warn!(error = %e, "mangadex: failed to persist catalogue cursor");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "mangadex: catalogue cycle failed (cursor unchanged)");
        }
    }

    // The chapter half USED to run here, on the catalogue's 6-hourly cadence. It has its
    // own ticker now (Phase D) — see `run_recurring`.
    let _ = run_start;
}

/// The chapter half of a cycle, on its own schedule (Phase D). Takes the same single-flight
/// lock as the catalogue sweep: they write the same tables, and the chapter sweep's
/// uncatalogued-work skip reads state the catalogue sweep is mutating.
pub async fn chapter_cycle(pool: &sqlx::SqlitePool, client: &MangaDexClient) {
    if CATALOGUE_SYNC_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        tracing::debug!("mangadex: catalogue sync busy; skipping this chapter tick");
        return;
    }
    let _guard = SyncGuard;
    let run_start = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    run_chapter_cycle(pool, client, &run_start).await;
}

/// Whether a completed chapter SEED may latch `seed_done` — i.e. whether it landed
/// everything it fetched. Both counters mean "fetched and not in the mirror", differing
/// only in where the record died (`dropped` = it never parsed, `failed` = the write
/// errored), and latching on either one switches the sweep to the forward-only
/// `updatedAtSince` window, which never revisits an old `createdAt`: the loss becomes
/// permanent until a human forces another re-seed. One predicate rather than two branches
/// so a future third loss class can only be added in one place.
fn chapter_seed_may_latch(o: &SweepOutcome) -> bool {
    // `skipped` counts too: a chapter whose work is not catalogued yet has not been
    // mirrored, and latching `seed_done` with any of them outstanding switches the sweep to
    // the forward-only window while records it never landed are still behind the cursor.
    o.dropped == 0 && o.failed == 0 && o.skipped == 0
}

/// The same predicate for the CATALOGUE seed, and it is the same predicate on purpose.
///
/// The catalogue half used to latch on `dropped == 0` alone, so a work whose upsert
/// *errored* — as opposed to failing to parse — did not hold the seed open. Both counters
/// mean "fetched and not in the spine"; they differ only in where the record died. Letting
/// one of them latch `seed_done` switches the sweep to the forward-only `updatedAtSince`
/// window, which never revisits an old `createdAt`, and the loss becomes permanent until a
/// human forces a re-seed. That asymmetry was half of F9; the other half was that the
/// upsert's error arm never incremented `failed` at all, so the counter it now consults was
/// always zero.
fn catalogue_seed_may_latch(o: &SweepOutcome) -> bool {
    o.dropped == 0 && o.failed == 0
}

/// The chapter half of a sync cycle: seed or incrementally refresh the `/chapter`
/// firehose mirror. Split out of `run_one_cycle` because the two halves have very
/// different costs — the incremental chapter sweep is ~75 chapters a cycle while a
/// catalogue sweep is thousands of records — so this half can be driven on a much
/// tighter schedule than `CATALOGUE_SYNC_INTERVAL_SECS` (6h, which is why a brand-new
/// chapter can surface already labelled "5h ago"). `run_start` is the wall-clock at the
/// caller's cycle start in MangaDex `since` form: anything updated during the cycle is
/// >= it, so it's caught next cycle rather than missed.
async fn run_chapter_cycle(pool: &sqlx::SqlitePool, client: &MangaDexClient, run_start: &str) {
    // Gate the chapter seed on the catalogue seed having completed (M3): until
    // every work is catalogued, a chapter whose work is missing is skipped, and
    // the chapters cursor would advance past it (updatedAtSince never revisits an
    // old createdAt), permanently losing it. Re-read state so a seed that just
    // completed above is observed this same cycle.
    let catalogue_done = catalog::get_sync_state(pool, "catalogue")
        .await
        .ok()
        .flatten()
        .map(|s| s.seed_done)
        .unwrap_or(false);
    if !catalogue_done {
        tracing::info!("mangadex: skipping chapter sync until the catalogue seed completes");
        return;
    }

    let chapter_state = match catalog::get_sync_state(pool, "chapters").await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "mangadex: chapter state read failed; skipping");
            return;
        }
    };
    let chapters_seeded = chapter_state.as_ref().map(|s| s.seed_done).unwrap_or(false);
    let (window, since) = match &chapter_state {
        Some(s) if s.seed_done => (SyncWindow::Updated, Some(s.cursor.clone())),
        Some(s) => (SyncWindow::Created, Some(s.cursor.clone())),
        None => (SyncWindow::Created, None),
    };
    match sync_chapters(pool, client, window, since, "chapters").await {
        Ok(o) => {
            tracing::info!(
                stored = o.upserted,
                unchanged = o.unchanged,
                dropped = o.dropped,
                failed = o.failed,
                incremental = chapters_seeded,
                "mangadex: chapter cycle done"
            );
            let res = if chapters_seeded {
                catalog::set_sync_cursor(pool, "chapters", run_start).await
            } else if !chapter_seed_may_latch(&o) {
                // Same rule as the catalogue seed: a seed that fetched records and did not
                // land them must not flip `seed_done`, because that switches the sweep to
                // the forward-only `updatedAtSince` window which never revisits an old
                // `createdAt`. The per-window `set_seed_progress` checkpoint is kept, so
                // the next cycle resumes rather than restarting.
                tracing::error!(
                    dropped = o.dropped,
                    failed = o.failed,
                    "mangadex: chapter seed lost records (dropped = unparseable, failed = \
                     the write errored) — withholding seed_done so the seed keeps resuming \
                     instead of switching to the incremental window"
                );
                Ok(())
            } else {
                catalog::mark_seed_done(pool, "chapters", run_start).await
            };
            if let Err(e) = res {
                tracing::warn!(error = %e, "mangadex: failed to persist chapter cursor");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "mangadex: chapter cycle failed (cursor unchanged)");
        }
    }
}

/// Spawn the recurring catalogue + chapter sync. The first tick fires immediately
/// (seeding on startup), then every `interval_secs`. Exits cleanly when `shutdown`
/// fires. Mirrors the `scanner::spawn` background-task pattern.
pub fn spawn_recurring(
    pool: sqlx::SqlitePool,
    client: Arc<MangaDexClient>,
    cover_phash: bool,
    interval_secs: u64,
    chapter_interval_secs: u64,
    reconcile_interval_secs: u64,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Supervisor: run the loop in a child task and, if it panics, restart it after
    // a short backoff so a single panic doesn't silently kill catalogue sync for
    // the process lifetime. A clean (shutdown) exit ends supervision.
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_recurring(
                pool.clone(),
                client.clone(),
                cover_phash,
                interval_secs,
                chapter_interval_secs,
                reconcile_interval_secs,
                shutdown.clone(),
            ));
            match handle.await {
                Ok(()) => break,
                Err(e) if e.is_panic() => {
                    if *shutdown.borrow() {
                        break;
                    }
                    tracing::error!("mangadex: catalogue sync loop panicked; restarting in 30s");
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    if *shutdown.borrow() {
                        break;
                    }
                }
                Err(_) => break, // cancelled
            }
        }
    });
}

async fn run_recurring(
    pool: sqlx::SqlitePool,
    client: Arc<MangaDexClient>,
    cover_phash: bool,
    interval_secs: u64,
    chapter_interval_secs: u64,
    reconcile_interval_secs: u64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Start 60s behind the scanner so the boot burst is spread across the five
    // background loops rather than landing in one instant — see scanner::run_loop.
    // THREE CADENCES, NOT ONE (Phases C3 + D). They used to be the same tick, which meant
    // the cheapest and most time-critical of the three — the chapter firehose — ran at the
    // pace of the most expensive.
    //
    //   catalogue   6 h  — thousands of records; genuinely slow-changing.
    //   chapters   15 m  — ~75 chapters an incremental pass. This is the freshness knob.
    //   reconcile  24 h  — the demoted wholesale chain, now a drift REPORT (C3).
    //
    // Staggered starts so the boot burst is spread rather than landing in one instant, and
    // `Delay` on all three so a long pass postpones the next tick instead of queueing a
    // backlog. They share `CATALOGUE_SYNC_RUNNING`, so two sweeps can never interleave.
    let mut catalogue = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(interval_secs),
    );
    catalogue.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut chapters = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(150),
        std::time::Duration::from_secs(chapter_interval_secs),
    );
    chapters.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut reconcile = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(300),
        std::time::Duration::from_secs(reconcile_interval_secs),
    );
    reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(
        interval_secs,
        chapter_interval_secs,
        reconcile_interval_secs,
        cover_phash,
        "mangadex: recurring sync started"
    );
    loop {
        tokio::select! {
            _ = catalogue.tick() => {
                sync_cycle(&pool, &client, cover_phash).await;
            }
            _ = chapters.tick() => {
                // The freshness path. What reaches /updates from here is the Phase C2
                // incremental writer inside `sync_chapters` — one feed upsert per touched
                // series, per page — NOT the wholesale chain, which is what makes a
                // 15-minute cadence affordable at all.
                chapter_cycle(&pool, &client).await;
            }
            _ = reconcile.tick() => {
                // Phase C3: rebuild everything and report what the incremental writers
                // missed. `drifted == 0` is the healthy answer, and it is the evidence that
                // the incremental path is actually correct rather than merely present.
                match crate::catalog::ledger::reconcile_feed(&pool).await {
                    Ok(r) if r.drifted == 0 => tracing::info!(
                        rows = r.rows_after,
                        duration_ms = r.duration_ms,
                        "feed reconciler: no drift — incremental writers are keeping up"
                    ),
                    Ok(r) => tracing::warn!(
                        rows_before = r.rows_before,
                        rows_after = r.rows_after,
                        drifted = r.drifted,
                        duration_ms = r.duration_ms,
                        sample = %r.sample.join(" | "),
                        "feed reconciler: DRIFT — the incremental writers missed rows"
                    ),
                    Err(e) => tracing::warn!(error = %e, "feed reconciler: failed"),
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("mangadex: recurring sync stopping");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A migrated in-memory DB, as in the `catalog` tests.
    async fn pool() -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    // --- the MangaDex half's incremental feed writer ---------------------------------
    //
    // §8h: this path had NO test at all. `scanner::incremental_write_converges_with_the_
    // periodic_rebuild` drives `touch_feed_series_update`, i.e. the SCANNER half only, so
    // the mirror half's writer shipped never having created a feed row and nothing failed.

    /// EVERY column of a work's merged-feed row plus the sort key's STORAGE CLASS, as one
    /// comparable string — the mirror-half twin of `scanner::tests::feed_digest`, and every
    /// column for the same reason: a row whose contents depend on WHICH writer touched it
    /// last is a row that appears, disappears or re-sorts depending on firehose timing.
    /// `~` stands in for NULL so a NULL cannot swallow the concatenation.
    async fn feed_digest(pool: &sqlx::SqlitePool, work_id: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT work_id || '|' || reader_id || '|' || title \
                 || '|' || COALESCE(cover_url, '~') || '|' || COALESCE(suwayomi_thumbnail, '~') \
                 || '|' || COALESCE(comic_type, '~') || '|' || COALESCE(latest_chapter, '~') \
                 || '|' || COALESCE(latest_chapter_title, '~') \
                 || '|' || COALESCE(chapter_count, '~') \
                 || '|' || released_at || '|' || typeof(released_at) \
                 || '|' || COALESCE(detected_at, '~') || '|' || is_nsfw \
                 || '|' || COALESCE(status, '~') || '|' || content_rating \
                 || '|' || en_chapter_count \
             FROM feed_series_updates WHERE work_id = ?",
        )
        .bind(work_id)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    /// The same for `feed_updates` (migration 0051), which `canonicalUpdates` reads directly
    /// — the OTHER surface this writer has to keep honest.
    async fn canonical_digest(pool: &sqlx::SqlitePool, work_id: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT work_id || '|' || COALESCE(mangadex_id, '~') || '|' || title \
                 || '|' || is_nsfw || '|' || COALESCE(cover_url, '~') \
                 || '|' || COALESCE(latest_chapter, '~') \
                 || '|' || COALESCE(latest_chapter_title, '~') || '|' || latest_at \
             FROM feed_updates WHERE work_id = ?",
        )
        .bind(work_id)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    /// One MangaDex-anchored work, its `source_series` id, and no chapters yet.
    async fn mirror_work(pool: &sqlx::SqlitePool, uuid: &str, title: &str) -> (String, String) {
        let work = catalog::upsert_work_from_mangadex(
            pool,
            uuid,
            &catalog::WorkInput {
                primary_title: Some(title.into()),
                original_language: Some("ja".into()),
                status: Some("ONGOING".into()),
                content_rating: Some("safe".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let ssid = catalog::find_source_series_id(pool, "mangadex", "mangadex", uuid)
            .await
            .unwrap()
            .unwrap();
        (work, ssid)
    }

    async fn mirror_chapter(pool: &sqlx::SqlitePool, ssid: &str, id: &str, number: &str, at: &str) {
        catalog::upsert_chapter(
            pool,
            ssid,
            &catalog::ChapterInput {
                external_id: id.into(),
                number: Some(number.into()),
                lang: Some("en".into()),
                title: Some(format!("Title of {number}")),
                readable_at: Some(at.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    /// Drive the extracted writer exactly as `sync_chapters` does: a set of the
    /// `source_series` this page wrote to, drained.
    async fn flush(pool: &sqlx::SqlitePool, ssid: &str) -> u64 {
        let mut touched = std::collections::HashSet::new();
        touched.insert(ssid.to_string());
        let n = flush_touched_feed_rows(pool, &mut touched).await;
        assert!(touched.is_empty(), "the set must be drained, not re-walked");
        n
    }

    /// THE BUG §8h FOUND, stated as a test: the incremental path must CREATE the feed row.
    ///
    /// The old inline block ran only `project_feed_from_ledger_for_work`, an `UPDATE … FROM`.
    /// An UPDATE matches nothing when there is no row, so a work whose first English chapter
    /// had just been mirrored stayed invisible on `/updates` until the next 6-hourly rebuild
    /// — precisely the F5 staleness the block was written to remove. It reported success
    /// (`0 rows changed`) the whole time.
    #[tokio::test]
    async fn the_mirror_half_creates_a_feed_row_it_does_not_only_update_one() {
        let pool = pool().await;
        let (work, ssid) = mirror_work(&pool, "md-new", "Brand New Series").await;
        mirror_chapter(&pool, &ssid, "c1", "1", "2026-06-01T00:00:00Z").await;

        assert!(
            feed_digest(&pool, &work).await.is_none(),
            "no row before the write, or this asserts nothing"
        );
        assert_eq!(flush(&pool, &ssid).await, 1, "the work must reach the feed");

        let (released_at, typ, label, count): (i64, String, Option<String>, i64) = sqlx::query_as(
            "SELECT released_at, typeof(released_at), latest_chapter, en_chapter_count \
             FROM feed_series_updates WHERE work_id = ?",
        )
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
        // The sort key must land as INTEGER epoch-millis. A TEXT key sorts every ISO '2…'
        // row above every millis '1…' row under BINARY collation — migration 0064.
        assert_eq!(typ, "integer", "released_at must be an INTEGER, not TEXT");
        assert_eq!(
            released_at,
            chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
                .unwrap()
                .timestamp_millis()
        );
        // The chapter NUMBER, never the COUNT (F4).
        assert_eq!(label.as_deref(), Some("1"));
        assert_eq!(count, 1);
        // A brand-new row must not land type-less: `comic_type IS NULL` is invisible to the
        // reader's format tabs and to Browse's format chips.
        let typed: Option<String> =
            sqlx::query_scalar("SELECT comic_type FROM feed_series_updates WHERE work_id = ?")
                .bind(&work)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(typed.as_deref(), Some("MANGA"));
    }

    /// THE convergence proof for the mirror half: drive the incremental writer, then hand the
    /// tables to the REAL periodic rebuild and demand a byte-identical row — the same
    /// property `scanner::incremental_write_converges_with_the_periodic_rebuild` pins for the
    /// other half, and the one that makes two writers of one materialized table safe.
    ///
    /// Three interleavings, because they exercise different clauses:
    ///   1. a mirror-only work — the INSERT branch, which is the branch that did not exist;
    ///   2. a second chapter arriving later — the CONFLICT branch on our own earlier row;
    ///   3. a work that also has a Suwayomi half, whose row the rebuild has ALREADY settled,
    ///      with the firehose then arriving on top. That is the interleaving the conflict
    ///      clause is restated for: the rebuild reaches the settled row as "mirror inserted,
    ///      then scanner merged", and here the mirror arrives second and must land in the
    ///      same place — assigning the display fields it owns and leaving `chapter_count`,
    ///      `detected_at` and `suwayomi_thumbnail` to the half that owns them. Copying the
    ///      rebuild's clause verbatim would write the mirror's NULL over a real Suwayomi
    ///      count, and a scanner-half card renders `Ch. {latest_chapter ?? chapter_count}`.
    #[tokio::test]
    async fn the_mirror_half_incremental_write_converges_with_the_periodic_rebuild() {
        let pool = pool().await;
        let (work, ssid) = mirror_work(&pool, "md-conv", "Convergent Series").await;
        mirror_chapter(&pool, &ssid, "c1", "1", "2026-06-01T00:00:00Z").await;

        // (1) INSERT branch.
        flush(&pool, &ssid).await;
        let incremental = feed_digest(&pool, &work).await.expect("must publish");
        let incremental_canonical = canonical_digest(&pool, &work).await.expect("must publish");
        catalog::refresh_feed_updates(&pool).await.unwrap();
        assert_eq!(
            incremental,
            feed_digest(&pool, &work).await.unwrap(),
            "the incremental writer and the periodic rebuild must not disagree about any \
             column of the merged feed"
        );
        assert_eq!(
            incremental_canonical,
            canonical_digest(&pool, &work).await.unwrap(),
            "nor about any column of `feed_updates`, which `canonicalUpdates` reads"
        );

        // (2) CONFLICT branch, on a row this path created. The new chapter must move BOTH
        //     the clock and the label, or the card announces an update it cannot name.
        mirror_chapter(&pool, &ssid, "c2", "2.5", "2026-06-08T00:00:00Z").await;
        flush(&pool, &ssid).await;
        let incremental = feed_digest(&pool, &work).await.unwrap();
        assert!(
            incremental.contains("|2.5|"),
            "the label must follow the newest RELEASE: {incremental}"
        );
        catalog::refresh_feed_updates(&pool).await.unwrap();
        assert_eq!(
            incremental,
            feed_digest(&pool, &work).await.unwrap(),
            "a second incremental write must still converge"
        );

        // (3) A work with BOTH halves, settled by the rebuild first.
        let (both, both_ssid) = mirror_work(&pool, "md-both", "Both Halves").await;
        catalog::upsert_source_series(&pool, &both, "suwayomi", "asura", "7", None, false)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO suwayomi_series \
                 (id, title, thumbnail_url, status, in_library, source_id, chapter_count, \
                  latest_chapter_at, updated_at) \
             VALUES (7, 'Suwayomi Title', '/thumb/7', 'ONGOING', 1, '1', 13, '1780000000000', \
                     '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO suwayomi_chapter \
                 (id, manga_id, name, chapter_number, page_count, upload_date, updated_at) \
             VALUES (1, 7, 'Ch 13', 13.0, 0, '1780000000000', '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        mirror_chapter(&pool, &both_ssid, "b1", "12", "2026-06-01T00:00:00Z").await;
        catalog::refresh_feed_updates(&pool).await.unwrap();
        let settled: (Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT chapter_count, suwayomi_thumbnail FROM feed_series_updates WHERE work_id = ?",
        )
        .bind(&both)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            settled,
            (Some(13), Some("/thumb/7".into())),
            "the fixture must really carry both halves, or (3) proves nothing"
        );

        // The firehose now mirrors a NEWER chapter than either half had.
        mirror_chapter(&pool, &both_ssid, "b2", "14", "2026-07-01T00:00:00Z").await;
        flush(&pool, &both_ssid).await;
        let incremental = feed_digest(&pool, &both).await.unwrap();
        assert!(
            incremental.contains("|14|") && incremental.contains("|13|"),
            "the mirror's label must advance while the Suwayomi chapter_count survives: \
             {incremental}"
        );
        catalog::refresh_feed_updates(&pool).await.unwrap();
        assert_eq!(
            incremental,
            feed_digest(&pool, &both).await.unwrap(),
            "a mirror write landing on a row the rebuild had already settled with the \
             scanner half must leave it where the next rebuild puts it"
        );

        // --- and again with the release ledger ACTIVE ---------------------------------
        //
        // Everything above ran with an empty `release_event`, so `ledger::is_complete` was
        // false and pass (5) was inert in BOTH writers — which proves they agree when the
        // projection is off and nothing about when it is on.
        while catalog::spine::drain_chapter_keys(&pool).await.unwrap() > 0 {}
        catalog::spine::drain_suwayomi_series(&pool).await.unwrap();
        while catalog::spine::drain_chapter_keys(&pool).await.unwrap() > 0 {}
        while catalog::ledger::seed_batch(&pool).await.unwrap() > 0 {}
        assert!(
            catalog::ledger::is_complete(&pool).await.unwrap(),
            "the fixture must reach a complete ledger, or this asserts nothing"
        );
        mirror_chapter(&pool, &ssid, "c3", "3", "2026-06-20T00:00:00Z").await;
        flush(&pool, &ssid).await;
        let incremental = feed_digest(&pool, &work).await.unwrap();
        assert!(
            incremental.contains("|3|"),
            "the ledger projection must relabel to the newest release: {incremental}"
        );
        catalog::refresh_feed_updates(&pool).await.unwrap();
        assert_eq!(
            incremental,
            feed_digest(&pool, &work).await.unwrap(),
            "with the ledger projecting the release clock, the two writers must STILL agree"
        );
    }

    /// A work whose every English chapter is scheduled in the FUTURE is not an update, and
    /// the incremental writer must reach the same answer the rebuild does — including
    /// RETRACTING a row it published earlier, which is what the rebuild's DELETE + INSERT
    /// does for free and an upsert-only path would not.
    #[tokio::test]
    async fn an_unreleased_chapter_never_reaches_the_feed() {
        let pool = pool().await;
        let (work, ssid) = mirror_work(&pool, "md-future", "Scheduled").await;
        mirror_chapter(&pool, &ssid, "f1", "1", "2099-01-01T00:00:00Z").await;

        assert_eq!(flush(&pool, &ssid).await, 0, "unreleased is not published");
        assert!(canonical_digest(&pool, &work).await.is_none());
        assert!(feed_digest(&pool, &work).await.is_none());
        catalog::refresh_feed_updates(&pool).await.unwrap();
        assert!(
            feed_digest(&pool, &work).await.is_none(),
            "and the rebuild agrees"
        );

        // Now it releases, and then the upload is WITHDRAWN back to a future date.
        mirror_chapter(&pool, &ssid, "f1", "1", "2026-06-01T00:00:00Z").await;
        assert_eq!(flush(&pool, &ssid).await, 1);
        assert!(canonical_digest(&pool, &work).await.is_some());
        mirror_chapter(&pool, &ssid, "f1", "1", "2099-01-01T00:00:00Z").await;
        assert_eq!(flush(&pool, &ssid).await, 0);
        assert!(
            canonical_digest(&pool, &work).await.is_none(),
            "a withdrawn release must not outlive itself on `canonicalUpdates` — the \
             rebuild's DELETE would have removed it"
        );
    }

    #[test]
    fn chapter_firehose_requests_every_content_rating() {
        // REGRESSION (2026-07-26): `list_chapters` sent no `contentRating[]` at all, and
        // MangaDex defaults list endpoints to safe,suggestive,erotica — so every
        // `pornographic` English chapter was invisible to the firehose from day one.
        // Upstream `total` for /chapter?translatedLanguage[]=en: 810,292 with the defaults
        // vs 876,433 with all four. In the mirror: 4 pornographic works with chapters,
        // 19,923 without.
        let params = chapter_list_params(SyncWindow::Created, None, 0);
        let ratings: Vec<&str> = params
            .iter()
            .filter(|(k, _)| k == "contentRating[]")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            ratings,
            vec!["safe", "suggestive", "erotica", "pornographic"],
            "the chapter firehose must not inherit MangaDex's default rating filter"
        );
        // Whatever else changes, these must not: English-only at the source, the manga
        // relationship (the only thing that maps a chapter to a source_series), and the
        // window's own order/since keys.
        assert!(params.contains(&("translatedLanguage[]".into(), "en".into())));
        assert!(params.contains(&("includes[]".into(), "manga".into())));
        assert!(params.contains(&("order[createdAt]".into(), "asc".into())));
        assert!(
            !params.iter().any(|(k, _)| k.ends_with("Since")),
            "a seed with no cursor must send no since param — that is what makes it \
             start at the beginning of the firehose"
        );
        // The same params with a cursor carry exactly one since key, matching the window.
        let incremental =
            chapter_list_params(SyncWindow::Updated, Some("2026-07-26T00:00:00"), 100);
        assert!(incremental.contains(&("updatedAtSince".into(), "2026-07-26T00:00:00".into())));
        assert!(incremental.contains(&("offset".into(), "100".into())));
        assert_eq!(
            incremental
                .iter()
                .filter(|(k, _)| k == "contentRating[]")
                .count(),
            4,
            "the incremental window needs the ratings too, or the forward path re-opens \
             the same hole"
        );
    }

    #[test]
    fn a_lost_chapter_write_never_latches_the_seed() {
        // REGRESSION: an `upsert_chapter` error was only `warn!`ed, so a seed that lost
        // chapters to SQLITE_BUSY (the scanner, the cover crawler and Litestream all write
        // this DB during a sweep) still reported dropped == 0 and latched `seed_done`.
        // After that the sweep runs the forward-only `updatedAtSince` window, which never
        // revisits an old `createdAt` — the loss is permanent, which is precisely how the
        // catalogue's 4,493-record gap became permanent.
        let clean = SweepOutcome {
            upserted: 71_126,
            dropped: 0,
            failed: 0,
            skipped: 0,
            unchanged: 805_307,
            // The feed writer's counter is not a loss counter and deliberately gates
            // nothing: a feed row is a cache the rebuild reconstructs, whereas every field
            // above describes a chapter that did or did not reach the durable mirror.
            feed: 0,
        };
        assert!(chapter_seed_may_latch(&clean));
        assert!(!chapter_seed_may_latch(&SweepOutcome {
            failed: 1,
            ..clean
        }));
        assert!(!chapter_seed_may_latch(&SweepOutcome {
            dropped: 1,
            ..clean
        }));
        assert!(!chapter_seed_may_latch(&SweepOutcome {
            dropped: 3,
            failed: 4,
            ..clean
        }));
        // F9: a chapter skipped because its work is not catalogued yet has not been
        // mirrored either, so it must hold the seed open just like a drop or a write error.
        // Before the fix the cursor advanced past those chapters and they were never
        // re-offered — a silent, permanent loss with no counter to show for it.
        assert!(!chapter_seed_may_latch(&SweepOutcome {
            skipped: 1,
            ..clean
        }));
        // `unchanged` is bookkeeping, not loss: a re-seed over an already-complete mirror
        // writes almost nothing and MUST still be allowed to latch, or the seed re-runs
        // its 45-70 minute walk every cycle forever.
        assert!(chapter_seed_may_latch(&SweepOutcome {
            upserted: 0,
            unchanged: 876_433,
            ..Default::default()
        }));
        // And the catalogue half now uses the same predicate — its upsert error arm used to
        // only warn, so `failed` was always 0 for it and `seed_done` latched over the loss.
        assert!(catalogue_seed_may_latch(&SweepOutcome {
            upserted: 113_741,
            ..Default::default()
        }));
        assert!(!catalogue_seed_may_latch(&SweepOutcome {
            upserted: 113_741,
            failed: 1,
            ..Default::default()
        }));
    }

    #[tokio::test]
    async fn an_unchanged_chapter_row_is_not_rewritten() {
        // The 0067 re-seed re-walks ~876k chapters of which ~805k are already mirrored
        // byte-identical. Without this check that is ~805k no-op write transactions
        // against SQLite's single writer; with it they are one index seek each.
        let pool = pool().await;
        let m: MdManga = serde_json::from_value(manga_json()).unwrap();
        let (id, input) = to_work_input(&m);
        catalog::upsert_work_from_mangadex(&pool, &id, &input)
            .await
            .unwrap();
        let ssid = catalog::find_source_series_id(&pool, "mangadex", "mangadex", &id)
            .await
            .unwrap()
            .expect("the mangadex source_series was ensured by the upsert");

        let ch = catalog::ChapterInput {
            external_id: "ch-uuid-1".into(),
            number: Some("12".into()),
            volume: Some("2".into()),
            lang: Some("en".into()),
            title: Some("The Storm".into()),
            published_at: Some("2026-07-01T00:00:00+00:00".into()),
            readable_at: Some("2026-06-30T00:00:00+00:00".into()),
            external_url: None,
            scanlator: None,
        };
        // Absent → must be written, never skipped.
        assert!(!chapter_row_unchanged(&pool, &ssid, &ch).await);
        catalog::upsert_chapter(&pool, &ssid, &ch).await.unwrap();
        assert!(chapter_row_unchanged(&pool, &ssid, &ch).await);

        // Every mutable column `upsert_chapter`'s DO UPDATE SET touches must defeat the
        // skip — a re-seed that swallowed a retitled or renumbered chapter would be worse
        // than the writes it saves.
        for changed in [
            catalog::ChapterInput {
                number: Some("12.5".into()),
                ..ch.clone()
            },
            catalog::ChapterInput {
                volume: None,
                ..ch.clone()
            },
            catalog::ChapterInput {
                lang: Some("es".into()),
                ..ch.clone()
            },
            catalog::ChapterInput {
                title: Some("The Storm (revised)".into()),
                ..ch.clone()
            },
            catalog::ChapterInput {
                published_at: Some("2026-07-02T00:00:00+00:00".into()),
                ..ch.clone()
            },
            // Migration 0073's two columns. These matter MORE than the others here: every
            // pre-0073 row has both NULL, so if the presence check ignored them the
            // firehose would skip its own chance to heal them and the reader would keep
            // rendering a blank page for an external chapter it already mirrored.
            catalog::ChapterInput {
                readable_at: Some("2026-06-29T00:00:00+00:00".into()),
                ..ch.clone()
            },
            catalog::ChapterInput {
                external_url: Some("https://mangaplus.shueisha.co.jp/viewer/1019123".into()),
                ..ch.clone()
            },
        ] {
            assert!(
                !chapter_row_unchanged(&pool, &ssid, &changed).await,
                "a changed column must not be skipped: {changed:?}"
            );
        }
        // Same external id under a DIFFERENT source_series is a different row: the
        // presence check is keyed on the pair, exactly like the UNIQUE constraint.
        assert!(!chapter_row_unchanged(&pool, "ss_does_not_exist", &ch).await);
    }

    #[test]
    fn chapter_id_lookups_ask_for_every_rating_and_filter_nothing_else() {
        // A1b's request shape. The two things that can go wrong here are both silent and
        // both permanent: an id MangaDex filters out of the response reads as "upstream has
        // no answer", so the row takes a fallback clock and leaves the work-list for good.
        let ids: Vec<String> = (1..=100).map(|i| format!("chapter-uuid-{i}")).collect();
        let params = chapter_ids_params(&ids);

        // One `ids[]` per id, in order, none dropped or deduped.
        let sent: Vec<&str> = params
            .iter()
            .filter(|(k, _)| k == "ids[]")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(sent.len(), 100);
        assert_eq!(sent[0], "chapter-uuid-1");
        assert_eq!(sent[99], "chapter-uuid-100");
        // `limit` must cover the whole batch: MangaDex defaults `/chapter` to 10, so a
        // missing limit would answer 10 of the 100 and silently fall back on the other 90.
        assert!(params.contains(&("limit".into(), "100".into())));
        // All four ratings — `/chapter` applies its safe,suggestive,erotica default to an
        // `ids[]` query too, exactly as it does to the firehose.
        let ratings: Vec<&str> = params
            .iter()
            .filter(|(k, _)| k == "contentRating[]")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            ratings,
            vec!["safe", "suggestive", "erotica", "pornographic"]
        );
        // And NOTHING else that can subtract from an already-exact id list.
        assert!(
            !params.iter().any(|(k, _)| k == "translatedLanguage[]"),
            "an id lookup must not carry a language filter — it can only lose rows"
        );
        assert!(!params.iter().any(|(k, _)| k.ends_with("Since")));
        assert!(!params.iter().any(|(k, _)| k == "offset"));
    }

    #[tokio::test]
    async fn an_oversized_id_batch_is_rejected_rather_than_truncated() {
        // Truncating to MangaDex's 100-id ceiling would leave the tail of the batch
        // unanswered — and an unanswered row that is never written stays in the work-list
        // forever, which is the one failure mode a cursorless drain cannot survive. Fails
        // before any request is built, so this test makes no network call.
        let client = MangaDexClient::new("komika-test", 5.0, 40.0);
        let ids: Vec<String> = (0..101).map(|i| format!("id-{i}")).collect();
        let Err(err) = client.get_chapters_by_ids(&ids).await else {
            panic!("101 ids must be rejected, not silently cut to the API's ceiling");
        };
        assert!(err.to_string().contains("at most 100 ids"), "{err}");
        // Empty is a no-op, not a request for "every chapter on MangaDex".
        let page = client.get_chapters_by_ids(&[]).await.unwrap();
        assert_eq!(page.raw_len, 0);
        assert!(page.chapters.is_empty());
    }

    #[test]
    fn a_chapter_with_no_upstream_readable_at_still_leaves_the_work_list() {
        // THE INFINITE-LOOP GUARD. The work-list is `readable_at IS NULL` and there is no
        // cursor, so any row this rule declines to fill is a row that is offered again
        // forever — the exact loop `catalog::ledger::seedable_where` documents having hit
        // and fixed. Every branch below must produce a value.
        fn attrs(v: Value) -> MdChapterAttrs {
            serde_json::from_value(v).unwrap()
        }
        let row = PendingChapter {
            id: "c_1".into(),
            external_id: "chapter-uuid-1".into(),
            published_at: Some("2026-01-01T00:00:00+00:00".into()),
            created_at: "2025-12-01T00:00:00+00:00".into(),
        };

        // 1. Upstream `readableAt` wins, and it wins even when it PRECEDES `publishAt` —
        //    sampled bilibili chapters are readable two weeks before their publishAt, so
        //    these are independent timestamps, not a scheduled/actual pair.
        let r = resolve_readable_at(
            &row,
            Some(&attrs(serde_json::json!({
                "readableAt": "2022-11-02T00:00:00+00:00",
                "publishAt": "2022-11-16T00:00:00+00:00",
            }))),
        );
        assert_eq!(r.readable_at, "2022-11-02T00:00:00+00:00");
        assert!(r.answered);

        // 2. Upstream answered but has no readable clock → `publishAt`, per §6.4's
        //    `released_at := readable_at when present := published_at otherwise`.
        let r = resolve_readable_at(
            &row,
            Some(&attrs(serde_json::json!({
                "publishAt": "2037-12-31T15:00:00+00:00",
            }))),
        );
        assert_eq!(r.readable_at, "2037-12-31T15:00:00+00:00");

        // 3. Upstream answered with neither, or did not answer at all → the row's own
        //    `published_at`. Both must terminate; neither may return an empty string, which
        //    is what MangaDex sends for some absent fields and which SQLite would happily
        //    store as a non-NULL non-timestamp.
        for upstream in [
            Some(attrs(serde_json::json!({
                "readableAt": "",
                "publishAt": "   ",
            }))),
            None,
        ] {
            let r = resolve_readable_at(&row, upstream.as_ref());
            assert_eq!(r.readable_at, "2026-01-01T00:00:00+00:00");
        }

        // 4. Last rung: `created_at` is NOT NULL, so the ladder cannot run out. (Unreachable
        //    on the 2026-07-30 snapshot — 0 of 877,868 MangaDex chapters have a NULL
        //    `published_at` — but the drain must not depend on that staying true.)
        let bare = PendingChapter {
            published_at: None,
            ..row.clone()
        };
        let r = resolve_readable_at(&bare, None);
        assert_eq!(r.readable_at, "2025-12-01T00:00:00+00:00");
        assert!(
            !r.answered,
            "an absent upstream record must not read as answered"
        );

        // `externalUrl` is the ONLY valid external-chapter test — `pages` is not, and a
        // sampled bilibilicomics external reports `pages: 45`.
        let r = resolve_readable_at(
            &row,
            Some(&attrs(serde_json::json!({
                "readableAt": "2023-10-07T00:00:00+00:00",
                "externalUrl": "https://mangaplus.shueisha.co.jp/viewer/1019123",
                "pages": 45,
            }))),
        );
        assert_eq!(
            r.external_url.as_deref(),
            Some("https://mangaplus.shueisha.co.jp/viewer/1019123")
        );
        // An empty `externalUrl` must not badge a perfectly readable chapter as off-site.
        let r = resolve_readable_at(
            &row,
            Some(&attrs(serde_json::json!({
                "readableAt": "2023-10-07T00:00:00+00:00",
                "externalUrl": "",
                "pages": 0,
            }))),
        );
        assert_eq!(r.external_url, None);
    }

    #[tokio::test]
    async fn the_readable_backfill_drains_scoped_to_mangadex_and_resumes() {
        let pool = pool().await;
        let m: MdManga = serde_json::from_value(manga_json()).unwrap();
        let (work_key, input) = to_work_input(&m);
        catalog::upsert_work_from_mangadex(&pool, &work_key, &input)
            .await
            .unwrap();
        let ssid = catalog::find_source_series_id(&pool, "mangadex", "mangadex", &work_key)
            .await
            .unwrap()
            .unwrap();
        let work_id: String = sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = ?")
            .bind(&ssid)
            .fetch_one(&pool)
            .await
            .unwrap();

        let unfilled = |external_id: &str, external_url: Option<&str>| catalog::ChapterInput {
            external_id: external_id.into(),
            number: Some("1".into()),
            lang: Some("en".into()),
            title: Some("Ch 1".into()),
            published_at: Some("2037-12-31T15:00:00+00:00".into()),
            // Every pre-0073 row looks exactly like this.
            readable_at: None,
            external_url: external_url.map(str::to_string),
            ..Default::default()
        };
        for id in ["md-1", "md-2", "md-3"] {
            catalog::upsert_chapter(&pool, &ssid, &unfilled(id, None))
                .await
                .unwrap();
        }
        // md-4 already carries a URL the firehose healed onto it, and upstream will NOT
        // answer for it below — the backfill must not blank it.
        catalog::upsert_chapter(
            &pool,
            &ssid,
            &unfilled("md-4", Some("https://comikey.com/x")),
        )
        .await
        .unwrap();

        // A Suwayomi row with no release clock. `chapter` is the unified spine since Phase
        // B, so an unscoped work-list would hand this numeric external_id to
        // `/chapter?ids[]=`, which can never resolve it — offering it forever.
        let suwa = catalog::upsert_source_series(
            &pool, &work_id, "suwayomi", "src-1", "4242", None, false,
        )
        .await
        .unwrap();
        catalog::upsert_chapter(&pool, &suwa, &unfilled("991", None))
            .await
            .unwrap();

        let pending = chapters_needing_readable_at(&pool, 100).await.unwrap();
        let mut got: Vec<&str> = pending.iter().map(|p| p.external_id.as_str()).collect();
        got.sort_unstable();
        assert_eq!(got, vec!["md-1", "md-2", "md-3", "md-4"]);
        assert_eq!(readable_backfill_remaining(&pool).await.unwrap(), 4);

        // --- Batch 1, resumable by construction: take two of the four. ------------------
        let batch = chapters_needing_readable_at(&pool, 2).await.unwrap();
        assert_eq!(batch.len(), 2);
        let asked: Vec<String> = batch.iter().map(|p| p.external_id.clone()).collect();
        // Upstream answers for the first only, and that one is external — the F1 shape.
        let upstream: Vec<MdChapter> = vec![serde_json::from_value(serde_json::json!({
            "id": asked[0],
            "attributes": {
                "readableAt": "2023-10-07T00:00:00+00:00",
                "publishAt": "2037-12-31T15:00:00+00:00",
                "externalUrl": "https://mangaplus.shueisha.co.jp/viewer/1019123",
                "pages": 0,
            },
        }))
        .unwrap()];
        let fill = write_readable_backfill(&pool, &batch, &upstream)
            .await
            .unwrap();
        assert_eq!(fill.filled, 2, "every offered row must be written");
        assert_eq!(fill.from_upstream, 1);
        assert_eq!(fill.external, 1);
        assert_eq!(fill.unanswered, 1);
        // The 2037 sentinel is gone from the answered row: this is what returns the 46
        // sentinel works to /updates.
        let (readable, url): (String, Option<String>) =
            sqlx::query_as("SELECT readable_at, external_url FROM chapter WHERE external_id = ?")
                .bind(&asked[0])
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(readable, "2023-10-07T00:00:00+00:00");
        assert_eq!(
            url.as_deref(),
            Some("https://mangaplus.shueisha.co.jp/viewer/1019123")
        );

        // Resumability: the work-list is the data, so the second pass is offered exactly
        // the rows the first did not write — no cursor, nothing to go stale across a
        // restart, and the two already-written rows never come back.
        let remaining_ids: Vec<String> = chapters_needing_readable_at(&pool, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.external_id)
            .collect();
        assert_eq!(remaining_ids.len(), 2);
        assert!(remaining_ids.iter().all(|id| !asked.contains(id)));

        // --- Batch 2: upstream answers for NOTHING. ------------------------------------
        // The termination case. If a row upstream has no answer for were left NULL it would
        // be re-offered forever and the drain would never reach "complete".
        let batch = chapters_needing_readable_at(&pool, 100).await.unwrap();
        let fill = write_readable_backfill(&pool, &batch, &[]).await.unwrap();
        assert_eq!(fill.filled, 2);
        assert_eq!(fill.unanswered, 2);
        assert_eq!(fill.from_upstream, 0);

        assert!(chapters_needing_readable_at(&pool, 100)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(readable_backfill_remaining(&pool).await.unwrap(), 0);

        // Fallback rows took their own `published_at` — i.e. the backfill materialised the
        // `COALESCE(readable_at, published_at, created_at)` the read path already computes,
        // changing no behaviour and only ending the loop.
        let sentinels: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chapter WHERE source_series_id = ? \
               AND readable_at = '2037-12-31T15:00:00+00:00'",
        )
        .bind(&ssid)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sentinels, 3);
        // And an unanswered row keeps the URL the firehose already healed onto it. Writing
        // NULL there would re-blank a chapter we had already fixed.
        let url: Option<String> =
            sqlx::query_scalar("SELECT external_url FROM chapter WHERE external_id = 'md-4'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(url.as_deref(), Some("https://comikey.com/x"));

        // The Suwayomi row was never offered and is untouched — still NULL, still invisible
        // to a backfill that could not resolve it.
        let suwa_readable: Option<String> =
            sqlx::query_scalar("SELECT readable_at FROM chapter WHERE source_series_id = ?")
                .bind(&suwa)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(suwa_readable, None);
    }

    #[test]
    fn classify_page_non_empty_always_processes() {
        // The regression guard: a full RAW page whose PARSED count fell below
        // PAGE_LIMIT because records were skipped must still be Process, never
        // EndWindow — parse-skips must not truncate the seed.
        assert_eq!(classify_page(100, 0, 113_610), PageStep::Process);
        assert_eq!(classify_page(97, 1_100, 113_610), PageStep::Process); // 3 skipped
        assert_eq!(classify_page(1, 113_609, 113_610), PageStep::Process);
    }

    #[test]
    fn classify_page_empty_at_or_after_total_ends() {
        assert_eq!(classify_page(0, 113_610, 113_610), PageStep::EndWindow);
        assert_eq!(classify_page(0, 200_000, 113_610), PageStep::EndWindow);
        // An empty window (total == 0) is immediately done.
        assert_eq!(classify_page(0, 0, 0), PageStep::EndWindow);
    }

    #[test]
    fn classify_page_empty_before_total_retries() {
        // A transient empty page short of the total is NOT the end — retry it.
        assert_eq!(classify_page(0, 1_100, 113_610), PageStep::RetryEmpty);
        assert_eq!(classify_page(0, 0, 113_610), PageStep::RetryEmpty);
    }

    #[test]
    fn page_error_status_extracts_the_http_code() {
        assert_eq!(
            page_error_status("MangaDex /manga error 400 Bad Request"),
            Some(400)
        );
        assert_eq!(
            page_error_status("MangaDex /manga?ids error 404 Not Found"),
            Some(404)
        );
        assert_eq!(
            page_error_status("MangaDex /manga request failed: connection reset"),
            None
        );
    }

    #[test]
    fn tolerable_page_errors_are_the_transient_ones() {
        // The exact error that killed the 2026-07-24 backfill after 3 of ~4,500 works.
        assert!(is_tolerable_page_error(&anyhow!(
            "MangaDex /manga error 400 Bad Request"
        )));
        assert!(is_tolerable_page_error(&anyhow!(
            "MangaDex /manga error 429 Too Many Requests"
        )));
        assert!(is_tolerable_page_error(&anyhow!(
            "MangaDex /manga error 503 Service Unavailable"
        )));
        // Transport failures (already retried twice inside get_with_retry) too.
        assert!(is_tolerable_page_error(&anyhow!(
            "MangaDex /manga request failed: operation timed out"
        )));
        // Real client errors and decode failures still abort the pass.
        assert!(!is_tolerable_page_error(&anyhow!(
            "MangaDex /manga error 404 Not Found"
        )));
        assert!(!is_tolerable_page_error(&anyhow!(
            "MangaDex /manga error 403 Forbidden"
        )));
        assert!(!is_tolerable_page_error(&anyhow!(
            "error decoding response body"
        )));
    }

    #[test]
    fn backfill_completion_requires_a_clean_sweep() {
        // The gate `run_backfill_pass` applies before latching the one-shot marker:
        // reaching the end of the catalogue is NOT enough if records were lost.
        let clean = BackfillOutcome {
            scanned: 113_610,
            ingested: 4_500,
            tagged: 0,
            failed: 0,
            complete: true,
            truncated: 0,
            dropped: 0,
        };
        assert!(clean.is_clean_completion());
        assert!(!clean.is_dirty());
        let lossy = BackfillOutcome { failed: 4, ..clean };
        assert!(!lossy.is_clean_completion());
        // A budget-truncated pass never completes either.
        let partial = BackfillOutcome {
            complete: false,
            ..clean
        };
        assert!(!partial.is_clean_completion());
        // A fresh pass starts clean and incomplete.
        assert!(!BackfillOutcome::default().complete);
        assert!(!BackfillOutcome::default().is_clean_completion());
    }

    #[test]
    fn a_truncated_window_never_latches_completion() {
        // REGRESSION: an empty page that outlives EMPTY_PAGE_RETRIES while
        // `offset < total` used to set `done = true`, which made the pass report
        // `complete` with `failed == 0` — latching a one-shot `maintenance_flag` that
        // nothing ever clears. That is the exact blip that truncated the original seed,
        // so it would have frozen the very gap this backfill exists to repair.
        let truncated = BackfillOutcome {
            scanned: 40_000,
            ingested: 900,
            tagged: 0,
            failed: 0,
            complete: true,
            truncated: 1,
            dropped: 0,
        };
        assert!(
            !truncated.is_clean_completion(),
            "a truncated window must never latch the one-shot completion marker"
        );
        assert!(
            truncated.is_dirty(),
            "a truncated window must force a cursor rewind"
        );
    }

    #[test]
    fn a_dirty_pass_rewinds_even_when_incomplete() {
        // REGRESSION (cross-pass amnesia): `failed`/`truncated` are per-pass but the
        // completion marker is permanent and survives restarts. A budget-stopped pass
        // that lost records used to keep its cursor, so a LATER pass resumed past them,
        // reported `failed == 0`, and latched the marker — abandoning them for good.
        // `is_dirty()` is checked regardless of `complete`, so the cursor is rewound and
        // any future completion is preceded by a full clean re-walk.
        let dirty_partial = BackfillOutcome {
            scanned: 20_000,
            ingested: 300,
            tagged: 0,
            failed: 2,
            complete: false, // stopped on the wall-clock budget
            truncated: 0,
            dropped: 0,
        };
        assert!(
            dirty_partial.is_dirty(),
            "a budget-stopped pass that lost records must still rewind the cursor"
        );
        assert!(!dirty_partial.is_clean_completion());
        // The clean budget-stop is the case that must NOT rewind — it keeps its
        // hard-won cursor so the next pass resumes instead of re-walking.
        let clean_partial = BackfillOutcome {
            scanned: 20_000,
            ingested: 300,
            tagged: 0,
            failed: 0,
            complete: false,
            truncated: 0,
            dropped: 0,
        };
        assert!(
            !clean_partial.is_dirty(),
            "a clean budget-stop must keep its cursor"
        );
    }

    #[test]
    fn dropped_records_never_latch_completion() {
        // REGRESSION (2026-07-26, the 4,493-record no-op). The v1 backfill walked the
        // ENTIRE catalogue, fetched all 4,493 records it existed to repair, failed to
        // parse every one of them on `"links": null`, threw the serde error away, and
        // reported scanned≈109,266 / ingested=0 / failed=0 / truncated=0. That passed
        // `is_clean_completion()` and latched a permanent `maintenance_flag` over a total
        // no-op — the gap looked repaired and was not. A record that was FETCHED but never
        // persisted is the same class of loss as `failed`/`truncated`, so `dropped` must
        // both block the one-shot marker and force the cursor rewind.
        let dropped = BackfillOutcome {
            scanned: 109_266,
            ingested: 0,
            tagged: 0,
            failed: 0,
            complete: true,
            truncated: 0,
            dropped: 1,
        };
        assert!(
            !dropped.is_clean_completion(),
            "a pass that dropped a fetched record must never latch the one-shot marker"
        );
        assert!(
            dropped.is_dirty(),
            "a dropped record must force the cursor rewind, like failed/truncated"
        );
        // The shape of the actual 2026-07-26 no-op, for the record.
        let the_no_op = BackfillOutcome {
            dropped: 4_493,
            ..dropped
        };
        assert!(!the_no_op.is_clean_completion());
        assert!(the_no_op.is_dirty());
    }

    fn manga_json() -> Value {
        serde_json::json!({
            "id": "md-uuid-1",
            "attributes": {
                "title": { "ja-ro": "Tensei Shitara Slime Datta Ken" },
                "altTitles": [ { "en": "That Time I Got Reincarnated as a Slime" }, { "ja": "転生したらスライムだった件" } ],
                "description": { "en": "Mikami Satoru is reincarnated as a slime." },
                "originalLanguage": "ja",
                "status": "ongoing",
                "year": 2015,
                "publicationDemographic": "seinen",
                "contentRating": "safe",
                "links": { "al": "101517", "mal": "70951", "raw": "https://example.com" },
                "createdAt": "2018-10-04T22:16:00+00:00"
            },
            "relationships": [
                { "id": "auth-1", "type": "author", "attributes": { "name": "Fuse" } },
                { "id": "cov-1", "type": "cover_art", "attributes": { "fileName": "abc.jpg" } }
            ]
        })
    }

    #[test]
    fn maps_manga_to_work_input() {
        let m: MdManga = serde_json::from_value(manga_json()).unwrap();
        let (id, input) = to_work_input(&m);
        assert_eq!(id, "md-uuid-1");
        assert_eq!(input.primary_lang.as_deref(), Some("ja-ro"));
        assert_eq!(input.status.as_deref(), Some("ONGOING"));
        assert_eq!(input.year, Some(2015));
        assert_eq!(input.author.as_deref(), Some("Fuse"));
        assert!(!input.is_nsfw);
        // altTitles + localized titles all become aliases.
        assert!(input
            .aliases
            .iter()
            .any(|a| a.raw.contains("Slime Datta Ken")));
        assert!(input.aliases.iter().any(|a| a.raw.contains("Reincarnated")));
        // Only known external providers are captured ("raw" is ignored).
        assert!(input.external_ids.contains(&("al".into(), "101517".into())));
        assert!(input.external_ids.contains(&("mal".into(), "70951".into())));
        assert!(!input.external_ids.iter().any(|(p, _)| p == "raw"));
        assert_eq!(cover_file_name(&m).as_deref(), Some("abc.jpg"));
        // The fileName is carried on the work input so a cover URL can be built later.
        assert_eq!(input.cover_file_name.as_deref(), Some("abc.jpg"));
        assert_eq!(
            cover_url("md-uuid-1", "abc.jpg"),
            "https://uploads.mangadex.org/covers/md-uuid-1/abc.jpg"
        );
        assert_eq!(
            cover_thumb_url("md-uuid-1", "abc.jpg"),
            "https://uploads.mangadex.org/covers/md-uuid-1/abc.jpg.512.jpg"
        );
    }

    #[test]
    fn maps_multilang_descriptions_and_full_credits() {
        // S2: a manga with descriptions in several languages and multiple
        // authors + artists must surface ALL of them (not just the primary).
        let mut v = manga_json();
        v["attributes"]["description"] = serde_json::json!({
            "en": "English blurb.",
            "es": "Sinopsis en español.",
            "fr": "" // empty is dropped
        });
        v["relationships"] = serde_json::json!([
            { "id": "a1", "type": "author", "attributes": { "name": "Fuse" } },
            { "id": "a2", "type": "author", "attributes": { "name": "Co-Author" } },
            { "id": "ar1", "type": "artist", "attributes": { "name": "Mitz Vah" } },
            { "id": "ar1dup", "type": "artist", "attributes": { "name": "Mitz Vah" } }, // dedup
            { "id": "cov", "type": "cover_art", "attributes": { "fileName": "abc.jpg" } }
        ]);
        let m: MdManga = serde_json::from_value(v).unwrap();
        let (_, input) = to_work_input(&m);

        // Descriptions: en + es present, empty fr dropped, sorted by lang.
        assert_eq!(
            input.descriptions,
            vec![
                ("en".to_string(), "English blurb.".to_string()),
                ("es".to_string(), "Sinopsis en español.".to_string()),
            ]
        );
        // Credits: both authors + the artist, artist de-duplicated.
        assert!(input.credits.contains(&("author".into(), "Fuse".into())));
        assert!(input
            .credits
            .contains(&("author".into(), "Co-Author".into())));
        assert_eq!(
            input
                .credits
                .iter()
                .filter(|(r, n)| r == "artist" && n == "Mitz Vah")
                .count(),
            1,
            "artist is de-duplicated"
        );
        // The singular fields keep the FIRST of each (reader-shape primary).
        assert_eq!(input.author.as_deref(), Some("Fuse"));
        assert_eq!(input.artist.as_deref(), Some("Mitz Vah"));
    }

    #[test]
    fn relationship_cover_and_cover_fetch_mapping() {
        // F2: the manga's cover_art relationship maps to a primary Cover...
        let mut v = manga_json();
        v["relationships"] = serde_json::json!([
            { "id": "cov1", "type": "cover_art",
              "attributes": { "fileName": "primary.jpg", "locale": "ja", "volume": "1" } },
            { "id": "a1", "type": "author", "attributes": { "name": "Fuse" } }
        ]);
        let m: MdManga = serde_json::from_value(v).unwrap();
        let (_, input) = to_work_input(&m);
        assert_eq!(input.covers.len(), 1);
        assert_eq!(input.covers[0].file_name, "primary.jpg");
        assert!(input.covers[0].is_primary);
        assert_eq!(input.covers[0].volume.as_deref(), Some("1"));

        // ...and a /cover fetch maps to the full set, marking the primary by name.
        let fetched = vec![
            (
                "primary.jpg".to_string(),
                Some("ja".into()),
                Some("1".into()),
            ),
            ("vol2.jpg".to_string(), Some("ja".into()), Some("2".into())),
            ("en3.jpg".to_string(), Some("en".into()), Some("3".into())),
        ];
        let covers = covers_from_fetch(fetched, Some("primary.jpg"));
        assert_eq!(covers.len(), 3);
        assert_eq!(covers.iter().filter(|c| c.is_primary).count(), 1);
        assert!(
            covers
                .iter()
                .find(|c| c.file_name == "primary.jpg")
                .unwrap()
                .is_primary
        );

        // No match → first is promoted to primary (exactly one primary always).
        let covers = covers_from_fetch(
            vec![("x.jpg".into(), None, None), ("y.jpg".into(), None, None)],
            Some("missing.jpg"),
        );
        assert!(covers[0].is_primary);
        assert_eq!(covers.iter().filter(|c| c.is_primary).count(), 1);
    }

    /// `manga_json()` plus a realistic `attributes.tags[]`, in deliberately SHUFFLED
    /// group order so the ordering assertions below can't pass by accident.
    fn manga_json_with_tags() -> Value {
        let mut v = manga_json();
        v["attributes"]["tags"] = serde_json::json!([
            // `content` — must sort LAST despite appearing first.
            { "id": "t-gore", "attributes": { "group": "content", "name": { "en": "Gore" } } },
            // `theme`, listed before `genre` on purpose.
            { "id": "t-isekai", "attributes": { "group": "theme", "name": { "en": "Isekai", "ja": "異世界" } } },
            { "id": "t-monsters", "attributes": { "group": "theme", "name": { "en": "Monsters" } } },
            // `genre` — must sort FIRST. Two of them, to prove within-group API order holds.
            { "id": "t-action", "attributes": { "group": "genre", "name": { "en": "Action" } } },
            { "id": "t-comedy", "attributes": { "group": "genre", "name": { "en": "Comedy" } } },
            // `format`, between theme and content.
            { "id": "t-strip", "attributes": { "group": "format", "name": { "en": "Long Strip" } } },
            // Japanese-only: unusable as an English facet label, must be DROPPED rather
            // than leaking a Japanese string into an English genre list.
            { "id": "t-jp", "attributes": { "group": "genre", "name": { "ja": "日本語のみ" } } },
            // A duplicate of an earlier tag — the (work_id, tag) primary key would drop
            // it at insert anyway; dedupe here keeps `ord` contiguous.
            { "id": "t-dup", "attributes": { "group": "content", "name": { "en": "Action" } } },
            // Whitespace-only name: nothing to store.
            { "id": "t-blank", "attributes": { "group": "genre", "name": { "en": "   " } } },
            // Unknown group: kept, sorted after every known group.
            { "id": "t-weird", "attributes": { "group": "newfangled", "name": { "en": "Experimental" } } },
            // No attributes at all — must contribute nothing and, crucially, must not
            // fail the whole record (the `links: null` lesson).
            { "id": "t-bare" }
        ]);
        v
    }

    #[test]
    fn tags_become_ordered_english_genres() {
        let m: MdManga = serde_json::from_value(manga_json_with_tags())
            .expect("a realistic tags[] array must parse");
        let (_, input) = to_work_input(&m);
        // Grouped genre -> theme -> format -> content -> unknown, API order within a
        // group, English only, deduped, blanks dropped.
        assert_eq!(
            input.tags,
            vec![
                "Action",       // genre
                "Comedy",       // genre
                "Isekai",       // theme
                "Monsters",     // theme
                "Long Strip",   // format
                "Gore",         // content
                "Experimental", // unknown group sorts last
            ]
        );
    }

    #[test]
    fn absent_or_null_tags_yield_no_genres() {
        // A work with no tags is a legitimate state, and `replace_source_tags` relies on
        // an empty list meaning "this work genuinely has none" rather than "parse failed".
        let m: MdManga = serde_json::from_value(manga_json()).unwrap();
        assert!(to_work_input(&m).1.tags.is_empty(), "absent tags[]");

        let mut v = manga_json();
        v["attributes"]["tags"] = Value::Null;
        let m: MdManga = serde_json::from_value(v)
            .expect("tags: null must behave like absent tags, not fail the work");
        assert!(to_work_input(&m).1.tags.is_empty(), "null tags[]");
    }

    #[test]
    fn null_links_record_still_parses() {
        // THE REGRESSION GUARD for the 4,493-record silent gap (2026-07-26).
        //
        // MangaDex emits an explicit `"links": null` — key PRESENT, value null — on a
        // closed legacy cohort (2018: 636, 2019: 1,520, 2020: 2,054, 2021: 283, 2022+: 0).
        // Live exemplar: c6a8967b-2b61-4e14-8aca-1525a37b63f7 ("Yururira", createdAt
        // 2018-02-12), whose `links` and `year` are both null.
        //
        // `#[serde(default)]` covers only an ABSENT field: with the key present, serde
        // still calls `HashMap`'s `Deserialize` on a `Null` token and fails with
        // "invalid type: null, expected a map" — failing the WHOLE `MdManga`. 4,493 works
        // were fetched and thrown away on every sweep because of it.
        let mut v = manga_json();
        v["attributes"]["links"] = Value::Null;
        v["attributes"]["year"] = Value::Null; // null alongside it in the real cohort
        let m: MdManga = serde_json::from_value(v)
            .expect("a record with links: null must parse, not fail the whole work");
        let (id, input) = to_work_input(&m);
        assert_eq!(id, "md-uuid-1");
        // The title survives — this is the whole point: the work reaches the spine.
        assert_eq!(input.primary_lang.as_deref(), Some("ja-ro"));
        assert!(input
            .aliases
            .iter()
            .any(|a| a.raw.contains("Slime Datta Ken")));
        // A null `links` yields no external ids, exactly like an absent one.
        assert!(
            input.external_ids.is_empty(),
            "null links must behave like absent links"
        );
        assert_eq!(input.year, None);
    }

    #[test]
    fn every_null_container_behaves_like_an_absent_one() {
        // Same trap, every other non-`Option` container in the response shapes. `links`
        // is the field that actually bit us; hardening only `links` would just wait for a
        // second cohort with a different null field to reproduce the outage.
        for field in ["title", "altTitles", "description", "links"] {
            let mut v = manga_json();
            v["attributes"][field] = Value::Null;
            let m: MdManga = serde_json::from_value(v)
                .unwrap_or_else(|e| panic!("attributes.{field} = null must parse, got: {e}"));
            let (id, _) = to_work_input(&m);
            assert_eq!(id, "md-uuid-1");
        }
        // Top-level `relationships: null` (drops author/artist/cover, keeps the work).
        let mut v = manga_json();
        v["relationships"] = Value::Null;
        let m: MdManga =
            serde_json::from_value(v).expect("relationships = null must parse, not fail the work");
        let (_, input) = to_work_input(&m);
        assert_eq!(input.primary_lang.as_deref(), Some("ja-ro"));
        assert_eq!(input.author, None);
        assert!(input.covers.is_empty());
        assert_eq!(cover_file_name(&m), None);
        // A record with EVERY container null still yields a usable (if bare) work.
        let mut v = manga_json();
        v["attributes"]["altTitles"] = Value::Null;
        v["attributes"]["description"] = Value::Null;
        v["attributes"]["links"] = Value::Null;
        v["relationships"] = Value::Null;
        let m: MdManga = serde_json::from_value(v).expect("all-null containers must parse");
        let (_, input) = to_work_input(&m);
        assert!(input.external_ids.is_empty());
        assert!(input.descriptions.is_empty());

        // Chapter records get the same hardening, for symmetry.
        let c: MdChapter = serde_json::from_value(serde_json::json!({
            "id": "ch-1",
            "attributes": { "chapter": "1", "translatedLanguage": "en" },
            "relationships": Value::Null,
        }))
        .expect("chapter relationships = null must parse");
        assert_eq!(c.id, "ch-1");
        assert!(c.relationships.is_empty());
    }

    #[test]
    fn parse_manga_page_reports_drops() {
        // The pure per-record parse step, split out of `list_manga` so the drop
        // accounting is testable without HTTP.
        let good = manga_json();
        let mut null_links = manga_json();
        null_links["id"] = serde_json::json!("c6a8967b-2b61-4e14-8aca-1525a37b63f7");
        null_links["attributes"]["links"] = Value::Null;
        let mut third = manga_json();
        third["id"] = serde_json::json!("md-uuid-3");

        let raw = vec![good, null_links, third];
        let raw_len = raw.len();
        let (mangas, drops) = parse_manga_page(raw);
        // AFTER THE FIX: all three parse. Before it, the middle record vanished.
        assert_eq!(mangas.len(), 3, "the null-links record must parse now");
        assert!(drops.is_empty(), "nothing should be dropped: {drops:?}");

        // And when a record genuinely IS unparseable, the drop is ATTRIBUTABLE: the uuid
        // is read off the raw Value before the parse attempt and the serde message is
        // kept, instead of the old `Err(_) => skipped += 1` that hid this bug for months.
        let mut broken = manga_json();
        broken["id"] = serde_json::json!("md-uuid-broken");
        broken["attributes"] = Value::Null; // no attributes => no title => rightly dropped
        let raw = vec![manga_json(), broken];
        let raw_len_with_drop = raw.len();
        let (mangas, drops) = parse_manga_page(raw);
        assert_eq!(mangas.len(), 1);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].0, "md-uuid-broken");
        assert!(
            !drops[0].1.is_empty(),
            "the serde error must be kept, not discarded"
        );

        // PAGINATION INVARIANT: `raw_len` — never the parsed count — drives
        // `classify_page` and the `offset` advance. A page trimmed only by drops must
        // still be `Process`, or the sweep mistakes it for the end of the window (and,
        // with a short offset advance, desyncs from the API's own pagination).
        assert_eq!(
            classify_page(raw_len, 0, 113_759),
            PageStep::Process,
            "raw_len must drive classify_page"
        );
        assert_eq!(
            classify_page(raw_len_with_drop, 0, 113_759),
            PageStep::Process
        );
        // The parsed count is what would have been WRONG: a 100-record page where every
        // record dropped (the real 2026-07-26 shape at some offsets) reads as EndWindow.
        assert_eq!(classify_page(0, 0, 113_759), PageStep::RetryEmpty);
        assert_ne!(classify_page(0, 0, 0), PageStep::Process);
    }

    #[test]
    fn offset_advances_by_the_raw_length_not_the_parsed_one() {
        // THE HIGHEST-RISK REGRESSION in the drop-accounting change, pinned: `offset`
        // must advance by the number of records the API RETURNED. A page of 100 that
        // parsed 97 still advances 100 — advancing 97 would re-request three records
        // every page and, with a `< PAGE_LIMIT` end test, walk off the API's own
        // pagination. Silent record loss, worse than the bug being fixed.
        let raw = vec![manga_json(), {
            let mut broken = manga_json();
            broken["attributes"] = Value::Null;
            broken
        }];
        let raw_len = raw.len();
        let (parsed, drops) = parse_manga_page(raw);
        assert_eq!((parsed.len(), drops.len()), (1, 1));
        assert_eq!(next_offset(0, raw_len), 2);
        assert_ne!(
            next_offset(0, raw_len),
            next_offset(0, parsed.len()),
            "the parsed count must not be what advances the cursor"
        );
        // Ordinary paging, and the >9,900 window cap arithmetic.
        assert_eq!(next_offset(0, 100), 100);
        assert_eq!(next_offset(9_800, 100), 9_900);
        assert_eq!(next_offset(42, 0), 42);
        // Saturating, so a hostile page length can never wrap the cursor negative.
        assert_eq!(next_offset(i64::MAX, 100), i64::MAX);
    }

    #[test]
    fn chapter_pagination_uses_the_raw_page_length() {
        // REGRESSION: `sync_chapters` paged on `chapters.len()`, the PARSED count. One
        // unparseable chapter in a 100-record page made `page_len` 99, which tripped the
        // `page_len < PAGE_LIMIT` end-of-window test — ending the window early while the
        // cursor still advanced to the cycle's start time, so every later chapter in that
        // window was never mirrored and never re-offered.
        let ok = serde_json::json!({
            "id": "ch-ok",
            "attributes": { "chapter": "1", "translatedLanguage": "en" },
            "relationships": [ { "id": "m-1", "type": "manga" } ],
        });
        let broken = serde_json::json!({
            "id": "ch-broken",
            "attributes": Value::Null, // no attributes => nothing to mirror => dropped
            "relationships": [],
        });
        let mut raw: Vec<Value> = (0..99).map(|_| ok.clone()).collect();
        raw.push(broken);
        let raw_len = raw.len();
        let (chapters, drops) = parse_chapter_page(raw);
        assert_eq!(chapters.len(), 99);
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].0, "ch-broken", "drops stay attributable");

        // The full raw page is a full page: keep going.
        assert_eq!(raw_len as i64, PAGE_LIMIT);
        assert_eq!(classify_page(raw_len, 0, 810_270), PageStep::Process);
        assert_eq!(next_offset(0, raw_len), 100);
        // What the old code did with the same page: 99 < PAGE_LIMIT => "window over".
        assert!(
            (chapters.len() as i64) < PAGE_LIMIT,
            "the parsed count is exactly what used to truncate the window"
        );
        // And a chapter whose relationships are null still parses (null hardening).
        let (chapters, drops) = parse_chapter_page(vec![serde_json::json!({
            "id": "ch-null-rels",
            "attributes": { "chapter": "1", "translatedLanguage": "en" },
            "relationships": Value::Null,
        })]);
        assert!(drops.is_empty());
        assert_eq!(chapter_manga_id(&chapters[0]), None);
    }

    #[test]
    fn a_list_envelope_without_a_usable_total_fails_the_decode() {
        // `total` gates every "is this window exhausted?" decision, so a 0 there ends a
        // window, reports a clean+complete sweep and can latch a permanent marker over an
        // unwalked catalogue. Both ways of arriving at 0 must therefore fail the body:
        // an explicit null AND an absent key (which `#[serde(default)]` used to swallow).
        let ok: RawList = serde_json::from_value(serde_json::json!({
            "data": [], "limit": 100, "offset": 0, "total": 113_762
        }))
        .expect("a normal envelope decodes");
        assert_eq!(ok.total, 113_762);
        assert!(
            serde_json::from_value::<RawList>(serde_json::json!({ "data": [] })).is_err(),
            "an absent total must fail the body, not decode as 0"
        );
        assert!(
            serde_json::from_value::<RawList>(
                serde_json::json!({ "data": [], "total": Value::Null })
            )
            .is_err(),
            "a null total must fail the body, not decode as 0"
        );
        assert!(
            serde_json::from_value::<RawChapterList>(serde_json::json!({ "data": [] })).is_err(),
            "same rule for the chapter envelope"
        );
        // `data`, by contrast, IS hardened both ways: absent or null decodes as empty,
        // which is handled safely downstream (RetryEmpty → truncated → completion blocked).
        let d: RawList =
            serde_json::from_value(serde_json::json!({ "data": Value::Null, "total": 5 })).unwrap();
        assert!(d.data.is_empty());
        let d: RawList = serde_json::from_value(serde_json::json!({ "total": 5 })).unwrap();
        assert!(d.data.is_empty());
    }

    #[test]
    fn at_home_requires_its_page_list() {
        // A chapter's page list is not a collection envelope: decoding a missing/null
        // `data` as `[]` hands the reader a zero-page chapter that looks like a success.
        let ok: AtHome = serde_json::from_value(serde_json::json!({
            "baseUrl": "https://x.mangadex.network",
            "chapter": { "hash": "h", "data": ["1.png", "2.png"] }
        }))
        .unwrap();
        assert_eq!(ok.chapter.data.len(), 2);
        assert!(serde_json::from_value::<AtHome>(serde_json::json!({
            "baseUrl": "https://x.mangadex.network",
            "chapter": { "hash": "h" }
        }))
        .is_err());
        assert!(serde_json::from_value::<AtHome>(serde_json::json!({
            "baseUrl": "https://x.mangadex.network",
            "chapter": { "hash": "h", "data": Value::Null }
        }))
        .is_err());
    }

    #[test]
    fn drop_logging_is_detailed_then_capped() {
        // The counts that gate completion are exact; only the PER-RECORD log lines are
        // capped, so a recurrence can't emit >100k ERROR lines per boot (24 dirty passes
        // × 4,493 drops) and get the whole stream dropped by the log pipeline.
        assert_eq!(detailed_drop_count(0, 100), 100);
        assert_eq!(detailed_drop_count(150, 100), 50, "partial page budget");
        assert_eq!(detailed_drop_count(MAX_DETAILED_DROP_LOGS, 100), 0);
        assert_eq!(detailed_drop_count(u64::MAX, 100), 0, "no underflow");
        assert_eq!(detailed_drop_count(0, 0), 0);
    }

    #[test]
    fn nsfw_flag_from_content_rating() {
        let mut v = manga_json();
        v["attributes"]["contentRating"] = serde_json::json!("pornographic");
        let m: MdManga = serde_json::from_value(v).unwrap();
        assert!(to_work_input(&m).1.is_nsfw);
    }

    #[test]
    fn to_since_strips_offset() {
        assert_eq!(
            to_since("2018-10-04T22:16:00+00:00").as_deref(),
            Some("2018-10-04T22:16:00")
        );
        assert_eq!(to_since("garbage"), None);
    }

    #[test]
    fn to_since_is_inclusive_at_the_boundary_second() {
        // Records the MEASURED semantics of `createdAtSince` (verified live 2026-07-26):
        // it is INCLUSIVE (`>=`). Querying the earliest record's exact timestamp,
        // `createdAtSince=2018-01-18T00:00:00`, returns that record
        // (c0ee660b-f9f2-45c3-8068-5123ff53f84a, createdAt 2018-01-18T00:00:00+00:00) at
        // offset 0, with the full total (113,760) unchanged.
        //
        // That is what makes the sweep's overlapping window slide LOSSLESS: `to_since`
        // truncates to whole seconds and slides to the LAST record seen, so every record
        // sharing that boundary second is re-offered in the next window (present ids are
        // then skipped, so the overlap is free). `to_since_next_second` is therefore the
        // ONLY lossy path in the slide — it deliberately skips the tail of a second that
        // holds more than WINDOW_OFFSET_CAP records, and is reached only there.
        let earliest = "2018-01-18T00:00:00+00:00";
        assert_eq!(
            to_since(earliest).as_deref(),
            Some("2018-01-18T00:00:00"),
            "the cursor keeps the boundary second, and the API re-includes it"
        );
        // Sub-second components are truncated, never rounded up, so the slide can only
        // ever re-fetch — it can never step over an unseen record.
        assert_eq!(
            to_since("2018-01-18T00:00:00.999+00:00").as_deref(),
            Some("2018-01-18T00:00:00")
        );
        // The lossy sibling, for contrast: +1s EXCLUDES everything in the tied second.
        assert_eq!(
            to_since_next_second(earliest).as_deref(),
            Some("2018-01-18T00:00:01")
        );
        // Minute/hour/day carries are the calendar's, not string arithmetic.
        assert_eq!(
            to_since_next_second("2018-01-18T23:59:59+00:00").as_deref(),
            Some("2018-01-19T00:00:00")
        );
        // A non-UTC offset is normalized through the same formatter as the sweep uses.
        assert_eq!(
            to_since("2018-01-18T09:00:00+09:00").as_deref(),
            Some("2018-01-18T09:00:00")
        );
        assert_eq!(to_since_next_second("garbage"), None);
    }

    #[test]
    fn token_bucket_floors_capacity_at_one() {
        // Normal rates are unchanged...
        assert_eq!(TokenBucket::new(5.0).capacity, 5.0);
        // ...but a sub-1/s rate is floored so `acquire` can accumulate a whole
        // token (otherwise refill caps below 1.0 and it blocks forever).
        assert_eq!(TokenBucket::new(40.0 / 60.0).capacity, 1.0);
        assert_eq!(TokenBucket::new(0.05).capacity, 1.0);
    }

    #[tokio::test]
    async fn sub_one_per_sec_bucket_does_not_hang() {
        // 40/min = 0.67/s. Before the capacity floor this bucket could never reach
        // the 1-token threshold and the very first acquire would block forever.
        let b = TokenBucket::new(40.0 / 60.0);
        tokio::time::timeout(std::time::Duration::from_secs(1), b.acquire())
            .await
            .expect("first acquire must not hang");
    }

    #[test]
    fn at_home_limiter_is_built_from_per_minute_rate() {
        let c = MangaDexClient::new("ua", 5.0, 40.0);
        assert_eq!(c.limiter.refill_per_sec, 5.0);
        assert!((c.athome_limiter.refill_per_sec - 40.0 / 60.0).abs() < 1e-9);
        assert_eq!(c.athome_limiter.capacity, 1.0);
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        assert_eq!(backoff(0), Duration::from_millis(500));
        assert_eq!(backoff(1), Duration::from_millis(1000));
        assert_eq!(backoff(3), Duration::from_millis(4000));
        // Shift is capped so a large attempt index can't overflow / explode.
        assert_eq!(backoff(10), Duration::from_millis(500 << 6));
    }

    #[test]
    fn retry_after_parses_and_clamps() {
        use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("5"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(5)));
        // Absurd values are clamped to 60s.
        h.insert(RETRY_AFTER, HeaderValue::from_static("100000"));
        assert_eq!(retry_after(&h), Some(Duration::from_secs(60)));
        // HTTP-date form (non-integer) and missing header → None (fall back to backoff).
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2099 07:28:00 GMT"),
        );
        assert_eq!(retry_after(&h), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }
}
