//! Regional engines, fetch-only (M9 P5).
//!
//! The owner engine (`SUWAYOMI_URL`) keeps the catalogue, the numeric ids and scanning.
//! A regional engine is a second Suwayomi in another place that only FETCHES: page bytes
//! and covers of the sources an admin routes to it (`source_engine_route`).
//!
//! A regional engine's numeric ids mean nothing outside it. They are found from the
//! source's own keys, which PostgreSQL already stores per owner generation
//! (`engine_manga_key` / `engine_chapter_key`), by the same url lookup the reader's
//! device engine uses (`packages/api/src/local-suwayomi-backend.ts`): `mangas(sourceId,
//! url)`, then `chapters(mangaId, url)`. The answers live in memory with a TTL and are
//! never written to PostgreSQL.
//!
//! A regional engine is reached over a private tunnel only (WireGuard or SSH) and always
//! with HTTP Basic auth: its GraphQL API installs extensions, which is running code.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{bail, Result};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

use crate::database::Database;
use crate::source_budget::{SourceBudgets, SourceUse};
use crate::suwayomi::{EngineGraphqlError, PageFetchError, SuwayomiClient};
use crate::upstream_cache::TtlMemo;

/// `KOMIKA_ENGINES=sg=http://10.8.0.2:4567,eu=http://10.8.0.3:4567`
pub(crate) const ENGINES_ENV: &str = "KOMIKA_ENGINES";
/// The owner engine's id in the admin API. Never a regional id.
pub(crate) const OWNER_ENGINE: &str = "owner";

/// A source's manga lives under the same regional id until that engine's data is
/// replaced, which an operator does by hand; an hour bounds the damage of that.
const MANGA_TTL: Duration = Duration::from_secs(60 * 60);
/// The chapter lookup and its page list are re-read as often as the owner's own page
/// memo, so a chapter the regional engine re-numbers is found again within minutes.
const PAGES_TTL: Duration = Duration::from_secs(5 * 60);
/// A source the regional engine cannot find is not searched again for this long; its
/// reads use the owner engine meanwhile (or answer 503 under a strict route).
const MISSING_TTL: Duration = Duration::from_secs(10 * 60);
/// A page index past the end of a remembered page list reads the list again only when
/// the list is at least this old, so out-of-range requests cannot force a source
/// request each.
const PAGE_LIST_REREAD_FLOOR: Duration = Duration::from_secs(30);
const MEMO_CAP: usize = 4096;
/// Bound on remembered absences; the oldest half goes when it fills.
const MISSING_CAP: usize = 4096;

// ---- configuration --------------------------------------------------------------------

/// One configured regional engine. The credential is kept as the ready header value and
/// is never printed (see the `Debug` impl).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct EngineSpec {
    pub id: String,
    pub base_url: String,
    authorization: String,
}

impl std::fmt::Debug for EngineSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineSpec")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("authorization", &"<redacted>")
            .finish()
    }
}

/// The environment variable that names engine `id`'s Basic-auth file:
/// `sg` → `KOMIKA_ENGINE_SG_AUTH_FILE`, `eu-2` → `KOMIKA_ENGINE_EU_2_AUTH_FILE`.
pub(crate) fn auth_file_env(id: &str) -> String {
    format!(
        "KOMIKA_ENGINE_{}_AUTH_FILE",
        id.to_ascii_uppercase().replace('-', "_")
    )
}

pub(crate) fn valid_engine_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && id != OWNER_ENGINE
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// Read `KOMIKA_ENGINES` and each engine's auth file from the process environment.
pub(crate) fn engines_from_env() -> std::result::Result<Vec<EngineSpec>, String> {
    parse_engines(
        std::env::var(ENGINES_ENV).ok().as_deref(),
        |name| std::env::var(name).ok(),
        |path| std::fs::read_to_string(path),
    )
}

/// Parse the engine list. Every engine needs an auth file holding one `user:password`
/// line; a regional engine without auth is refused rather than reached unauthenticated.
pub(crate) fn parse_engines(
    raw: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
    read_file: impl Fn(&str) -> std::io::Result<String>,
) -> std::result::Result<Vec<EngineSpec>, String> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut engines: Vec<EngineSpec> = Vec::new();
    for (position, entry) in raw
        .split(',')
        .map(str::trim)
        .enumerate()
        .filter(|(_, e)| !e.is_empty())
    {
        let Some((id, url)) = entry.split_once('=') else {
            return Err(format!(
                "{ENGINES_ENV}: entry {} is not `id=url`",
                position + 1
            ));
        };
        let (id, url) = (id.trim(), url.trim());
        if !valid_engine_id(id) {
            return Err(format!(
                "{ENGINES_ENV}: entry {} engine id must be 1-32 of a-z, 0-9 and `-`, and not `{OWNER_ENGINE}`", position + 1
            ));
        }
        if engines.iter().any(|e| e.id == id) {
            return Err(format!("{ENGINES_ENV}: engine `{id}` is listed twice"));
        }
        let base_url = engine_base_url(id, url)?;
        let var = auth_file_env(id);
        let Some(path) = env(&var).filter(|p| !p.trim().is_empty()) else {
            return Err(format!(
                "engine `{id}`: set {var} to a file holding `user:password` (a regional engine needs Basic auth)"
            ));
        };
        let contents =
            read_file(path.trim()).map_err(|e| format!("engine `{id}`: cannot read {var}: {e}"))?;
        let authorization = basic_authorization(&contents)
            .ok_or_else(|| format!("engine `{id}`: {var} must hold one `user:password` line"))?;
        engines.push(EngineSpec {
            id: id.to_string(),
            base_url,
            authorization,
        });
    }
    Ok(engines)
}

fn engine_base_url(id: &str, raw: &str) -> std::result::Result<String, String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("engine `{id}`: invalid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("engine `{id}`: the URL must be http or https"));
    }
    if url.host_str().is_none() {
        return Err(format!("engine `{id}`: the URL has no host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "engine `{id}`: put the credential in {}, not in the URL",
            auth_file_env(id)
        ));
    }
    if url.query().is_some() || url.fragment().is_some() || url.path() != "/" {
        return Err(format!(
            "engine `{id}`: the URL must be the engine's root, without a path or query"
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn basic_authorization(contents: &str) -> Option<String> {
    let line = contents.trim();
    if line.contains('\n') || line.contains('\r') {
        return None;
    }
    let (user, password) = line.split_once(':')?;
    if user.is_empty() || password.is_empty() {
        return None;
    }
    Some(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(line)
    ))
}

// ---- a regional engine and its resolver -----------------------------------------------

/// The source's own identity of one chapter, as the owner stored it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceKeys {
    pub source_id: String,
    pub manga_key: String,
    pub chapter_key: String,
    /// The manga's title: the search word when the regional engine has never seen it.
    pub title: Option<String>,
}

impl SourceKeys {
    fn manga_memo_key(&self) -> String {
        format!("{}\n{}", self.source_id, self.manga_key)
    }
    fn chapter_memo_key(&self) -> String {
        format!(
            "{}\n{}\n{}",
            self.source_id, self.manga_key, self.chapter_key
        )
    }
}

/// Request counts of one regional engine, for the admin health view and the gate.
#[derive(Default)]
pub(crate) struct EngineCounters {
    /// Engine-database lookups (`mangas` / `chapters` by url): no source request.
    pub lookups: AtomicU64,
    /// Requests the engine passes on to the source: search, chapter list, page list.
    pub source_requests: AtomicU64,
    pub page_fetches: AtomicU64,
    pub page_bytes: AtomicU64,
    pub cover_fetches: AtomicU64,
    pub failures: AtomicU64,
    /// Reads of a routed source that went to the owner engine instead.
    pub fallbacks: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct CounterSnapshot {
    pub lookups: u64,
    pub source_requests: u64,
    pub page_fetches: u64,
    pub page_bytes: u64,
    pub cover_fetches: u64,
    pub failures: u64,
    pub fallbacks: u64,
}

impl EngineCounters {
    pub(crate) fn snapshot(&self) -> CounterSnapshot {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        CounterSnapshot {
            lookups: get(&self.lookups),
            source_requests: get(&self.source_requests),
            page_fetches: get(&self.page_fetches),
            page_bytes: get(&self.page_bytes),
            cover_fetches: get(&self.cover_fetches),
            failures: get(&self.failures),
            fallbacks: get(&self.fallbacks),
        }
    }
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// A regional chapter, found by its keys: the engine's own chapter id and its page
/// paths (`/api/v1/manga/{m}/chapter/{i}/page/{p}` in that engine's numbering).
#[derive(Debug, Clone)]
pub(crate) struct RegionalChapter {
    pub chapter_id: i64,
    pub pages: Vec<String>,
    /// The engine's list epoch when `pages` was read. A later chapter-list refresh
    /// may renumber the paths.
    pub epoch: u64,
    /// When `pages` was read.
    pub filled_at: Instant,
}

tokio::task_local! {
    static PREPAID: ();
}

/// Run `work` with its regional source requests already paid for: the caller spent from
/// the source's budget itself (ingest, the cover jobs). Other callers spend per request.
pub(crate) async fn prepaid<F: std::future::Future>(work: F) -> F::Output {
    PREPAID.scope((), work).await
}

/// A regional source request refused because the source's budget is spent: a busy
/// answer, never a fallback to the owner engine.
#[derive(Debug)]
pub(crate) struct RegionalBudgetRefused;

impl std::fmt::Display for RegionalBudgetRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the source's request budget is spent")
    }
}

impl std::error::Error for RegionalBudgetRefused {}

pub(crate) fn is_budget_refusal(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RegionalBudgetRefused>().is_some()
}

/// What a regional page read gives.
#[derive(Debug)]
pub(crate) enum RegionalPage {
    Page(axum::body::Bytes, String),
    /// The engine does not have this manga, chapter or page: use the owner (or 503).
    Missing,
    /// The engine's page pool is full: a retryable 503, like the owner's.
    Busy,
}

pub(crate) struct RegionalEngine {
    pub id: String,
    pub(crate) client: SuwayomiClient,
    manga: TtlMemo<i64>,
    chapters: TtlMemo<RegionalChapter>,
    list_epoch: AtomicU64,
    missing: Mutex<HashMap<String, Instant>>,
    missing_ttl: Duration,
    reread_floor: Duration,
    budgets: std::sync::OnceLock<Arc<SourceBudgets>>,
    pub counters: EngineCounters,
    health: RwLock<Option<EngineHealth>>,
}

impl RegionalEngine {
    pub(crate) fn new(spec: &EngineSpec) -> Self {
        Self::with_ttls(spec, MANGA_TTL, PAGES_TTL, MISSING_TTL)
    }

    pub(crate) fn with_ttls(
        spec: &EngineSpec,
        manga_ttl: Duration,
        pages_ttl: Duration,
        missing_ttl: Duration,
    ) -> Self {
        let authorization = reqwest::header::HeaderValue::from_str(&spec.authorization).ok();
        Self {
            id: spec.id.clone(),
            client: SuwayomiClient::with_authorization(spec.base_url.clone(), authorization),
            manga: TtlMemo::new(manga_ttl, MEMO_CAP, &crate::telemetry::CACHE_REGIONAL_MANGA),
            chapters: TtlMemo::new(pages_ttl, MEMO_CAP, &crate::telemetry::CACHE_REGIONAL_PAGES),
            list_epoch: AtomicU64::new(0),
            missing: Mutex::new(HashMap::new()),
            missing_ttl,
            reread_floor: PAGE_LIST_REREAD_FLOOR.min(pages_ttl),
            budgets: std::sync::OnceLock::new(),
            counters: EngineCounters::default(),
            health: RwLock::new(None),
        }
    }

    /// Forget every resolved id (a route changed, or the engine's data was replaced).
    pub(crate) fn clear(&self) {
        self.manga.clear();
        self.chapters.clear();
        self.missing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Forget what was resolved for one chapter and its manga, after a read that the
    /// resolved ids no longer answer.
    pub(crate) fn forget(&self, keys: &SourceKeys) {
        self.forget_manga(keys);
        self.chapters.forget(&keys.chapter_memo_key());
    }

    pub(crate) fn forget_manga(&self, keys: &SourceKeys) {
        self.manga.forget(&keys.manga_memo_key());
    }

    fn known_missing(&self, key: &str) -> bool {
        let mut missing = self.missing.lock().unwrap_or_else(|e| e.into_inner());
        match missing.get(key) {
            Some(at) if at.elapsed() < self.missing_ttl => true,
            Some(_) => {
                missing.remove(key);
                false
            }
            None => false,
        }
    }

    fn unmark_missing(&self, key: &str) {
        self.missing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    /// Take one request from the source's budget before a request the engine passes on
    /// to the source. Free inside [`prepaid`] and before budgets are set. A reader is not
    /// paced like a background job: the request goes at once while the source's rolling
    /// window has room, and is refused (busy) at once when it does not.
    async fn admit(&self, source_id: &str) -> Result<()> {
        if PREPAID.try_with(|_| ()).is_ok() {
            return Ok(());
        }
        let Some(budgets) = self.budgets.get() else {
            return Ok(());
        };
        if budgets.try_spend_burst(source_id, SourceUse::Regional) {
            Ok(())
        } else {
            Err(RegionalBudgetRefused.into())
        }
    }

    fn remember_missing(&self, key: String) {
        let mut missing = self.missing.lock().unwrap_or_else(|e| e.into_inner());
        if missing.len() >= MISSING_CAP {
            let ttl = self.missing_ttl;
            missing.retain(|_, at| at.elapsed() < ttl);
            if missing.len() >= MISSING_CAP {
                let mut ages: Vec<Instant> = missing.values().copied().collect();
                ages.sort();
                let cut = ages[ages.len() / 2];
                missing.retain(|_, at| *at > cut);
            }
        }
        missing.insert(key, Instant::now());
    }

    /// The regional engine's manga id for `(source_id, manga_key)`. Its own database
    /// first; on a miss, one source search by title, matched by url (which also makes
    /// the engine store the manga). `None` when neither finds it.
    pub(crate) async fn manga_id(&self, keys: &SourceKeys) -> Result<Option<i64>> {
        let key = keys.manga_memo_key();
        if self.known_missing(&key) {
            return Ok(None);
        }
        // Absence is remembered before the memo's initializer is released, so readers of
        // the same manga queued behind a lookup that found nothing take that answer
        // instead of each spending a source search.
        self.manga
            .get_or_try_init_optional(&key, || async {
                if self.known_missing(&key) {
                    return Ok(None);
                }
                let found = self.lookup_manga(keys).await?;
                if found.is_none() {
                    self.remember_missing(key.clone());
                }
                Ok(found)
            })
            .await
    }

    async fn lookup_manga(&self, keys: &SourceKeys) -> Result<Option<i64>> {
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
        EngineCounters::bump(&self.counters.lookups);
        let known: Data = self
            .client
            .gql(
                "query RegionalManga($sid: LongString!, $url: String!) { \
                 mangas(condition: { sourceId: $sid, url: $url }) { nodes { id } } }",
                json!({ "sid": keys.source_id, "url": keys.manga_key }),
            )
            .await?;
        if let Some(node) = known.mangas.nodes.first() {
            return Ok(Some(node.id));
        }
        let Some(title) = keys.title.as_deref().filter(|t| !t.trim().is_empty()) else {
            return Ok(None);
        };
        #[derive(Deserialize)]
        struct Found {
            id: i64,
            url: String,
        }
        #[derive(Deserialize)]
        struct Payload {
            mangas: Vec<Found>,
        }
        #[derive(Deserialize)]
        struct Search {
            #[serde(rename = "fetchSourceManga")]
            fetch_source_manga: Payload,
        }
        self.admit(&keys.source_id).await?;
        EngineCounters::bump(&self.counters.source_requests);
        let searched: Search = self
            .client
            .gql(
                "mutation RegionalSearch($source: LongString!, $query: String) { \
                 fetchSourceManga(input: { source: $source, type: SEARCH, page: 1, query: $query }) { \
                 mangas { id url } } }",
                json!({ "source": keys.source_id, "query": title }),
            )
            .await?;
        Ok(searched
            .fetch_source_manga
            .mangas
            .into_iter()
            .find(|m| m.url == keys.manga_key)
            .map(|m| m.id))
    }

    /// The regional chapter for `keys` and its page paths. The chapter is looked up by
    /// url every time the page list is (re)read, so a re-numbered chapter is found again
    /// within one page-list TTL.
    pub(crate) async fn chapter(&self, keys: &SourceKeys) -> Result<Option<RegionalChapter>> {
        let key = keys.chapter_memo_key();
        if self.known_missing(&key) {
            return Ok(None);
        }
        let Some(manga_id) = self.manga_id(keys).await? else {
            return Ok(None);
        };
        // As for the manga: queued readers of one absent chapter share one lookup.
        self.chapters
            .get_or_try_init_optional(&key, || async {
                if self.known_missing(&key) {
                    return Ok(None);
                }
                let found = self.lookup_chapter(manga_id, keys).await?;
                if found.is_none() {
                    self.remember_missing(key.clone());
                }
                Ok(found)
            })
            .await
    }

    pub(crate) fn chapter_cached(&self, keys: &SourceKeys) -> bool {
        self.chapters.has_value(&keys.chapter_memo_key())
            || self.known_missing(&keys.chapter_memo_key())
            || self.known_missing(&keys.manga_memo_key())
    }

    pub(crate) async fn fresh_chapter(&self, keys: &SourceKeys) -> Result<Option<RegionalChapter>> {
        self.chapters.forget(&keys.chapter_memo_key());
        self.chapter(keys).await
    }

    async fn lookup_chapter(
        &self,
        manga_id: i64,
        keys: &SourceKeys,
    ) -> Result<Option<RegionalChapter>> {
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
            chapters: Nodes,
        }
        EngineCounters::bump(&self.counters.lookups);
        let known: Data = self
            .client
            .gql(
                "query RegionalChapter($mid: Int!, $url: String!) { \
                 chapters(condition: { mangaId: $mid, url: $url }) { nodes { id } } }",
                json!({ "mid": manga_id, "url": keys.chapter_key }),
            )
            .await?;
        let chapter_id = match known.chapters.nodes.first() {
            Some(node) => node.id,
            None => {
                // Not in the engine's database yet: one chapter-list request to the source,
                // which is also how a chapter newer than the engine's last list is found.
                #[derive(Deserialize)]
                struct Listed {
                    id: i64,
                    url: String,
                }
                #[derive(Deserialize)]
                struct Payload {
                    chapters: Vec<Listed>,
                }
                #[derive(Deserialize)]
                struct Fetched {
                    #[serde(rename = "fetchMangaAndChapters")]
                    fetch_manga_and_chapters: Payload,
                }
                self.admit(&keys.source_id).await?;
                EngineCounters::bump(&self.counters.source_requests);
                self.list_epoch.fetch_add(1, Ordering::AcqRel);
                let fetched = self
                    .client
                    .gql(
                        "mutation RegionalChapters($id: Int!) { \
                         fetchMangaAndChapters(input: { id: $id, fetchManga: false, fetchChapters: true }) { \
                         chapters { id url } } }",
                        json!({ "id": manga_id }),
                    )
                    .await;
                self.list_epoch.fetch_add(1, Ordering::AcqRel);
                // A list refresh can move every sibling's page paths.
                self.chapters.clear();
                let fetched: Fetched = fetched?;
                match fetched
                    .fetch_manga_and_chapters
                    .chapters
                    .into_iter()
                    .find(|c| c.url == keys.chapter_key)
                {
                    Some(chapter) => chapter.id,
                    None => return Ok(None),
                }
            }
        };
        #[derive(Deserialize)]
        struct Pages {
            pages: Vec<String>,
        }
        #[derive(Deserialize)]
        struct PageData {
            #[serde(rename = "fetchChapterPages")]
            fetch_chapter_pages: Pages,
        }
        self.admit(&keys.source_id).await?;
        let epoch = self.list_epoch.load(Ordering::Acquire);
        let filled_at = Instant::now();
        EngineCounters::bump(&self.counters.source_requests);
        let listed: PageData = self
            .client
            .gql(
                "mutation RegionalPages($id: Int!) { \
                 fetchChapterPages(input: { chapterId: $id }) { pages } }",
                json!({ "id": chapter_id }),
            )
            .await?;
        let pages = listed
            .fetch_chapter_pages
            .pages
            .iter()
            .map(|url| page_path(url))
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(RegionalChapter {
            chapter_id,
            pages,
            epoch,
            filled_at,
        }))
    }

    /// Page `index` of the chapter `keys` names, from this engine. One retry with fresh
    /// lookups when the resolved ids stop answering; `Missing` when the engine does not
    /// have the chapter or the page; `Busy` when the source's budget is spent.
    pub(crate) async fn fetch_page(&self, keys: &SourceKeys, index: usize) -> Result<RegionalPage> {
        for attempt in 0..2 {
            let remembered_manga = self.manga.has_value(&keys.manga_memo_key())
                && !self.known_missing(&keys.manga_memo_key())
                && !self.known_missing(&keys.chapter_memo_key());
            let chapter = match self.chapter(keys).await {
                Ok(Some(chapter)) => chapter,
                Err(error) if is_budget_refusal(&error) => return Ok(RegionalPage::Busy),
                result => {
                    // A manga id remembered from before the engine's data was replaced
                    // finds nothing: look the manga up again once, instead of failing
                    // until the manga memo expires. Only a "not found" answer (no chapter,
                    // or the engine's own GraphQL error) says the id may be stale; a
                    // transport error or timeout would only be waited out twice.
                    let stale = match &result {
                        Ok(_) => true,
                        Err(error) => error.downcast_ref::<EngineGraphqlError>().is_some(),
                    };
                    if attempt == 0 && remembered_manga && stale {
                        self.forget(keys);
                        self.unmark_missing(&keys.chapter_memo_key());
                        continue;
                    }
                    return result.map(|_| RegionalPage::Missing);
                }
            };
            let Some(path) = chapter.pages.get(index) else {
                // The page list may be older than the source's: read it again once, and
                // only when it is not fresh, so a bad index is not a source request.
                if attempt == 0 && chapter.filled_at.elapsed() >= self.reread_floor {
                    self.chapters.forget(&keys.chapter_memo_key());
                    continue;
                }
                return Ok(RegionalPage::Missing);
            };
            match self.client.fetch_page_now(path).await {
                Ok((bytes, mime)) => {
                    // A concurrent list refresh can make this path serve another chapter.
                    if self.list_epoch.load(Ordering::Acquire) != chapter.epoch {
                        self.chapters.forget(&keys.chapter_memo_key());
                        if attempt == 0 {
                            continue;
                        }
                        return Ok(RegionalPage::Missing);
                    }
                    EngineCounters::bump(&self.counters.page_fetches);
                    self.counters
                        .page_bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    return Ok(RegionalPage::Page(bytes, mime));
                }
                Err(PageFetchError::Busy) => return Ok(RegionalPage::Busy),
                Err(PageFetchError::Upstream(error)) => {
                    EngineCounters::bump(&self.counters.failures);
                    if attempt == 0 {
                        self.forget(keys);
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Ok(RegionalPage::Missing)
    }

    pub(crate) fn health(&self) -> Option<EngineHealth> {
        self.health
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Ask the engine which extensions it has installed. An unreachable engine is a
    /// health answer, not an error.
    pub(crate) async fn probe(&self) -> EngineHealth {
        let health = match self.client.fetch_extensions().await {
            Ok(extensions) => {
                let mut installed = HashMap::new();
                for extension in extensions {
                    for source in &extension.source_ids {
                        installed.insert(
                            source.clone(),
                            InstalledExtension {
                                pkg_name: extension.pkg_name.clone(),
                                version_code: extension.version_code,
                            },
                        );
                    }
                }
                EngineHealth {
                    reachable: true,
                    checked_at: chrono::Utc::now(),
                    error: None,
                    installed,
                }
            }
            Err(error) => EngineHealth {
                reachable: false,
                checked_at: chrono::Utc::now(),
                error: Some(short_error(&error)),
                installed: HashMap::new(),
            },
        };
        *self.health.write().unwrap_or_else(|e| e.into_inner()) = Some(health.clone());
        health
    }
}

/// A page url as the engine lists it, reduced to the path this client fetches. Only the
/// engine's own page route is accepted.
fn page_path(url: &str) -> Result<String> {
    let path = if url.starts_with('/') {
        url.to_string()
    } else {
        let parsed = reqwest::Url::parse(url)?;
        match parsed.query() {
            Some(query) => format!("{}?{query}", parsed.path()),
            None => parsed.path().to_string(),
        }
    };
    if !path.starts_with("/api/v1/manga/") || path.contains("..") {
        bail!("regional engine listed a page outside its page route");
    }
    Ok(path)
}

/// One line, no credential: an error text for the admin view.
fn short_error(error: &anyhow::Error) -> String {
    let text = error.to_string();
    let line = text.lines().next().unwrap_or_default();
    line.chars().take(200).collect()
}

// ---- health and extension versions ----------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstalledExtension {
    pub pkg_name: String,
    pub version_code: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) struct EngineHealth {
    pub reachable: bool,
    pub checked_at: chrono::DateTime<chrono::Utc>,
    pub error: Option<String>,
    /// Installed extension per source id.
    pub installed: HashMap<String, InstalledExtension>,
}

/// Whether a regional engine runs the owner's extension for a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VersionCheck {
    Match,
    /// The owner records no extension for the source (nothing to compare against).
    OwnerUnknown,
    /// The regional engine has no extension for the source.
    NotInstalled,
    Differs {
        owner: InstalledExtension,
        regional: InstalledExtension,
    },
}

pub(crate) fn compare_versions(
    owner: Option<&InstalledExtension>,
    regional: Option<&InstalledExtension>,
) -> VersionCheck {
    match (owner, regional) {
        (None, _) => VersionCheck::OwnerUnknown,
        (Some(_), None) => VersionCheck::NotInstalled,
        (Some(owner), Some(regional))
            if owner.pkg_name == regional.pkg_name
                && owner.version_code.is_some()
                && owner.version_code == regional.version_code =>
        {
            VersionCheck::Match
        }
        (Some(owner), Some(regional)) => VersionCheck::Differs {
            owner: owner.clone(),
            regional: regional.clone(),
        },
    }
}

/// The extension the owner recorded for a source (`source_extension`).
pub(crate) async fn owner_extension(
    pool: &Database,
    source_id: &str,
) -> Result<Option<InstalledExtension>> {
    let row: Option<(String, Option<i64>)> =
        sqlx::query_as("SELECT pkg_name, version_code FROM source_extension WHERE source_id=$1")
            .bind(source_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(pkg_name, version_code)| InstalledExtension {
        pkg_name,
        version_code,
    }))
}

// ---- routes ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Route {
    pub engine_id: String,
    pub strict: bool,
    /// The owner's extension for the source, read with the route.
    pub owner_extension: Option<InstalledExtension>,
}

/// `source_id, engine_id, strict, pkg_name, version_code`.
type RouteRow = (String, String, bool, Option<String>, Option<i64>);

pub(crate) async fn load_routes(pool: &Database) -> Result<HashMap<String, Route>> {
    let rows: Vec<RouteRow> = sqlx::query_as(
        "SELECT r.source_id, r.engine_id, r.strict, e.pkg_name, e.version_code \
         FROM source_engine_route r LEFT JOIN source_extension e ON e.source_id=r.source_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(source_id, engine_id, strict, pkg_name, version_code)| {
            (
                source_id,
                Route {
                    engine_id,
                    strict,
                    owner_extension: pkg_name.map(|pkg_name| InstalledExtension {
                        pkg_name,
                        version_code,
                    }),
                },
            )
        })
        .collect())
}

/// Write or remove a source's route. Checks (engine known, versions equal) belong to
/// the caller; this only stores.
pub(crate) async fn store_route(
    pool: &Database,
    source_id: &str,
    route: Option<(&str, bool)>,
    set_by: Option<&str>,
) -> Result<()> {
    match route {
        Some((engine_id, strict)) => {
            if !valid_engine_id(engine_id) {
                bail!("invalid engine id");
            }
            sqlx::query(
                "INSERT INTO source_engine_route (source_id, engine_id, strict, set_at, set_by) \
                 VALUES ($1, $2, $3, now(), $4) \
                 ON CONFLICT (source_id) DO UPDATE SET engine_id=EXCLUDED.engine_id, \
                 strict=EXCLUDED.strict, set_at=EXCLUDED.set_at, set_by=EXCLUDED.set_by",
            )
            .bind(source_id)
            .bind(engine_id)
            .bind(strict)
            .bind(set_by)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM source_engine_route WHERE source_id=$1")
                .bind(source_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

// ---- the owner's stored keys ----------------------------------------------------------

/// The source keys of an owner chapter id, from the active generation's bindings.
pub(crate) async fn keys_for_chapter(
    pool: &Database,
    chapter_id: i64,
) -> Result<Option<SourceKeys>> {
    let row: Option<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT m.source_id, m.manga_key, c.chapter_key, m.title FROM engine_chapter_key c \
         JOIN engine_manga_key m ON m.generation=c.generation AND m.manga_id=c.manga_id \
         JOIN engine_generation g ON g.id=c.generation \
         WHERE g.state='active' AND c.chapter_id=$1",
    )
    .bind(chapter_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(keys_from_row))
}

/// The source keys of an owner manga id and a chapter position (`sourceOrder`): the
/// legacy page path `/api/v1/manga/{manga}/chapter/{order}/page/{page}`.
pub(crate) async fn keys_for_order(
    pool: &Database,
    manga_id: i64,
    source_order: i32,
) -> Result<Option<SourceKeys>> {
    let row: Option<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT m.source_id, m.manga_key, c.chapter_key, m.title FROM engine_chapter_key c \
         JOIN engine_manga_key m ON m.generation=c.generation AND m.manga_id=c.manga_id \
         JOIN engine_generation g ON g.id=c.generation \
         WHERE g.state='active' AND c.manga_id=$1 AND c.source_order=$2 \
         ORDER BY c.chapter_id LIMIT 1",
    )
    .bind(manga_id)
    .bind(source_order)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(keys_from_row))
}

/// The source id, manga key and title of an owner manga id (covers).
pub(crate) async fn keys_for_manga(
    pool: &Database,
    manga_id: i64,
) -> Result<Option<(String, String, Option<String>)>> {
    Ok(sqlx::query_as(
        "SELECT m.source_id, m.manga_key, m.title FROM engine_manga_key m \
         JOIN engine_generation g ON g.id=m.generation \
         WHERE g.state='active' AND m.manga_id=$1",
    )
    .bind(manga_id)
    .fetch_optional(pool)
    .await?)
}

fn keys_from_row(row: (String, String, String, Option<String>)) -> SourceKeys {
    let (source_id, manga_key, chapter_key, title) = row;
    SourceKeys {
        source_id,
        manga_key,
        chapter_key,
        title,
    }
}

#[derive(Debug)]
pub(crate) enum Routed<T> {
    Owner,
    Served(T),
    Busy,
    Refuse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PageTarget {
    Chapter(i64),
    Order { manga_id: i64, source_order: i32 },
}

type OwnerKeys = HashMap<(Option<uuid::Uuid>, PageTarget), (Instant, SourceKeys)>;

async fn page_keys(
    owner: &SuwayomiClient,
    pool: &Database,
    target: PageTarget,
) -> Result<Option<SourceKeys>> {
    let chapter_id = match target {
        PageTarget::Chapter(id) => id,
        PageTarget::Order {
            manga_id,
            source_order,
        } => {
            return keys_for_order(pool, manga_id, source_order).await;
        }
    };
    let key = (owner.read_generation().0, target);
    {
        let memo = owner
            .regional
            .owner_keys
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((at, keys)) = memo.get(&key) {
            if at.elapsed() < Duration::from_secs(60) {
                return Ok(Some(keys.clone()));
            }
        }
    }
    let found = keys_for_chapter(pool, chapter_id).await?;
    if let Some(keys) = &found {
        let mut memo = owner
            .regional
            .owner_keys
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if memo.len() >= MEMO_CAP {
            memo.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(60));
            if memo.len() >= MEMO_CAP {
                memo.clear();
            }
        }
        memo.insert(key, (Instant::now(), keys.clone()));
    }
    Ok(found)
}

pub(crate) fn regional_pages_cached(owner: &SuwayomiClient, chapter_id: i64) -> bool {
    let key = (owner.read_generation().0, PageTarget::Chapter(chapter_id));
    let keys = {
        let memo = owner
            .regional
            .owner_keys
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match memo.get(&key) {
            Some((at, keys)) if at.elapsed() < Duration::from_secs(60) => keys.clone(),
            _ => return false,
        }
    };
    match owner.regional.decide(&keys.source_id) {
        Decision::Regional { engine, .. } => {
            // A remembered absence may fall back to a cold owner list.
            engine.chapter_cached(&keys) && engine.chapters.has_value(&keys.chapter_memo_key())
        }
        _ => false,
    }
}

pub(crate) fn unavailable<T>(
    engines: &Engines,
    source_id: &str,
    engine: &RegionalEngine,
    strict: bool,
) -> Routed<T> {
    if strict || engines.route(source_id).is_some_and(|route| route.strict) {
        Routed::Refuse
    } else {
        EngineCounters::bump(&engine.counters.fallbacks);
        Routed::Owner
    }
}

pub(crate) async fn route_page(
    owner: &SuwayomiClient,
    pool: &Database,
    target: PageTarget,
    index: usize,
) -> Routed<(axum::body::Bytes, String)> {
    if !owner.regional.any_route() {
        return Routed::Owner;
    }
    let keys = match page_keys(owner, pool, target).await {
        Ok(Some(keys)) => keys,
        Ok(None) => return Routed::Owner,
        Err(error) => {
            tracing::warn!(error = %error, "regional page keys unreadable; using owner");
            return Routed::Owner;
        }
    };
    match owner.regional.decide_read(&keys.source_id) {
        Decision::Owner => Routed::Owner,
        Decision::Refuse => Routed::Refuse,
        Decision::Regional { engine, strict } => match engine.fetch_page(&keys, index).await {
            Ok(RegionalPage::Page(bytes, mime)) => Routed::Served((bytes, mime)),
            Ok(RegionalPage::Busy) => Routed::Busy,
            Ok(RegionalPage::Missing) => {
                unavailable(&owner.regional, &keys.source_id, &engine, strict)
            }
            Err(_) => {
                tracing::warn!(engine = %engine.id, "regional page failed");
                unavailable(&owner.regional, &keys.source_id, &engine, strict)
            }
        },
    }
}

/// Regional page counts, with URLs in the owner's numbering only.
pub(crate) async fn page_urls(
    owner: &SuwayomiClient,
    pool: &Database,
    chapter_id: i64,
) -> Result<Option<Vec<String>>> {
    if !owner.regional.any_route() {
        return Ok(None);
    }
    let keys = match page_keys(owner, pool, PageTarget::Chapter(chapter_id)).await {
        Ok(Some(keys)) => keys,
        Ok(None) => return Ok(None),
        Err(error) => {
            tracing::warn!(error = %error, "regional page keys unreadable; using owner");
            return Ok(None);
        }
    };
    let (engine, strict) = match owner.regional.decide_read(&keys.source_id) {
        Decision::Owner => return Ok(None),
        Decision::Refuse => bail!("regional engine unavailable"),
        Decision::Regional { engine, strict } => (engine, strict),
    };
    let chapter = match engine.chapter(&keys).await {
        Ok(Some(chapter)) => chapter,
        Err(error) if is_budget_refusal(&error) => {
            return Err(crate::request_budget::UpstreamBusy.into());
        }
        result => {
            if result.is_err() {
                tracing::warn!(engine = %engine.id, "regional page list failed");
            }
            if matches!(
                unavailable::<()>(&owner.regional, &keys.source_id, &engine, strict),
                Routed::Refuse
            ) {
                bail!("regional engine unavailable");
            }
            return Ok(None);
        }
    };
    let position: Option<(i64, i32)> = sqlx::query_as(
        "SELECT c.manga_id, c.source_order FROM engine_chapter_key c \
         JOIN engine_generation g ON g.id=c.generation \
         WHERE g.state='active' AND c.chapter_id=$1",
    )
    .bind(chapter_id)
    .fetch_optional(pool)
    .await?;
    let Some((manga, order)) = position else {
        bail!("owner chapter binding unavailable");
    };
    Ok(Some(
        (0..chapter.pages.len())
            .map(|i| {
                owner.page_locator_url(chapter_id, i).unwrap_or_else(|| {
                    owner.abs(Some(&format!(
                        "/api/v1/manga/{manga}/chapter/{order}/page/{i}"
                    )))
                })
            })
            .collect(),
    ))
}

pub(crate) enum CoverTarget {
    OwnerManga(i64),
    Keys {
        source_id: String,
        manga_key: String,
        title: Option<String>,
    },
}

pub(crate) async fn route_cover(
    owner: &SuwayomiClient,
    pool: &Database,
    target: CoverTarget,
    background: bool,
) -> Routed<(Vec<u8>, String)> {
    if !owner.regional.any_route() {
        return Routed::Owner;
    }
    let (source_id, manga_key, title) = match target {
        CoverTarget::Keys {
            source_id,
            manga_key,
            title,
        } if !manga_key.is_empty() => (source_id, manga_key, title),
        CoverTarget::Keys { .. } => return Routed::Owner,
        CoverTarget::OwnerManga(id) => match keys_for_manga(pool, id).await {
            Ok(Some(keys)) => keys,
            Ok(None) => return Routed::Owner,
            Err(error) => {
                tracing::warn!(error = %error, "regional cover keys unreadable; using owner");
                return Routed::Owner;
            }
        },
    };
    let (engine, strict) = match owner.regional.decide_read(&source_id) {
        Decision::Owner => return Routed::Owner,
        Decision::Refuse => return Routed::Refuse,
        Decision::Regional { engine, strict } => (engine, strict),
    };
    let keys = SourceKeys {
        source_id,
        manga_key,
        chapter_key: String::new(),
        title,
    };
    engine.forget_manga(&keys);
    let id = match engine.manga_id(&keys).await {
        Ok(Some(id)) => id,
        Err(error) if is_budget_refusal(&error) => return Routed::Busy,
        _ => return unavailable(&owner.regional, &keys.source_id, &engine, strict),
    };
    let path = format!("/api/v1/manga/{id}/thumbnail");
    EngineCounters::bump(&engine.counters.cover_fetches);
    let fetched = if background {
        engine.client.fetch_cover_background(&path).await
    } else {
        match engine.client.fetch_cover_now(&path).await {
            Ok(cover) => Ok(cover),
            Err(crate::suwayomi::CoverFetchError::Busy) => return Routed::Busy,
            Err(crate::suwayomi::CoverFetchError::Upstream(error)) => Err(error),
        }
    };
    match fetched {
        Ok(cover) => Routed::Served(cover),
        Err(_) => {
            EngineCounters::bump(&engine.counters.failures);
            tracing::warn!(engine = %engine.id, "regional cover failed");
            unavailable(&owner.regional, &keys.source_id, &engine, strict)
        }
    }
}

/// The thumbnail path's owner manga id, including absolute thumbnail URLs.
pub(crate) fn cover_manga_id(url: &str) -> Option<i64> {
    let parsed = reqwest::Url::parse(url).ok();
    let path = parsed.as_ref().map_or(url, |url| url.path());
    let id = path
        .strip_prefix("/api/v1/manga/")?
        .strip_suffix("/thumbnail")?;
    id.parse().ok()
}

pub(crate) async fn cover_bytes(
    owner: &SuwayomiClient,
    pool: &Database,
    target: CoverTarget,
    thumbnail: Option<&str>,
) -> Option<Vec<u8>> {
    owner.wait_for_generation().await.ok()?;
    match route_cover(owner, pool, target, true).await {
        Routed::Served((bytes, _)) => {
            crate::avatar::ensure_complete(&bytes).ok()?;
            Some(bytes)
        }
        Routed::Owner => owner.cover_bytes(thumbnail).await,
        Routed::Busy | Routed::Refuse => None,
    }
}

// ---- provisioning ---------------------------------------------------------------------

/// What `engine provision` did for one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Provisioned {
    pub source_id: String,
    pub pkg_name: Option<String>,
    pub check: VersionCheck,
    /// What was done or why nothing could be done, in a few words.
    pub note: String,
}

/// The owner's extension and the repository it came from.
async fn owner_extension_with_repo(
    pool: &Database,
    source_id: &str,
) -> Result<Option<(InstalledExtension, String)>> {
    let row: Option<(String, Option<i64>, String)> = sqlx::query_as(
        "SELECT pkg_name, version_code, repo_url FROM source_extension WHERE source_id=$1",
    )
    .bind(source_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(pkg_name, version_code, repo)| {
        (
            InstalledExtension {
                pkg_name,
                version_code,
            },
            repo,
        )
    }))
}

/// Install on `engine` the extension the owner runs for each source: add its repository,
/// refresh the list, install (or update an older install). The engine installs what the
/// repository has now and cannot pick an older version, so a source whose versions still
/// differ afterwards is reported and stays unroutable.
pub(crate) async fn provision(
    engine: &RegionalEngine,
    pool: &Database,
    sources: &[String],
) -> Result<Vec<Provisioned>> {
    let mut out = Vec::new();
    let mut refreshed_repos: Vec<String> = Vec::new();
    for source_id in sources {
        let Some((owner, repo)) = owner_extension_with_repo(pool, source_id).await? else {
            out.push(Provisioned {
                source_id: source_id.clone(),
                pkg_name: None,
                check: VersionCheck::OwnerUnknown,
                note: "the owner engine records no extension for this source".into(),
            });
            continue;
        };
        if !refreshed_repos.contains(&repo) {
            engine.client.add_extension_store(&repo).await?;
            engine.client.refresh_extensions().await?;
            refreshed_repos.push(repo.clone());
        }
        let installed = engine.probe().await;
        if !installed.reachable {
            bail!(
                "engine `{}` is not reachable: {}",
                engine.id,
                installed.error.unwrap_or_default()
            );
        }
        let before = installed.installed.get(source_id).cloned();
        let note = match &before {
            None => {
                engine.client.install_extension(&owner.pkg_name).await?;
                "installed"
            }
            Some(regional)
                if compare_versions(Some(&owner), Some(regional)) == VersionCheck::Match =>
            {
                "already the same version"
            }
            Some(regional)
                if regional.pkg_name == owner.pkg_name
                    && regional.version_code < owner.version_code =>
            {
                engine.client.update_extension(&owner.pkg_name).await?;
                "updated"
            }
            Some(_) => "left as it is",
        };
        let after = engine.probe().await;
        let check = compare_versions(Some(&owner), after.installed.get(source_id));
        let note = match &check {
            VersionCheck::Match => note.to_string(),
            VersionCheck::Differs { regional, .. }
                if regional.pkg_name == owner.pkg_name
                    && regional.version_code > owner.version_code =>
            {
                format!("{note}; the regional engine is newer: update the owner engine")
            }
            _ => format!("{note}; versions still differ, the route stays off"),
        };
        out.push(Provisioned {
            source_id: source_id.clone(),
            pkg_name: Some(owner.pkg_name),
            check,
            note,
        });
    }
    Ok(out)
}

/// Why a source cannot be routed to `engine_id` now, or `None` when it can.
pub(crate) async fn route_refusal(
    engines: &Engines,
    pool: &Database,
    source_id: &str,
    engine_id: &str,
) -> Result<Option<String>> {
    let Some(engine) = engines.engine(engine_id) else {
        return Ok(Some(format!(
            "engine `{engine_id}` is not configured on this server"
        )));
    };
    let health = engine.probe().await;
    if !health.reachable {
        return Ok(Some(format!("engine `{engine_id}` is not reachable")));
    }
    let owner = owner_extension(pool, source_id).await?;
    Ok(match compare_versions(owner.as_ref(), health.installed.get(source_id)) {
        VersionCheck::Match => None,
        VersionCheck::OwnerUnknown => {
            Some("the owner engine records no extension version for this source".into())
        }
        VersionCheck::NotInstalled => Some(format!(
            "engine `{engine_id}` has no extension for this source: run `komika-server engine provision {engine_id} {source_id}`"
        )),
        VersionCheck::Differs { owner, regional } => Some(format!(
            "extension versions differ (owner {} {}, engine `{engine_id}` {} {})",
            owner.pkg_name,
            version_text(owner.version_code),
            regional.pkg_name,
            version_text(regional.version_code)
        )),
    })
}

fn version_text(version: Option<i64>) -> String {
    version.map_or_else(|| "unknown".into(), |v| v.to_string())
}

/// After the owner engine updated `pkg_name`, update it on every regional engine that
/// has it installed. Routes of its sources stay off until the versions match again,
/// because `decide` compares them; the probe here makes that immediate.
pub(crate) async fn update_regional(engines: &Engines, pkg_name: &str) {
    for engine in engines.engines() {
        let has_it = engine
            .health()
            .is_some_and(|h| h.installed.values().any(|e| e.pkg_name == pkg_name));
        if !has_it {
            continue;
        }
        if let Err(error) = engine.client.refresh_extensions().await {
            tracing::warn!(engine = %engine.id, error = %short_error(&error), "regional extension index refresh failed");
        }
        if let Err(error) = engine.client.update_extension(pkg_name).await {
            tracing::warn!(engine = %engine.id, pkg = %pkg_name, error = %short_error(&error), "regional extension update failed");
        }
        engine.probe().await;
    }
}

pub(crate) async fn reconcile_versions(engines: &Engines) {
    let mut attempted = std::collections::HashSet::new();
    let mut refreshed = std::collections::HashSet::new();
    for route in engines.routes().values() {
        let Some(engine) = engines.engine(&route.engine_id) else {
            continue;
        };
        let Some(owner) = &route.owner_extension else {
            continue;
        };
        let Some(health) = engine.health() else {
            continue;
        };
        let Some(regional) = health
            .installed
            .values()
            .find(|e| e.pkg_name == owner.pkg_name)
        else {
            continue;
        };
        if !matches!((regional.version_code, owner.version_code), (Some(a), Some(b)) if a < b)
            || !attempted.insert((engine.id.clone(), owner.pkg_name.clone()))
        {
            continue;
        }
        if refreshed.insert(engine.id.clone()) {
            if let Err(error) = engine.client.refresh_extensions().await {
                tracing::warn!(engine = %engine.id, error = %short_error(&error), "regional extension index refresh failed");
            }
        }
        if let Err(error) = engine.client.update_extension(&owner.pkg_name).await {
            tracing::warn!(engine = %engine.id, pkg = %owner.pkg_name, error = %short_error(&error), "regional extension retry failed");
        }
        engine.probe().await;
    }
}

// ---- the registry ---------------------------------------------------------------------

/// Where a routed read goes.
#[derive(Clone)]
pub(crate) enum Decision {
    Owner,
    Regional {
        engine: Arc<RegionalEngine>,
        strict: bool,
    },
    /// Strict route, engine cannot serve: answer 503.
    Refuse,
}

impl std::fmt::Debug for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Decision::Owner => f.write_str("Owner"),
            Decision::Regional { engine, strict } => {
                write!(f, "Regional({}, strict={strict})", engine.id)
            }
            Decision::Refuse => f.write_str("Refuse"),
        }
    }
}

/// Every configured regional engine and the routes, held by the owner client.
#[derive(Default)]
pub(crate) struct Engines {
    regional: HashMap<String, Arc<RegionalEngine>>,
    routes: RwLock<Arc<HashMap<String, Route>>>,
    reload: tokio::sync::Mutex<()>,
    owner_keys: Mutex<OwnerKeys>,
}

impl Engines {
    pub(crate) fn new(specs: &[EngineSpec]) -> Self {
        Self::from_engines(specs.iter().map(RegionalEngine::new).collect())
    }

    pub(crate) fn from_engines(engines: Vec<RegionalEngine>) -> Self {
        Self {
            regional: engines
                .into_iter()
                .map(|e| (e.id.clone(), Arc::new(e)))
                .collect(),
            routes: RwLock::new(Arc::new(HashMap::new())),
            reload: tokio::sync::Mutex::new(()),
            owner_keys: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn engine(&self, id: &str) -> Option<&Arc<RegionalEngine>> {
        self.regional.get(id)
    }

    /// Charge regional source requests (search, chapter list, page list) to the
    /// per-source budgets. Set once at start.
    pub(crate) fn set_budgets(&self, budgets: &Arc<SourceBudgets>) {
        for engine in self.regional.values() {
            let _ = engine.budgets.set(budgets.clone());
        }
    }

    /// Configured engines, by id.
    pub(crate) fn engines(&self) -> Vec<Arc<RegionalEngine>> {
        let mut all: Vec<_> = self.regional.values().cloned().collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all
    }

    /// Whether any source is routed at all. Unrouted servers skip every route lookup.
    pub(crate) fn any_route(&self) -> bool {
        !self.routes().is_empty()
    }

    pub(crate) fn routes(&self) -> Arc<HashMap<String, Route>> {
        self.routes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn route(&self, source_id: &str) -> Option<Route> {
        self.routes().get(source_id).cloned()
    }

    pub(crate) async fn reload_routes(&self, pool: &Database) -> Result<()> {
        // A slow monitor query must not publish after a newer admin reload.
        let _reload = self.reload.lock().await;
        let routes = load_routes(pool).await?;
        *self.routes.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(routes);
        Ok(())
    }

    /// Where reads of `source_id` go now. A route serves only while its engine is
    /// configured, answered its last health probe, and runs the owner's extension
    /// version; otherwise the owner engine serves, or a strict route refuses.
    pub(crate) fn decide(&self, source_id: &str) -> Decision {
        let Some(route) = self.route(source_id) else {
            return Decision::Owner;
        };
        let usable = self.engine(&route.engine_id).and_then(|engine| {
            let health = engine.health()?;
            let installed = health.installed.get(source_id);
            (health.reachable
                && compare_versions(route.owner_extension.as_ref(), installed)
                    == VersionCheck::Match)
                .then(|| engine.clone())
        });
        match usable {
            Some(engine) => Decision::Regional {
                engine,
                strict: route.strict,
            },
            None if route.strict => Decision::Refuse,
            None => Decision::Owner,
        }
    }

    /// `decide` for a read: a routed source that the owner serves counts as a fallback.
    /// The admin view calls `decide` itself, so looking does not move the counter.
    pub(crate) fn decide_read(&self, source_id: &str) -> Decision {
        let decision = self.decide(source_id);
        if matches!(decision, Decision::Owner) {
            if let Some(engine) = self
                .route(source_id)
                .and_then(|route| self.engine(&route.engine_id).cloned())
            {
                EngineCounters::bump(&engine.counters.fallbacks);
            }
        }
        decision
    }

    /// Probe every engine once.
    pub(crate) async fn probe_all(&self) {
        for engine in self.regional.values() {
            engine.probe().await;
        }
    }
}

/// Keep health and routes fresh: probe every engine and reload the routes each minute.
pub(crate) fn spawn_monitor(
    engines: Arc<Engines>,
    pool: Database,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    if engines.regional.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tick.tick() => {
                    engines.probe_all().await;
                    if let Err(error) = engines.reload_routes(&pool).await {
                        tracing::warn!(error = %error, "engine routes: reload failed");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
#[path = "engine_route_tests.rs"]
pub(crate) mod tests;
