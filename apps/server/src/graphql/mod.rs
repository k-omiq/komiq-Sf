pub mod types;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_graphql::{
    ComplexObject, Context, EmptySubscription, Error, Object, Result, Schema, SimpleObject, ID,
};
use chrono::Utc;
use sqlx::SqlitePool;

use crate::auth::{self, User};
use crate::browse::BROWSE_PAGE_SIZE;
use crate::catalog;
use crate::scanner::scan_series;
use crate::suwayomi::{FetchType, SuwayomiChapter, SuwayomiClient, SuwayomiManga};
use types::*;

const PAGE_SIZE: i64 = 20;

/// Aggregate scan-scheduler health, updated once per tick by `scanner::tick`.
#[derive(Default)]
pub struct ScanHealth {
    pub library_size: usize,
    pub overdue_count: usize,
    pub last_tick_at: Option<String>,
    /// Series that scanned successfully on the last tick.
    pub scanned_ok: usize,
    /// Series whose scan errored (and were backed off) on the last tick.
    pub scanned_failed: usize,
    /// ISO 8601 timestamp of the last tick that made real progress (>=1 success).
    pub last_success_at: Option<String>,
    /// Consecutive full batches that advanced nothing — a "stuck" signal (upstream
    /// outage or a wall of dead ids). 0 = healthy. Lets the admin console tell a live
    /// scanner from one that's looping without progress.
    pub consecutive_stuck_ticks: usize,
}

/// The sliding-window rate limiter now lives in [`crate::rate_limit`] as a sharded
/// implementation (one lock acquisition per check, per-shard sweeps instead of a
/// global O(n) scan). Re-exported here so `AppState`, the statics below, and
/// `main.rs`'s `use crate::graphql::RateLimiter` all keep working unchanged.
pub use crate::rate_limit::RateLimiter;

/// Sliding-window budget for the unauthenticated `recordView` write, keyed
/// `view:{client_ip}:{series_id}`. A process-global (rather than an `AppState` field)
/// so the limiter exists without changing `AppState`'s construction, which lives in
/// `main.rs`. 10 per minute per (ip, series) is far above a real reader — a chapter open
/// fires one — and far below what it takes to move the Trending top-10.
///
/// CAVEAT: this is only as good as `ClientIp`. If the deployment resolves every request
/// to one IP (all traffic behind a proxy whose forwarded header isn't trusted), the
/// budget degrades to a per-series global cap. That is still a hard bound on ballot
/// stuffing, just a blunter one; fixing the IP resolution is a separate change in the
/// HTTP layer.
static VIEW_LIMITER: std::sync::LazyLock<RateLimiter> =
    std::sync::LazyLock::new(|| RateLimiter::new(10, 60));

/// How long a memoized `discovery` / `genreFacets` payload stays usable.
///
/// These payloads are viewer-invariant given one input — the NSFW posture (see the
/// key below) — yet cost tens of ms of CPU each and dominate home-feed traffic, so
/// they were being recomputed per request for identical output. The TTL bounds how
/// stale a feed can be: a new catalogue entry or view-count shift lands within a
/// minute, which matches how often the underlying data actually moves.
///
/// Zero under test: like `browse::COUNT_TTL`, the memo is a process-wide static but
/// every test builds its own in-memory DB, so two tests whose viewer has the same
/// NSFW posture would otherwise read each other's feeds depending on scheduling. A
/// zero TTL makes every resolver compute exactly, always; the memo logic itself is
/// directly testable via [`cached_feed_with`] / [`store_feed`].
#[cfg(not(test))]
const FEED_TTL: std::time::Duration = std::time::Duration::from_secs(45);
#[cfg(test)]
const FEED_TTL: std::time::Duration = std::time::Duration::ZERO;

/// The memo key for the viewer-invariant feed caches: the viewer's NSFW posture,
/// and NOTHING else.
///
/// This must stay exactly one dimension wide. Every other input to `compute_discovery`
/// / `browse::genre_facets` is global state (the SQLite catalogue), and every per-viewer
/// field on the cached objects (`isMarked`, `libraryStatus`, `isFavorite`) is resolved
/// lazily per request by ComplexObject resolvers against the CALLER's own session — it
/// is not captured in the stored value. So two viewers who agree on `show_nsfw` get
/// byte-identical payloads, and keying on anything more (user id, token) would only
/// shred the hit rate. Conversely, OMITTING the NSFW posture would leak an opted-out
/// viewer a feed computed for an opted-in one — the gate runs inside the computation
/// (`filter_nsfw` + the SQL predicates), which is why it must be part of the key.
///
/// A bool keys the map to at most 2 live entries; no eviction is possible or needed.
fn feed_cache_key(show_nsfw: bool) -> bool {
    show_nsfw
}

/// Memoized viewer-invariant reads, keyed by [`feed_cache_key`]. Shape follows
/// `browse::COUNT_CACHE`: a process-wide `Mutex<HashMap<key, (value, when)>>`.
#[allow(clippy::type_complexity)]
type FeedCache<T> =
    std::sync::Mutex<std::collections::HashMap<bool, (std::sync::Arc<T>, std::time::Instant)>>;

/// The curated home-feed rails (`discovery`).
static DISCOVERY_CACHE: std::sync::LazyLock<FeedCache<Vec<DiscoveryFeed>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
/// Browse's genre chips (`genreFacets`).
static GENRE_FACETS_CACHE: std::sync::LazyLock<FeedCache<Vec<(String, i64)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Single-flight locks collapsing concurrent refills of [`DISCOVERY_CACHE`] (S1
/// pattern). Keyed 0/1 by NSFW posture — a separate keyspace from
/// [`GENRE_FACETS_INFLIGHT`] and from the AppState locks, whose keys are series ids,
/// so a feed refill can never serialize behind a series refresh.
static DISCOVERY_INFLIGHT: std::sync::LazyLock<KeyedLocks> =
    std::sync::LazyLock::new(KeyedLocks::default);
/// Single-flight locks collapsing concurrent refills of [`GENRE_FACETS_CACHE`].
static GENRE_FACETS_INFLIGHT: std::sync::LazyLock<KeyedLocks> =
    std::sync::LazyLock::new(KeyedLocks::default);

/// Fresh hit out of a feed memo, if any. Poisoned lock ⇒ "no memo" rather than an
/// error: the caller recomputes exactly, matching `browse::cached_count_with`.
fn cached_feed<T>(cache: &FeedCache<T>, key: bool, ttl: Duration) -> Option<std::sync::Arc<T>> {
    let map = cache.lock().ok()?;
    let (value, at) = map.get(&key)?;
    (at.elapsed() < ttl).then(|| value.clone())
}

/// Store one computed payload. The map is keyed by a bool, so it is bounded at 2
/// entries by construction — no cap/evict logic to maintain (unlike COUNT_CACHE,
/// whose client-controlled key space needs one). Expiry is enforced at READ time
/// against the stored `Instant`, so storing needs no TTL.
fn store_feed<T>(cache: &FeedCache<T>, key: bool, value: T) {
    let Ok(mut map) = cache.lock() else {
        return; // poisoned → just stop memoizing; every read still computes exactly
    };
    map.insert(key, (std::sync::Arc::new(value), std::time::Instant::now()));
}

/// Drop every memoized feed payload, so the next `discovery` / `genreFacets` recomputes.
///
/// Both posture keys go, always: one admin edit can move a work in EITHER direction
/// (pinning it NSFW removes it from the `show_nsfw = false` feed; clearing the pin puts
/// it back), and the rails are also keyed on title/type/tags, which the same mutation
/// can change. Two entries — clearing both is cheaper than reasoning about which moved.
///
/// `browse::clear_count_cache()` goes with them: Browse's row `COUNT(*)` is memoized on
/// the same posture and would otherwise report a total the filtered rows no longer match.
///
/// Deliberately NOT called from `resync_feed_nsfw`, even though that runs on every write
/// to the flag. The ingest paths (`addSourceSeries`, consolidate) call it once PER SERIES,
/// so busting there would throw the home feed away repeatedly through a catalogue crawl —
/// the exact recomputation the memo exists to stop. The admin mutations below are
/// deliberate, occasional, and want to be visible at once, so they call this themselves.
fn clear_feed_caches() {
    if let Ok(mut map) = DISCOVERY_CACHE.lock() {
        map.clear();
    }
    if let Ok(mut map) = GENRE_FACETS_CACHE.lock() {
        map.clear();
    }
    crate::browse::clear_count_cache();
}

/// Per-key single-flight locks (S1 TTL refresh). N concurrent misses/refreshes for
/// the same series/chapter id collapse onto ONE upstream fetch instead of
/// stampeding Suwayomi: each caller awaits the same per-key `tokio` mutex, and the
/// losers re-check the cache (now populated) after acquiring it. The outer map is
/// guarded by a std mutex held only for the O(1) lookup/insert — never across an
/// `await` — and dead entries are pruned via `Weak` so the map can't grow unbounded.
#[derive(Default)]
pub struct KeyedLocks {
    map: Mutex<HashMap<i64, std::sync::Weak<tokio::sync::Mutex<()>>>>,
}

impl KeyedLocks {
    /// Get (or create) the shared lock for `key`. Holding the returned `Arc`'s guard
    /// serializes the critical section for that key across tasks.
    pub fn lock_handle(&self, key: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get(&key).and_then(std::sync::Weak::upgrade) {
            return existing;
        }
        let arc = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        map.insert(key, std::sync::Arc::downgrade(&arc));
        // Opportunistically drop entries whose only reference was the (now-dead)
        // weak, so a long-lived process doesn't accumulate one entry per id ever seen.
        map.retain(|_, w| w.strong_count() > 0);
        arc
    }
}

/// Shared, request-independent application state.
pub struct AppState {
    pub pool: SqlitePool,
    /// Separate, un-replicated DB for cover blobs (`work_cover_blob`). Kept out of
    /// `pool` so Litestream/R2 backs up only the main DB; see `db::init_covers`.
    pub cover_pool: SqlitePool,
    pub suwayomi: SuwayomiClient,
    /// Direct MangaDex client — page resolution for canonical (MangaDex-mirrored)
    /// works via MangaDex@Home (CATALOGUE.md §5). Shared with the catalogue sync.
    pub mangadex: std::sync::Arc<crate::mangadex::MangaDexClient>,
    /// Usernames granted admin (see `Config::admin_users`).
    pub admin_users: Vec<String>,
    /// Aggregate scan-scheduler health (for `scanStatus`).
    pub scan_health: Mutex<ScanHealth>,
    /// Per-key sliding-window limiter for `login` / `register`.
    pub auth_limiter: RateLimiter,
    /// Per-user sliding-window limiter for `searchAllSources` (C1). That endpoint
    /// fans out to many sources and performs writes (library enrollment + dedup
    /// persist), so an authenticated client must not be able to hammer it.
    pub federated_limiter: RateLimiter,
    /// Absolute session lifetime in seconds (see `Config::session_ttl_secs`).
    pub session_ttl_secs: i64,
    /// Single-flight locks collapsing concurrent Suwayomi series-metadata
    /// refreshes for the same id (S1 TTL refresh).
    pub series_inflight: KeyedLocks,
    /// Single-flight locks collapsing concurrent Suwayomi chapter-list refreshes
    /// for the same manga id (S1 TTL refresh).
    pub chapters_inflight: KeyedLocks,
    /// Guards the DB-backed cover crawl (`materializeCatalogueCovers`) so two
    /// admin clicks can't run overlapping catalogue-wide MangaDex crawls.
    pub cover_crawl_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether the catalogue sync downloads + pHashes each work's cover during the
    /// sweep (`Config::catalogue_cover_phash`). Threaded here so the admin
    /// `resyncCatalogue` kicks a cycle with the same setting as the recurring loop.
    pub catalogue_cover_phash: bool,
    /// Directory of vendored extension icons served by `serve_ext_icon`
    /// (`Config::ext_icons_dir`).
    pub ext_icons_dir: std::path::PathBuf,
}

/// Per-request auth: the bearer token from the `Authorization` header, if any.
#[derive(Clone, Default)]
pub struct RequestAuth(pub Option<String>);

/// Per-request client IP, resolved by the HTTP layer (X-Forwarded-For behind a
/// proxy, else the socket peer). Used to key the auth rate limiter so one actor
/// cannot exhaust another account's budget.
#[derive(Clone, Default)]
pub struct ClientIp(pub Option<String>);

/// Per-request memoization of the authenticated user. `current_user` is called
/// once per series in a feed (via `is_marked`/`viewer_show_nsfw`/…), so without a
/// cache a single `discovery`/`search`/`library` query does one `sessions⋈users`
/// lookup per item. A fresh `OnceCell` is attached per HTTP request, so the DB
/// lookup runs at most once regardless of how many resolvers ask.
#[derive(Clone, Default)]
pub struct RequestUserCache(pub std::sync::Arc<tokio::sync::OnceCell<Option<User>>>);

/// Per-request memoization of the viewer's `show_nsfw` preference. `viewer_show_nsfw`
/// is consulted by nearly every feed resolver (`discovery`, `updates`, `search`,
/// per-series NSFW gates on `series`/`chapters`), and each call used to re-run a
/// `users` SELECT even though the session lookup feeding it is already memoized —
/// so one home render paid it several times over. A fresh cell is attached per HTTP
/// request; the flag is request-stable, so one query suffices.
#[derive(Clone, Default)]
pub struct RequestNsfwPref(pub std::sync::Arc<tokio::sync::OnceCell<bool>>);

/// One viewer's `user_library` row for a series — the shared payload behind the
/// `is_marked` / `library_status` / `is_favorite` resolvers.
#[derive(Clone)]
pub struct LibraryRow {
    pub is_favorite: bool,
    pub status: Option<String>,
}

/// Per-request memoization of the viewer's `user_library` row, keyed by `series_id`.
/// The three per-item resolvers (`is_marked` / `library_status` / `is_favorite`) each read
/// the SAME row, and async-graphql resolves an object's fields concurrently, so without this
/// a single feed does 3 SELECTs per item on the same row. A per-key `OnceCell` (guarded by a
/// short std-Mutex critical section that only swaps `Arc`s, never awaits) dedupes even the
/// concurrent reads down to one query per series. A fresh map is attached per HTTP request.
type LibraryCellMap = HashMap<String, std::sync::Arc<tokio::sync::OnceCell<Option<LibraryRow>>>>;

#[derive(Clone, Default)]
pub struct RequestLibraryCache(pub std::sync::Arc<std::sync::Mutex<LibraryCellMap>>);

/// Upper bound on password length accepted by `login`/`register`, enforced
/// before hashing so an over-long password can't amplify Argon2 CPU cost (A7).
const MAX_PASSWORD_LEN: usize = 1024;

/// A fixed dummy Argon2 hash (of a random password nobody knows), used to run a
/// constant-work verify on the login missing-user path so response time doesn't
/// reveal whether a username exists (A3). Built once with the default params so
/// it costs the same as a real verify.
static DUMMY_PASSWORD_HASH: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    auth::hash_password("komika-constant-time-dummy-abc123").expect("dummy hash")
});

pub type ApiSchema = Schema<QueryRoot, MutationRoot, EmptySubscription>;

/// Build the schema over a shared `AppState`. The same `Arc` is handed to the scan
/// scheduler so resolvers and the background task see one set of state. When
/// `disable_introspection` is set (production default), schema introspection is
/// turned off so the API surface isn't publicly enumerable.
pub fn build_schema(state: std::sync::Arc<AppState>, disable_introspection: bool) -> ApiSchema {
    let mut builder = Schema::build(QueryRoot, MutationRoot, EmptySubscription)
        .extension(ErrorLogger)
        // Bound query cost on this public endpoint (H7): cap nesting depth and the
        // total resolved-field complexity so an unauthenticated client can't fan a
        // single aliased/nested request out to thousands of per-item DB resolvers.
        .limit_depth(20)
        .limit_complexity(1000)
        .data(state);
    if disable_introspection {
        builder = builder.disable_introspection();
    }
    builder.finish()
}

/// async-graphql extension that logs every resolver error server-side. GraphQL
/// returns HTTP 200 with errors in the body, so the TraceLayer access log never
/// sees them — without this, DB/upstream failures would be invisible in logs.
/// This is our dependency-light "error tracking": a Sentry/OpenTelemetry hook is
/// intentionally NOT wired (no hard dependency, no committed DSN); if one is ever
/// added it should live behind an env-gated optional cargo feature.
struct ErrorLogger;

impl async_graphql::extensions::ExtensionFactory for ErrorLogger {
    fn create(&self) -> std::sync::Arc<dyn async_graphql::extensions::Extension> {
        std::sync::Arc::new(ErrorLoggerExtension)
    }
}

struct ErrorLoggerExtension;

#[async_trait::async_trait]
impl async_graphql::extensions::Extension for ErrorLoggerExtension {
    async fn request(
        &self,
        ctx: &async_graphql::extensions::ExtensionContext<'_>,
        next: async_graphql::extensions::NextRequest<'_>,
    ) -> async_graphql::Response {
        let resp = next.run(ctx).await;
        for err in &resp.errors {
            tracing::warn!(
                error = %err.message,
                path = ?err.path,
                "graphql resolver error"
            );
        }
        resp
    }
}

/// Guarantee an ISO-8601 date-time carries a UTC offset, appending `Z` when it does not.
///
/// `Date.parse` in the browser applies a documented split: a date-ONLY string is read as
/// UTC, but an offset-less date-TIME string is read as LOCAL time. So a bare
/// `"2026-07-26T13:58:51.705034"` renders in the reader shifted by the viewer's own UTC
/// offset — up to half a day out, and far enough to produce a detection timestamp in the
/// future ("in 9h"). Everything this server stores in these columns is UTC.
///
/// Deliberately NOT `types::to_iso`: that helper parses an epoch INTEGER string and
/// returns `None` on anything else, so routing an already-ISO column through it would
/// have silently blanked the field rather than fixing its offset.
///
/// Audited on a production snapshot: all 1,316 dated `series_scan_state` rows already
/// end in `+00:00`, because `scanner::scan_series` writes `to_rfc3339()`. So this is a
/// guard against a future writer, not a repair of existing rows — a bare
/// `strftime('%Y-%m-%dT%H:%M:%S')` (the shape used elsewhere in `scanner.rs`, which
/// appends `'+00:00'` by hand) is one forgotten concatenation away from the offset-less
/// case. Values that already carry `Z` or a `±HH:MM` offset pass through untouched.
fn ensure_utc_offset(s: &str) -> String {
    let t = s.trim();
    // Look for the offset only in the TIME part: the date part's own `-` separators
    // (`2026-07-26`) must not be mistaken for a negative UTC offset.
    let time = t.split_once('T').map_or("", |(_, time)| time);
    if time.ends_with('Z') || time.contains('+') || time.contains('-') {
        t.to_string()
    } else {
        format!("{t}Z")
    }
}

fn state<'a>(ctx: &Context<'a>) -> &'a AppState {
    ctx.data_unchecked::<std::sync::Arc<AppState>>()
}

/// Resolver fields on `Series` that read the S2 enrichment tables (H2). Opt-in
/// per query — a feed that doesn't select them pays nothing. `self.id` is the
/// work id for a canonical series (`w_…`); for a numeric Suwayomi series id there
/// are no rows (those works aren't MangaDex-anchored) and both return empty.
#[async_graphql::ComplexObject]
impl Series {
    /// The all-time / 7-day / 24-hour view counts for this series (the popularity
    /// signal — see the `views` module). Resolved lazily against the normalised view
    /// key, so a series read under either identity reports one unified count. Feeds that
    /// don't select `views` never run this query.
    async fn views(&self, ctx: &Context<'_>) -> Result<SeriesViews> {
        let c = crate::views::counts_for(&state(ctx).pool, &self.id.0).await;
        Ok(SeriesViews {
            total: c.total as i32,
            last7d: c.last7d as i32,
            last24h: c.last24h as i32,
        })
    }

    /// The canonical `work` this series belongs to, or null when the catalogue has no
    /// mapping for it yet.
    ///
    /// A numeric Suwayomi id addresses ONE source mapping, but the work behind it may
    /// carry several — and the reader has no other way to get from the id on the URL to
    /// the work whose sibling sources it must offer in the source picker. Browse links a
    /// Suwayomi-anchored work by its numeric `reader_id`, so without this field those
    /// pages silently render as single-source (see `getSeries` in the reader's
    /// `data/source.ts`).
    ///
    /// A RESOLVER field, not a column on the row: it costs a query per series, so it is
    /// selected ONLY by the single-series detail query, never by the shared
    /// `SeriesFields` fragment that feeds/browse grids spread — that would be a per-card
    /// N+1. For a `w_` id it is just an existence check and answers with the id itself.
    async fn work_id(&self, ctx: &Context<'_>) -> Result<Option<ID>> {
        Ok(resolve_work_id(&state(ctx).pool, &self.id.0).await.map(ID))
    }

    /// When OUR scanner first detected this series' newest chapter
    /// (`series_scan_state.last_new_chapter_at`) — discovery time, not upstream release
    /// time. Membership in the Updates feed is decided by this column, but the feed is
    /// ORDERED by `latestChapterAt` (the real release time it renders), so the reader
    /// can pair the two into "released 7d ago · we found it 1h ago". Deliberately a
    /// separate field: it used to be written over `updatedAt`, which made the feed
    /// misreport a week-old chapter as an hour old. Null for a series the scanner has
    /// never seen gain a chapter (including every canonical `w_` work, which has no
    /// `series_scan_state` row).
    ///
    /// Reads the value the batched mappers (`map_series_batch` / `map_browse_rows`)
    /// already fetched for the whole page — one grouped SELECT per render instead of
    /// one query per card (a home render used to pay ~80 of these). Only this
    /// single-series fallback shape could not batch: there is none any more.
    async fn detected_at(&self, _ctx: &Context<'_>) -> Result<Option<String>> {
        // Guaranteed to leave the server with a UTC offset, so the reader's `Date.parse`
        // cannot read it as LOCAL time and shift the tooltip by the viewer's offset.
        Ok(self.detected_at.as_deref().map(ensure_utc_offset))
    }

    /// Whether the signed-in viewer has this series in THEIR library (`user_library`).
    /// Per-viewer, resolved dynamically so every feed reflects the caller's own
    /// library; `false` for anonymous viewers (no made-up membership).
    async fn is_marked(&self, ctx: &Context<'_>) -> Result<bool> {
        Ok(viewer_library_row(ctx, &self.id.0).await.is_some())
    }

    /// The shelf the viewer has explicitly filed this series under
    /// ('reading' | 'completed' | 'onhold' | 'plan'), or null when unset — in which
    /// case the client derives the shelf from read progress. Per-viewer; null for
    /// anonymous viewers and series not in the viewer's library.
    async fn library_status(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        Ok(viewer_library_row(ctx, &self.id.0)
            .await
            .and_then(|r| r.status))
    }

    /// Whether the viewer has favourited this series. Per-viewer; false for
    /// anonymous viewers and series not in the viewer's library.
    async fn is_favorite(&self, ctx: &Context<'_>) -> Result<bool> {
        Ok(viewer_library_row(ctx, &self.id.0)
            .await
            .map(|r| r.is_favorite)
            .unwrap_or(false))
    }

    /// Every localized description of this work (all languages MangaDex carries),
    /// newest-language-agnostic, ordered by language tag. Empty for a work with no
    /// enrichment yet (run `backfillMangadexMetadata`) or a non-canonical series.
    async fn localized_descriptions(&self, ctx: &Context<'_>) -> Result<Vec<LocalizedDescription>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT lang, description FROM work_description WHERE work_id = ? ORDER BY lang",
        )
        .bind(&self.id.0)
        .fetch_all(&state(ctx).pool)
        .await
        .map_err(gql_err)?;
        Ok(rows
            .into_iter()
            .map(|(lang, description)| LocalizedDescription { lang, description })
            .collect())
    }

    /// The full author/artist credit list for this work (S2). The singular
    /// `author`/`artist` fields keep only the first of each; this returns all.
    async fn credits(&self, ctx: &Context<'_>) -> Result<Vec<Credit>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT role, name FROM work_credit WHERE work_id = ? ORDER BY role, name",
        )
        .bind(&self.id.0)
        .fetch_all(&state(ctx).pool)
        .await
        .map_err(gql_err)?;
        Ok(rows
            .into_iter()
            .map(|(role, name)| Credit { role, name })
            .collect())
    }

    /// The work's full cover set (F2), primary first then by volume. Empty for a
    /// work with no covers stored yet or a non-canonical series. `coverUrl` (the
    /// primary) keeps working independently.
    async fn covers(&self, ctx: &Context<'_>) -> Result<Vec<Cover>> {
        let pool = &state(ctx).pool;
        // Cover URLs are `covers/{mangadex_id}/{fileName}` — resolve the anchor.
        let mangadex_id: Option<String> = sqlx::query_scalar(
            "SELECT source_key FROM source_series \
             WHERE work_id = ? AND source_type = 'mangadex' LIMIT 1",
        )
        .bind(&self.id.0)
        .fetch_optional(pool)
        .await
        .map_err(gql_err)?;
        let Some(mid) = mangadex_id else {
            return Ok(Vec::new());
        };
        let covers = catalog::load_work_covers(pool, &self.id.0)
            .await
            .map_err(gql_err)?;
        Ok(covers
            .into_iter()
            .map(|c| Cover {
                url: crate::mangadex::cover_url(&mid, &c.file_name),
                thumbnail_url: crate::mangadex::cover_thumb_url(&mid, &c.file_name),
                file_name: c.file_name,
                lang: c.lang,
                volume: c.volume,
                is_primary: c.is_primary,
            })
            .collect())
    }
}

/// The current session token, if the request carried one.
fn token(ctx: &Context<'_>) -> Option<String> {
    ctx.data_opt::<RequestAuth>().and_then(|a| a.0.clone())
}

/// The request's client IP for rate-limiting, or `"unknown"` when it could not
/// be resolved (all such requests then share one conservative budget).
fn client_ip(ctx: &Context<'_>) -> String {
    ctx.data_opt::<ClientIp>()
        .and_then(|c| c.0.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Resolve the authenticated user, or `None` if the request is anonymous.
/// Memoized per request via `RequestUserCache` so the `sessions⋈users` lookup
/// runs at most once even when many resolvers ask (one per feed item).
async fn current_user(ctx: &Context<'_>) -> Option<User> {
    if let Some(cache) = ctx.data_opt::<RequestUserCache>() {
        return cache
            .0
            .get_or_init(|| async {
                match token(ctx) {
                    Some(tok) => auth::user_for_token(&state(ctx).pool, &tok)
                        .await
                        .ok()
                        .flatten(),
                    None => None,
                }
            })
            .await
            .clone();
    }
    // Fallback when no cache was attached (e.g. a code path that builds a bare request).
    let tok = token(ctx)?;
    auth::user_for_token(&state(ctx).pool, &tok)
        .await
        .ok()
        .flatten()
}

/// Fetch the viewer's `user_library` row for one series in a single SELECT. `None` when
/// the row doesn't exist (series not in the viewer's library); a transient DB error is
/// also folded to `None` so a feed degrades to "not in library" rather than erroring the
/// whole response (matching how anonymous viewers already resolve these flags to false).
async fn fetch_library_row(
    pool: &SqlitePool,
    user_id: &str,
    series_id: &str,
) -> Option<LibraryRow> {
    sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT is_favorite, status FROM user_library WHERE user_id = ? AND series_id = ?",
    )
    .bind(user_id)
    .bind(series_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|(fav, status)| LibraryRow {
        is_favorite: fav != 0,
        status,
    })
}

/// Resolve (and per-request memoize) the viewer's `user_library` row for `series_id`.
/// `None` for anonymous viewers or a series not in the viewer's library. Backs all three
/// of `is_marked` / `library_status` / `is_favorite` so they share one query per series
/// (see [`RequestLibraryCache`]).
async fn viewer_library_row(ctx: &Context<'_>, series_id: &str) -> Option<LibraryRow> {
    let user = current_user(ctx).await?;
    let cell = match ctx.data_opt::<RequestLibraryCache>() {
        Some(cache) => cache
            .0
            .lock()
            .unwrap()
            .entry(series_id.to_string())
            .or_default()
            .clone(),
        // No cache attached (bare request) — fall back to a direct fetch.
        None => return fetch_library_row(&state(ctx).pool, &user.id, series_id).await,
    };
    cell.get_or_init(|| fetch_library_row(&state(ctx).pool, &user.id, series_id))
        .await
        .clone()
}

/// Resolve the authenticated user or fail — for mutations that require sign-in.
async fn require_user(ctx: &Context<'_>) -> Result<User> {
    current_user(ctx)
        .await
        .ok_or_else(|| Error::new("Not authenticated"))
}

/// Resolve the authenticated user and require the admin flag — for the console.
async fn require_admin(ctx: &Context<'_>) -> Result<User> {
    let user = require_user(ctx).await?;
    if user.is_admin == 0 {
        return Err(Error::new("Admin access required"));
    }
    Ok(user)
}

/// Wrap an internal/DB error for return to a GraphQL client. The concrete detail
/// (sqlx messages carry table/column names and SQL fragments) is logged
/// server-side only; the client receives a generic message so the API surface
/// isn't leaked. Deliberately user-facing errors are built with `Error::new(...)`
/// directly and never routed through here, so they're preserved.
fn gql_err(e: impl std::fmt::Display) -> Error {
    tracing::error!(error = %e, "internal error");
    Error::new("Internal error")
}

/// Aggregate the stored reviews for a series into a `RatingSummary`.
async fn rating_summary(pool: &SqlitePool, series_id: &str) -> RatingSummary {
    // Aggregate in SQL: GROUP BY bounds the result to <=10 rows regardless of how
    // many reviews a series has, instead of pulling every review row into memory
    // (the old `SELECT score` loaded the full review set on every feed item).
    let rows: Vec<(i64, i64)> =
        sqlx::query_as("SELECT score, COUNT(*) FROM reviews WHERE series_id = ? GROUP BY score")
            .bind(series_id)
            .fetch_all(pool)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, series_id, "rating_summary query failed");
                Vec::new()
            });
    if rows.is_empty() {
        return RatingSummary::empty();
    }
    let mut dist = vec![0i32; 10];
    let mut sum = 0i64;
    let mut count = 0i64;
    for (score, n) in &rows {
        sum += *score * *n;
        count += *n;
        let idx = (*score - 1).clamp(0, 9) as usize;
        dist[idx] += *n as i32;
    }
    RatingSummary {
        average: sum as f64 / count as f64,
        count: count as i32,
        distribution: dist,
    }
}

/// Komika-native per-series admin overrides (from `series_admin`).
#[derive(Clone, Default, sqlx::FromRow)]
struct AdminOverrides {
    override_interval_hours: Option<f64>,
    poll_every_minutes: Option<i64>,
    paused_override: Option<i64>,
    status_override: Option<String>,
}

/// Whether a federated Suwayomi series is NSFW per the canonical model (CATALOGUE.md
/// §2). True once it's linked to a `work` flagged NSFW; false when uncatalogued (we
/// only hide what we positively know is NSFW).
async fn canonical_is_nsfw(pool: &SqlitePool, suwayomi_id: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(COALESCE(w.is_nsfw_override, w.is_nsfw)), 0) FROM source_series ss \
         JOIN work w ON w.id = ss.work_id \
         WHERE ss.source_type = 'suwayomi' AND ss.source_key = ?",
    )
    .bind(suwayomi_id)
    .fetch_optional(pool)
    .await
    // Fail CLOSED on a DB error: treat the series as NSFW so a transient failure
    // can't surface NSFW content to an opted-out viewer (matches the fail-closed
    // viewer-preference half). A successful query with no row means "not linked to
    // an NSFW work" → SFW.
    .map(|row| row.unwrap_or(0) != 0)
    .unwrap_or(true)
}

/// Whether a canonical `work` is NSFW, by its own id (as opposed to
/// `canonical_is_nsfw`, which resolves a Suwayomi source key). Uses the same effective
/// flag as every other surface: `COALESCE(is_nsfw_override, is_nsfw)`.
///
/// Fails CLOSED on a DB error (treats the work as NSFW). An id with no `work` row is
/// NOT NSFW — an unknown work has nothing to hide, and the caller's own lookup will
/// come back empty anyway.
async fn work_is_nsfw(pool: &SqlitePool, work_id: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(is_nsfw_override, is_nsfw) FROM work WHERE id = ?",
    )
    .bind(work_id)
    .fetch_optional(pool)
    .await
    .map(|row| row.unwrap_or(0) != 0)
    .unwrap_or(true)
}

/// Batched `work_is_nsfw`: the subset of `work_ids` that are NSFW. Fails CLOSED — a
/// query error returns EVERY input id, so an opted-out viewer sees nothing rather than
/// everything.
async fn nsfw_work_ids(
    pool: &SqlitePool,
    work_ids: &[String],
) -> std::collections::HashSet<String> {
    if work_ids.is_empty() {
        return std::collections::HashSet::new();
    }
    let sql = format!(
        "SELECT id FROM work WHERE COALESCE(is_nsfw_override, is_nsfw) = 1 AND id IN ({})",
        in_placeholders(work_ids.len())
    );
    let mut q = sqlx::query_scalar::<_, String>(&sql);
    for id in work_ids {
        q = q.bind(id);
    }
    match q.fetch_all(pool).await {
        Ok(rows) => rows.into_iter().collect(),
        Err(e) => {
            tracing::warn!(error = %e, "nsfw_work_ids query failed; failing closed");
            work_ids.iter().cloned().collect()
        }
    }
}

/// Re-derive the MATERIALIZED NSFW flag on all three feed tables for the given works.
///
/// `feed_updates` (migration 0051), `feed_series_updates` (0064) and `browse_catalogue`
/// (0069) each store a COPY of `COALESCE(work.is_nsfw_override, work.is_nsfw)` so their
/// resolvers can pin `is_nsfw = 0` as an index prefix instead of joining `work` per row. That
/// copy is only rewritten by `catalog::refresh_feed_updates`, i.e. at boot and once per
/// catalogue-sync cycle — so between refreshes an admin who marks a work NSFW keeps SERVING
/// it to opted-out viewers on `updatesFeed` / `canonicalUpdates` / Browse, for hours.
///
/// `browse_catalogue` is the one that matters most, and it is the reason this list is not
/// two entries: it is the LARGEST of the three (115,567 rows against 48,567) and the only one
/// that holds a work the moment it is catalogued, so a mis-flagged work that no feed carries
/// is still on Browse. Its `total` is memoized too — but the memo is keyed on the viewer's
/// posture and expires within `browse::COUNT_TTL`, whereas the flag would stay wrong until
/// the next sync.
///
/// That is a gap the pre-materialization feeds did not have: `graphql::updates` evaluates
/// the same COALESCE live in SQL and has a Rust-side `filter_nsfw` backstop on top, and
/// `updatesFeed` supersedes it as the reader's Updates surface. So every mutation that
/// writes `work.is_nsfw_override` or `work.is_nsfw` calls this immediately afterwards.
///
/// Re-derives from `work` rather than taking the written value as a parameter: the stored
/// flag is the COALESCE of two columns, and one caller clears the override (reverting to
/// the derived value) instead of setting it, so recomputing is the only form that is
/// right for all of them.
///
/// Best-effort. A failure here leaves the flag stale until the next refresh, which is
/// exactly the status quo it is closing — it must not fail the admin's edit, which has
/// already been committed to `work` (the authoritative column every non-feed surface
/// reads).
async fn resync_feed_nsfw(pool: &SqlitePool, work_ids: &[String]) {
    if work_ids.is_empty() {
        return;
    }
    // 500 ids per statement, matching the other bulk admin paths — well inside SQLite's
    // 32,766 bound-parameter limit.
    for chunk in work_ids.chunks(500) {
        let ph = in_placeholders(chunk.len());
        for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
            // The outer COALESCE keeps the current value if the work row is somehow
            // missing. It cannot be, on any of the three — `work_id` is a
            // `REFERENCES work(id) ON DELETE CASCADE` primary key, and the ids come from
            // `work`/`source_series` in the first place — but `is_nsfw` is NOT NULL, so
            // without it one impossible orphan would abort the statement and leave all
            // 500 works in its chunk stale. `{table}` is one of the three literals above,
            // never input.
            let sql = format!(
                "UPDATE {table} SET is_nsfw = COALESCE( \
                     (SELECT COALESCE(w.is_nsfw_override, w.is_nsfw) \
                      FROM work w WHERE w.id = {table}.work_id), is_nsfw) \
                 WHERE work_id IN ({ph})"
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            if let Err(e) = q.execute(pool).await {
                tracing::warn!(error = %e, table, "resync_feed_nsfw failed; flag stays stale until the next feed refresh");
            }
        }
    }
}

/// The canonical type inputs (admin override + original language) for a federated
/// Suwayomi series, looked up through its linked `work`. Both `None` when the series
/// isn't catalogued yet — the caller then falls back to genre/title heuristics. The
/// Suwayomi source language is deliberately NOT used as the origin language (it is the
/// translation language and would misclassify every non-Japanese title as Manga).
/// The admin metadata overrides + type inputs for a federated Suwayomi series,
/// looked up through its linked `work`. All fields default when the series isn't
/// catalogued (the caller then falls back to the source values). Deterministic
/// (`ORDER BY w.id`) if a source_key ever maps to more than one work.
#[derive(Clone, Default)]
struct SuwayomiWorkOverrides {
    work_id: Option<String>,
    content_type_override: Option<String>,
    original_language: Option<String>,
    title_override: Option<String>,
    description_override: Option<String>,
}

/// The viewer's NSFW preference (default false, including for anonymous requests).
///
/// Memoized per request via [`RequestNsfwPref`] — this is read by every feed resolver
/// and per-series gate, and the underlying `users` lookup must run once per request,
/// not once per resolver. The fallback path (no cache attached) still computes.
async fn viewer_show_nsfw(ctx: &Context<'_>) -> bool {
    if let Some(cache) = ctx.data_opt::<RequestNsfwPref>() {
        return *cache
            .0
            .get_or_init(|| async { resolve_viewer_show_nsfw(ctx).await })
            .await;
    }
    resolve_viewer_show_nsfw(ctx).await
}

/// The unmemoized body of [`viewer_show_nsfw`].
async fn resolve_viewer_show_nsfw(ctx: &Context<'_>) -> bool {
    match current_user(ctx).await {
        Some(u) => user_show_nsfw(&state(ctx).pool, &u.id).await,
        None => false,
    }
}

/// The viewer's NSFW preference, with an ADMIN-CONSOLE override.
///
/// Why this exists. `search` and `canonicalUpdates` are the console's catalogue and
/// updates views — and they are ALSO the reader's. An opted-out admin therefore could
/// not see a single NSFW-flagged work in the console, which is precisely the set the
/// console exists to fix: ~2,500 mainstream titles (Naruto, One Piece, …) are currently
/// mis-flagged `is_nsfw = 1`, and they were invisible to the only person able to
/// unflag them.
///
/// `extensions`/`sources` solve the same problem by hardcoding `show_nsfw = true` for
/// admins, but those resolvers are `require_admin`-gated and have no reader-facing use.
/// These two do, so a blanket admin exemption would silently defeat an admin's own
/// `show_nsfw = false` while they browse the reader. Instead the exemption is EXPLICIT
/// and opt-in per request: the caller asks for `includeNsfw: true`, and it is honoured
/// only for an admin. Anonymous and ordinary logged-in viewers are unaffected —
/// the argument is ignored for them, so it grants nothing and cannot be used to bypass
/// the gate.
async fn viewer_show_nsfw_or_admin(ctx: &Context<'_>, include_nsfw: Option<bool>) -> bool {
    let Some(user) = current_user(ctx).await else {
        return false; // anonymous: never, regardless of the argument
    };
    if include_nsfw == Some(true) && user.is_admin != 0 {
        return true;
    }
    user_show_nsfw(&state(ctx).pool, &user.id).await
}

/// Read a user's persisted `show_nsfw` flag (default false on any lookup failure).
async fn user_show_nsfw(pool: &SqlitePool, user_id: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT show_nsfw FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
        != 0
}

/// Drop NSFW series from a feed unless the viewer opted in. Pure over the already-set
/// `Series.is_nsfw`, so it's one canonical lookup per series (in `map_series`), not per feed.
fn filter_nsfw(show_nsfw: bool, items: Vec<Series>) -> Vec<Series> {
    if show_nsfw {
        return items;
    }
    items.into_iter().filter(|s| !s.is_nsfw).collect()
}

/// Map a single federated Suwayomi manga onto a Komika `Series`. One-off callers
/// (series detail, mutation return values) route through the batch path with a
/// list of one, so the assembly logic has a single source of truth and stays
/// byte-identical between the single and list paths.
async fn map_series(st: &AppState, m: SuwayomiManga) -> Series {
    map_series_batch(st, vec![m])
        .await
        .pop()
        .expect("map_series_batch returns one Series per input")
}

/// Assemble a `Series` from a manga plus its already-resolved per-series lookups.
/// Pure (no DB): every value it needs is passed in, so the single- and batch-map
/// paths share exactly this construction. Mirrors the old inline `map_series` body.
#[allow(clippy::too_many_arguments)]
fn assemble_series(
    st: &AppState,
    m: SuwayomiManga,
    rating: RatingSummary,
    ov: AdminOverrides,
    scan: Option<crate::scanner::ScanState>,
    alt_titles: Vec<String>,
    is_nsfw: bool,
    ov_meta: SuwayomiWorkOverrides,
    genres: Vec<String>,
) -> Series {
    let id = m.id.to_string();
    // Admin metadata overrides win over the source values (same as map_canonical_series,
    // so an edit shows on the numeric Suwayomi path too — feeds and the editor's return).
    let title = ov_meta
        .title_override
        .clone()
        .unwrap_or_else(|| m.title.clone());
    let description = ov_meta
        .description_override
        .clone()
        .or(m.description.clone());
    // Status: admin override wins over the source-derived status.
    let status = ov
        .status_override
        .as_deref()
        .and_then(komika_status)
        .unwrap_or_else(|| status_from(&m.status));
    // Paused: forced override wins; otherwise auto-pause by effective status.
    let paused = ov
        .paused_override
        .map(|v| v != 0)
        .unwrap_or_else(|| paused_for_status(status));

    Series {
        cover_url: st.suwayomi.abs(m.thumbnail_url.as_deref()),
        rating,
        detected_at: None,
        scan: ScanPolicy {
            // Derived scan state (from the background scheduler) wins when present;
            // else fall back to Suwayomi's lastFetchedAt for last_scanned_at.
            avg_interval_hours: scan.as_ref().map(|s| s.avg_interval_hours).unwrap_or(0.0),
            override_interval_hours: ov.override_interval_hours,
            poll_every_minutes: ov.poll_every_minutes.map(|v| v as i32).unwrap_or(30),
            paused,
            status_override: ov.status_override.as_deref().and_then(komika_status),
            paused_override: ov.paused_override.map(|v| v != 0),
            poll_every_minutes_override: ov.poll_every_minutes.map(|v| v as i32),
            last_scanned_at: scan
                .as_ref()
                .and_then(|s| s.last_scanned_at.clone())
                .or_else(|| to_iso(m.last_fetched_at.as_deref())),
            // Coerce the internal "due now" sentinel back to null for display — a
            // never-scanned/freshly-enrolled series shows "due now", not a 1970 timestamp.
            next_scan_at: scan
                .as_ref()
                .and_then(|s| s.next_scan_at.clone())
                .filter(|t| t != crate::scanner::DUE_NOW_SENTINEL),
        },
        r#type: resolve_comic_type(
            ov_meta.content_type_override.as_deref(),
            ov_meta.original_language.as_deref(),
            &genres,
            &m.title,
        ),
        status,
        created_at: to_iso(m.in_library_at.as_deref()).unwrap_or_default(),
        updated_at: to_iso(m.last_fetched_at.as_deref()).unwrap_or_default(),
        // Real newest-chapter time (migration 0050), or NOTHING. There is deliberately no
        // fallback here any more.
        //
        // The fallback was `last_fetched_at` — Suwayomi's `lastFetchedAt`, which OUR OWN
        // poll stamps to now (we fetch with `fetchManga: true`). Combined with
        // `latest_chapter_at` being absent from Suwayomi's wire shape (see
        // `SuwayomiManga`), every live-fetched series rendered "released 1 hour ago" for a
        // chapter that might be months old: the reader's `latestChapterAt` is the field it
        // labels "released N ago", so the server was asserting a POLL time as a RELEASE
        // time. Verified on live series 500 — `last_fetched_at` was that morning's poll.
        // The column itself is clean (0 of 13,802 rows hold a clock-derived value); the
        // lie was entirely in this display path.
        //
        // Empty is safe: the reader's `firstDated(newestUploadAt, latestChapterAt,
        // updatedAt)` chain (`apps/reader/src/lib/data/source.ts`) Date.parse-validates
        // each candidate and falls through to `updatedAt` on its own. A card still shows
        // something — it just no longer claims that something is a release date. Live
        // fetches are also hydrated upstream now (`series_cache::hydrate_latest_chapter_at`
        // for a single series, `map_series_batch` for a page), so in practice this is
        // populated wherever we hold a dated chapter at all.
        latest_chapter_at: to_iso(m.latest_chapter_at.as_deref()).unwrap_or_default(),
        chapter_count: m
            .chapters
            .as_ref()
            .map(|c| c.total_count as i32)
            .unwrap_or(0),
        is_nsfw,
        source_id: m.source_id,
        genres,
        author: m.author,
        artist: m.artist,
        description,
        alt_titles,
        title,
        id: ID(id),
    }
}

/// Map a whole list of federated Suwayomi mangas onto `Series` with O(1) grouped
/// queries per lookup instead of the old ~5·N serial per-series queries: a feed of
/// ~60 series previously issued ~300 round-trips; this issues ~6. Output is
/// byte-identical to mapping each item through the old `map_series` (same order,
/// same field values, same NSFW flag). Effective genres are batched too — see
/// `catalog::work_effective_genres_batch`.
async fn map_series_batch(st: &AppState, list: Vec<SuwayomiManga>) -> Vec<Series> {
    if list.is_empty() {
        return Vec::new();
    }
    // Deduped key set for the `IN (…)` lookups. For a Suwayomi series the numeric id
    // (as text) is both the `series_admin`/scan key and the `source_series.source_key`.
    let ids: Vec<String> = {
        let mut set = std::collections::BTreeSet::new();
        for m in &list {
            set.insert(m.id.to_string());
        }
        set.into_iter().collect()
    };

    // One grouped query per lookup (was one-per-series, run serially).
    let ratings = rating_summary_batch(&st.pool, &ids).await;
    let admins = admin_overrides_batch(&st.pool, &ids).await;
    let scans = scan_state_batch(&st.pool, &ids).await;
    let alts = canonical_alt_titles_batch(&st.pool, &ids).await;
    let nsfw = canonical_is_nsfw_batch(&st.pool, &ids).await;
    let ov_metas = canonical_overrides_batch(&st.pool, &ids).await;
    // Effective genres for every CATALOGUED item on the page, in two grouped queries.
    // This used to be two queries PER ITEM, on the false premise (see the old
    // `// TODO batch`) that a catalogued numeric series is rare on this feed: in
    // production 13,789 of 13,802 Suwayomi series are catalogued and `work_tag` is
    // empty, so essentially every item took the slow branch — ~15 ms each before the
    // join fix, i.e. ~375 ms on a 25-item browse page.
    let genre_work_ids: Vec<String> = {
        let mut set = std::collections::BTreeSet::new();
        for ov in ov_metas.values() {
            if let Some(wid) = &ov.work_id {
                set.insert(wid.clone());
            }
        }
        set.into_iter().collect()
    };
    let genres_by_work = catalog::work_effective_genres_batch(&st.pool, &genre_work_ids).await;
    // The REAL newest-chapter time for the whole page in one query. A live-fetched manga
    // carries `latest_chapter_at: None` (not part of Suwayomi's wire shape), and
    // `assemble_series` no longer papers over that with the poll time — so without this
    // every live-fetch list path (federated search, cold browse, trending-by-key) would
    // render a blank "released" label. This used to live only in the `updates` resolver,
    // which is why those other feeds still showed a poll time; hoisting it here fixes all
    // of them at the single point they all funnel through, at no extra cost (one query
    // per page, and the ids are already deduped above).
    let real_latest = suwayomi_latest_chapter_at_batch(&st.pool, &ids).await;

    let mut out = Vec::with_capacity(list.len());
    for m in list {
        let id = m.id.to_string();
        let rating = ratings
            .get(&id)
            .cloned()
            .unwrap_or_else(RatingSummary::empty);
        let ov = admins.get(&id).cloned().unwrap_or_default();
        let scan = scans.get(&id).cloned();
        // Drop any alt title equal to the primary so only genuine alternatives show.
        let mut alt_titles = alts.get(&id).cloned().unwrap_or_default();
        alt_titles.retain(|t| t != &m.title);
        // `canonical_is_nsfw_batch` fails CLOSED: after a successful query a missing
        // key means "not linked to an NSFW work" → SFW (false); a DB error marks every
        // key NSFW inside the batch fn, so the `unwrap_or(false)` here can't leak it.
        let is_nsfw = nsfw.get(&id).copied().unwrap_or(false);
        let ov_meta = ov_metas.get(&id).cloned().unwrap_or_default();
        // Curated genres (work_tag) when catalogued, else the source genres. Served
        // from the page-wide batch above; a catalogued work with neither curated tags
        // nor parseable source genres is absent from the map and yields `[]` — exactly
        // what the per-item call returned.
        let genres = match &ov_meta.work_id {
            Some(wid) => genres_by_work.get(wid).cloned().unwrap_or_default(),
            None => m.genre.clone(),
        };
        // The scanner's detection time for this series, off the SAME batched scan-state
        // read that feeds `ScanPolicy` — no extra query. `detectedAt` used to re-query
        // `series_scan_state` per card at resolve time (~80 point queries per home
        // render); it now resolves from this field (see the `Series.detected_at` doc).
        let detected_at = scans.get(&id).and_then(|s| s.last_new_chapter_at.clone());
        let mut s = assemble_series(
            st, m, rating, ov, scan, alt_titles, is_nsfw, ov_meta, genres,
        );
        s.detected_at = detected_at;
        // Empty here means the manga came off the WIRE (a cache-read manga already
        // carries the column). Never overwrite a value the manga brought with it — that
        // one came from this same column and is what a single-series load would show.
        if s.latest_chapter_at.is_empty() {
            if let Some(ts) = real_latest.get(&id) {
                s.latest_chapter_at.clone_from(ts);
            }
        }
        out.push(s);
    }
    out
}

/// Build the `?,?,…` placeholder list for an `IN (…)` clause of `n` values. Values
/// are always bound (never interpolated), so this only ever emits placeholders.
fn in_placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
}

/// Batched `rating_summary`: one grouped query for all series → per-series summary.
/// Missing ids (no reviews) are simply absent; the caller defaults them to empty.
async fn rating_summary_batch(pool: &SqlitePool, ids: &[String]) -> HashMap<String, RatingSummary> {
    if ids.is_empty() {
        return HashMap::new();
    }
    let sql = format!(
        "SELECT series_id, score, COUNT(*) FROM reviews WHERE series_id IN ({}) \
         GROUP BY series_id, score",
        in_placeholders(ids.len())
    );
    let mut q = sqlx::query_as::<_, (String, i64, i64)>(&sql);
    for id in ids {
        q = q.bind(id);
    }
    let rows = q.fetch_all(pool).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "rating_summary_batch query failed");
        Vec::new()
    });
    fold_rating_rows(rows)
}

/// Fold `(key, score, votes)` groups into one [`RatingSummary`] per key — the same
/// arithmetic `rating_summary` does for a single series.
///
/// Extracted so the two batch readers share it: [`rating_summary_batch`], which keys on
/// whatever `reviews.series_id` literally holds, and [`rating_summary_by_work_batch`],
/// which resolves both shapes of that column onto a work id. A duplicated fold would be a
/// place for the star average on one surface to drift from the same work's average on
/// another.
fn fold_rating_rows(rows: Vec<(String, i64, i64)>) -> HashMap<String, RatingSummary> {
    // (distribution[10], sum, count) per key.
    let mut acc: HashMap<String, (Vec<i32>, i64, i64)> = HashMap::new();
    for (sid, score, n) in rows {
        let e = acc
            .entry(sid)
            .or_insert_with(|| (vec![0i32; 10], 0i64, 0i64));
        e.1 += score * n;
        e.2 += n;
        let idx = (score - 1).clamp(0, 9) as usize;
        e.0[idx] += n as i32;
    }
    acc.into_iter()
        .map(|(sid, (dist, sum, count))| {
            (
                sid,
                RatingSummary {
                    average: sum as f64 / count as f64,
                    count: count as i32,
                    distribution: dist,
                },
            )
        })
        .collect()
}

/// Review summaries for a page of canonical works, keyed by **work id** — resolving BOTH
/// shapes of the polymorphic `reviews.series_id` in one query.
///
/// [`rating_summary_batch`] keys on the id `reviews` literally holds, which is correct for
/// `updates_feed` (it asks with the same `reader_id` the reader would have posted under) but
/// LOSES ratings for Browse: a MangaDex-anchored work carries a `w_…` `reader_id` while its
/// review may have been filed under the numeric Suwayomi id of whichever source the reader
/// had open. On production that is 2 of the 5 readable ratings. Rather than asking with a
/// union of key shapes — which needs a second `source_series` lookup to even build, and then
/// a fold in Rust to merge two keys onto one work — this pushes the resolution into the
/// query, so it stays ONE round trip and the answer is per-work by construction.
///
/// The subquery is the same union `browse::RATED_PER_WORK_CTE` uses for the RATING sort;
/// both arms yield a WORK id. Keeping them structurally identical is what makes "sorted by
/// rating" agree with the star each card prints.
async fn rating_summary_by_work_batch(
    pool: &SqlitePool,
    work_ids: &[String],
) -> HashMap<String, RatingSummary> {
    if work_ids.is_empty() {
        return HashMap::new();
    }
    let ph = in_placeholders(work_ids.len());
    let sql = format!(
        "SELECT wid, score, COUNT(*) FROM ( \
             SELECT r.series_id AS wid, r.score FROM reviews r \
               WHERE r.series_id LIKE 'w!_%' ESCAPE '!' \
           UNION ALL \
             SELECT ss.work_id AS wid, r.score FROM reviews r \
               JOIN source_series ss ON ss.source_key = r.series_id \
                                    AND ss.source_type = 'suwayomi' \
         ) WHERE wid IN ({ph}) GROUP BY wid, score"
    );
    let mut q = sqlx::query_as::<_, (String, i64, i64)>(&sql);
    for id in work_ids {
        q = q.bind(id);
    }
    let rows = q.fetch_all(pool).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "rating_summary_by_work_batch query failed");
        Vec::new()
    });
    fold_rating_rows(rows)
}

/// Batched read of the REAL newest-chapter time (`suwayomi_series.latest_chapter_at`,
/// migration 0050) for a page of numeric Suwayomi series ids, already converted to ISO.
/// Ids with no stored value are simply absent — the caller keeps whatever
/// `assemble_series` derived. Non-numeric (`w_…`) ids never match and cost nothing.
async fn suwayomi_latest_chapter_at_batch(
    pool: &SqlitePool,
    ids: &[String],
) -> HashMap<String, String> {
    if ids.is_empty() {
        return HashMap::new();
    }
    let sql = format!(
        "SELECT CAST(id AS TEXT), latest_chapter_at FROM suwayomi_series \
         WHERE latest_chapter_at IS NOT NULL AND id IN ({})",
        in_placeholders(ids.len())
    );
    let mut q = sqlx::query_as::<_, (String, String)>(&sql);
    for id in ids {
        q = q.bind(id);
    }
    q.fetch_all(pool)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "suwayomi_latest_chapter_at_batch failed"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(id, ts)| to_iso(Some(&ts)).map(|iso| (id, iso)))
        .collect()
}

/// Batched `admin_overrides`: one query for all series → per-series overrides.
async fn admin_overrides_batch(
    pool: &SqlitePool,
    ids: &[String],
) -> HashMap<String, AdminOverrides> {
    if ids.is_empty() {
        return HashMap::new();
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        series_id: String,
        #[sqlx(flatten)]
        ov: AdminOverrides,
    }
    let sql = format!(
        "SELECT series_id, override_interval_hours, poll_every_minutes, paused_override, \
         status_override FROM series_admin WHERE series_id IN ({})",
        in_placeholders(ids.len())
    );
    let mut q = sqlx::query_as::<_, Row>(&sql);
    for id in ids {
        q = q.bind(id);
    }
    q.fetch_all(pool)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "admin_overrides_batch query failed"))
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.series_id, r.ov))
        .collect()
}

/// Batched `scan_state`: one query for all series → per-series scan state.
async fn scan_state_batch(
    pool: &SqlitePool,
    ids: &[String],
) -> HashMap<String, crate::scanner::ScanState> {
    if ids.is_empty() {
        return HashMap::new();
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        series_id: String,
        #[sqlx(flatten)]
        state: crate::scanner::ScanState,
    }
    // Same columns as `scanner::SCAN_STATE_SELECT`, plus the series_id key and IN(…).
    let sql = format!(
        "SELECT series_id, avg_interval_hours, known_chapter_count, known_max_chapter, \
         last_scanned_at, next_scan_at, last_new_chapter_at, awaiting_since, known_chapter_ids \
         FROM series_scan_state WHERE series_id IN ({})",
        in_placeholders(ids.len())
    );
    let mut q = sqlx::query_as::<_, Row>(&sql);
    for id in ids {
        q = q.bind(id);
    }
    q.fetch_all(pool)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "scan_state_batch query failed"))
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.series_id, r.state))
        .collect()
}

/// Batched `canonical_alt_titles`: one query for all source keys → per-key alt
/// titles, each ordered by `raw_title` (identical to the single-key path).
async fn canonical_alt_titles_batch(
    pool: &SqlitePool,
    keys: &[String],
) -> HashMap<String, Vec<String>> {
    if keys.is_empty() {
        return HashMap::new();
    }
    let sql = format!(
        "SELECT DISTINCT ss.source_key, wa.raw_title \
         FROM source_series ss \
         JOIN work_alias wa ON wa.work_id = ss.work_id \
         WHERE ss.source_type = 'suwayomi' AND ss.source_key IN ({}) \
         ORDER BY ss.source_key, wa.raw_title",
        in_placeholders(keys.len())
    );
    let mut q = sqlx::query_as::<_, (String, String)>(&sql);
    for k in keys {
        q = q.bind(k);
    }
    let rows = q.fetch_all(pool).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "canonical_alt_titles_batch query failed");
        Vec::new()
    });
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (key, raw_title) in rows {
        map.entry(key).or_default().push(raw_title);
    }
    map
}

/// Batched `canonical_is_nsfw`: one grouped query for all source keys. Fails CLOSED
/// exactly like the single-key path — on a DB error every key is marked NSFW (true)
/// so a transient failure can't surface NSFW content to an opted-out viewer. On
/// success, keys absent from the result are unlinked/SFW (the caller defaults them
/// to false).
async fn canonical_is_nsfw_batch(pool: &SqlitePool, keys: &[String]) -> HashMap<String, bool> {
    if keys.is_empty() {
        return HashMap::new();
    }
    let sql = format!(
        "SELECT ss.source_key, COALESCE(MAX(COALESCE(w.is_nsfw_override, w.is_nsfw)), 0) \
         FROM source_series ss \
         JOIN work w ON w.id = ss.work_id \
         WHERE ss.source_type = 'suwayomi' AND ss.source_key IN ({}) \
         GROUP BY ss.source_key",
        in_placeholders(keys.len())
    );
    let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
    for k in keys {
        q = q.bind(k);
    }
    match q.fetch_all(pool).await {
        Ok(rows) => rows.into_iter().map(|(k, v)| (k, v != 0)).collect(),
        Err(e) => {
            // Fail closed: mark every key NSFW so nothing leaks on a transient error.
            tracing::warn!(error = %e, "canonical_is_nsfw_batch query failed; failing closed");
            keys.iter().map(|k| (k.clone(), true)).collect()
        }
    }
}

/// Batched `canonical_overrides`: one query for all source keys, then keep the first
/// row per key by `w.id` — byte-identical to the single-key `ORDER BY w.id LIMIT 1`.
async fn canonical_overrides_batch(
    pool: &SqlitePool,
    keys: &[String],
) -> HashMap<String, SuwayomiWorkOverrides> {
    if keys.is_empty() {
        return HashMap::new();
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        source_key: String,
        work_id: String,
        content_type_override: Option<String>,
        original_language: Option<String>,
        title_override: Option<String>,
        description_override: Option<String>,
    }
    let sql = format!(
        "SELECT ss.source_key, w.id AS work_id, w.content_type_override, w.original_language, \
                w.title_override, w.description_override \
         FROM source_series ss \
         JOIN work w ON w.id = ss.work_id \
         WHERE ss.source_type = 'suwayomi' AND ss.source_key IN ({}) \
         ORDER BY ss.source_key, w.id",
        in_placeholders(keys.len())
    );
    let mut q = sqlx::query_as::<_, Row>(&sql);
    for k in keys {
        q = q.bind(k);
    }
    let rows = q.fetch_all(pool).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "canonical_overrides_batch query failed");
        Vec::new()
    });
    let mut map: HashMap<String, SuwayomiWorkOverrides> = HashMap::new();
    for r in rows {
        let Row {
            source_key,
            work_id,
            content_type_override,
            original_language,
            title_override,
            description_override,
        } = r;
        // Rows are ordered by (source_key, w.id); keep only the first per key.
        map.entry(source_key)
            .or_insert_with(|| SuwayomiWorkOverrides {
                work_id: Some(work_id),
                content_type_override,
                original_language,
                title_override,
                description_override,
            });
    }
    map
}

/// Apply the S4 search filters to a mapped result set: keep series matching ANY of
/// `genres` (case-insensitive; empty/None = no genre filter) AND whose aggregate
/// user rating falls in `[min_rating, max_rating]`. A `min_rating > 0` excludes
/// unrated series (rating average 0 with 0 reviews). Pure so it's unit-testable.
///
/// Retained (with test coverage) for the genre/rating-filtered result path; the FTS
/// text-search path (AD-5) deliberately doesn't call it — see the `search` resolver.
#[cfg_attr(not(test), allow(dead_code))]
fn apply_search_filters(
    items: Vec<Series>,
    genres: Option<&[String]>,
    min_rating: Option<f64>,
    max_rating: Option<f64>,
) -> Vec<Series> {
    let want: Vec<String> = genres
        .map(|g| {
            g.iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    items
        .into_iter()
        .filter(|s| {
            if !want.is_empty() {
                let have: Vec<String> = s.genres.iter().map(|g| g.to_lowercase()).collect();
                if !want.iter().any(|w| have.iter().any(|h| h == w)) {
                    return false;
                }
            }
            let avg = s.rating.average;
            if let Some(min) = min_rating {
                if avg < min {
                    return false;
                }
            }
            if let Some(max) = max_rating {
                if avg > max {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// Group raw cross-source chapter rows (S2) into one `AggregatedChapter` per
/// chapter number, collecting each source that provides it, ascending by number.
/// Pure so the dedupe/ordering is unit-testable without a DB. A number is bucketed
/// by `round(number*100)` so "10.5" ≠ "10" but float noise can't split a number.
fn group_aggregated_chapters(rows: Vec<catalog::WorkChapterRow>) -> Vec<AggregatedChapter> {
    use std::collections::BTreeMap;
    let mut by_num: BTreeMap<i64, AggregatedChapter> = BTreeMap::new();
    for r in rows {
        let key = (r.number * 100.0).round() as i64;
        let entry = by_num.entry(key).or_insert_with(|| AggregatedChapter {
            number: r.number,
            title: r.title.clone(),
            sources: Vec::new(),
        });
        // Keep the first non-empty title we see for the number.
        if entry.title.as_deref().unwrap_or("").is_empty() {
            entry.title = r.title.clone();
        }
        entry.sources.push(ChapterSource {
            source_type: r.source_type,
            source_id: r.source_id,
            suwayomi_manga_id: r.suwayomi_manga_id.map(ID),
            chapter_id: ID(r.chapter_id),
            scanlator: r.scanlator,
        });
    }
    by_num.into_values().collect()
}

/// S1: resolve one Suwayomi series DB-first — return the cached row when it's still
/// fresh; on a stale hit or a miss, revalidate upstream under a single-flight lock
/// (so N concurrent readers of the same id trigger ONE fetch), and fall back to the
/// stale cached row if the refetch fails (the reader never hard-fails on upstream
/// trouble).
/// The aggregate bucket key for a chapter number (matches `group_aggregated_chapters`
/// and the `chapter_override.chapter_key` column).
fn chapter_key(number: f64) -> String {
    ((number * 100.0).round() as i64).to_string()
}

/// Admin chapter overrides for a work, keyed by aggregate bucket key → (hidden,
/// title_override). Empty (and best-effort) when the work has none.
async fn chapter_overrides(
    pool: &SqlitePool,
    work_id: &str,
) -> std::collections::HashMap<String, (bool, Option<String>)> {
    #[derive(sqlx::FromRow)]
    struct Row {
        chapter_key: String,
        hidden: i64,
        title_override: Option<String>,
    }
    sqlx::query_as::<_, Row>(
        "SELECT chapter_key, hidden, title_override FROM chapter_override WHERE work_id = ?",
    )
    .bind(work_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| (r.chapter_key, (r.hidden != 0, r.title_override)))
    .collect()
}

/// Resolve a catalog series id (numeric Suwayomi id or `w_` canonical id) to the
/// canonical work id its admin overrides live on. `None` when the series isn't
/// catalogued (numeric id with no `source_series` row, or an unknown `w_` id).
async fn resolve_work_id(pool: &SqlitePool, series_id: &str) -> Option<String> {
    if series_id.starts_with("w_") {
        return sqlx::query_scalar::<_, String>("SELECT id FROM work WHERE id = ?")
            .bind(series_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    }
    sqlx::query_scalar::<_, String>(
        "SELECT work_id FROM source_series \
         WHERE source_type = 'suwayomi' AND source_key = ? ORDER BY work_id LIMIT 1",
    )
    .bind(series_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

/// Whether `series_id` names something the catalogue actually knows: a canonical `w_`
/// work, a Suwayomi `source_series` mapping, or a cached `suwayomi_series` row. Used to
/// reject junk ids on the unauthenticated `recordView` write before they can seed the
/// view tables. Fails CLOSED (false) on a DB error — a counter write is best-effort and
/// must never be the thing that trusts a broken query.
async fn known_series_id(pool: &SqlitePool, series_id: &str) -> bool {
    if resolve_work_id(pool, series_id).await.is_some() {
        return true;
    }
    // A numeric id may be a cached Suwayomi series that isn't catalogued yet.
    let Ok(n) = series_id.parse::<i64>() else {
        return false;
    };
    sqlx::query_scalar::<_, i64>("SELECT 1 FROM suwayomi_series WHERE id = ?")
        .bind(n)
        .fetch_optional(pool)
        .await
        .map(|r| r.is_some())
        .unwrap_or(false)
}

/// Whether `chapter_id` names a chapter we know: a mirrored MangaDex chapter
/// (`chapter.external_id`, a uuid) or a cached Suwayomi chapter (numeric id). Used to
/// stop comment threads being opened on arbitrary ids. Fails CLOSED on a DB error.
async fn known_chapter_id(pool: &SqlitePool, chapter_id: &str) -> bool {
    if let Ok(n) = chapter_id.parse::<i64>() {
        return sqlx::query_scalar::<_, i64>("SELECT 1 FROM suwayomi_chapter WHERE id = ?")
            .bind(n)
            .fetch_optional(pool)
            .await
            .map(|r| r.is_some())
            .unwrap_or(false);
    }
    sqlx::query_scalar::<_, i64>("SELECT 1 FROM chapter WHERE external_id = ? LIMIT 1")
        .bind(chapter_id)
        .fetch_optional(pool)
        .await
        .map(|r| r.is_some())
        .unwrap_or(false)
}

/// Load a canonical work, following a `work_redirect` (migration 0056) when the id was
/// retired by a merge. `merge_works_ex` physically DELETES the losing `work` row, so
/// every bookmark / cached reader URL / shared link minted against it used to 404
/// forever — production logs show this as recurring
/// `error=No such work path=[Field("canonicalSeries")]`.
///
/// The returned work carries the SURVIVOR's id in `work_id`, so callers that map it
/// through `map_canonical_series` hand the client the new id and it self-corrects.
///
/// Exactly ONE hop: `merge_works_ex` rewrites any redirect pointing at the work it is
/// about to delete, so A->B->C is stored collapsed as A->C and a redirect target is
/// never itself a redirect source. The single lookup below is the defensive bound — no
/// loop, so a cycle is structurally impossible even if that invariant ever broke.
///
/// A redirect-table read failure is downgraded to "not found" (logged): the redirect is
/// a recovery path, and it must never convert a plain 404 into a request error.
async fn load_work_following_redirect(
    pool: &SqlitePool,
    work_id: &str,
) -> Result<Option<catalog::CanonicalWork>> {
    if let Some(w) = catalog::load_canonical_work(pool, work_id)
        .await
        .map_err(gql_err)?
    {
        return Ok(Some(w));
    }
    let redirected = match catalog::redirect_work_id(pool, work_id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(work_id, error = %e, "work_redirect lookup failed");
            None
        }
    };
    let Some(new_id) = redirected else {
        return Ok(None);
    };
    tracing::debug!(from = work_id, to = %new_id, "followed work_redirect");
    catalog::load_canonical_work(pool, &new_id)
        .await
        .map_err(gql_err)
}

/// Reload a work as a `Series` in the CALLER's id shape (`w_` canonical vs a numeric
/// Suwayomi id) so an admin edit updates the console in place. Mirrors the tail of
/// `update_series_metadata`, factored out for the alias mutations.
async fn reload_series_in_shape(
    st: &AppState,
    ctx: &Context<'_>,
    series_id: &str,
    work_id: &str,
) -> Result<Series> {
    if series_id.starts_with("w_") {
        // Follow a redirect: an alias edit can trigger an auto-merge that folds the
        // edited work into a different survivor, and reloading the id the caller sent
        // would then fail. The reload returns the survivor (and its id).
        let work = load_work_following_redirect(&st.pool, work_id)
            .await?
            .ok_or_else(|| Error::new("No such work"))?;
        let chapters = catalog::load_canonical_chapters(&st.pool, &work.work_id)
            .await
            .map_err(gql_err)?;
        let user = current_user(ctx).await;
        Ok(map_canonical_series(
            &st.pool,
            user.as_ref().map(|u| u.id.as_str()),
            work,
            catalog::main_chapter_count_str(&chapters) as i32,
        )
        .await)
    } else {
        let n = series_id.parse::<i64>().map_err(gql_err)?;
        let m = resolve_series_cached(st, n).await.map_err(gql_err)?;
        Ok(map_series(st, m).await)
    }
}

async fn resolve_series_cached(st: &AppState, id: i64) -> anyhow::Result<SuwayomiManga> {
    // Fast path: a fresh cache hit needs no lock and no upstream call.
    let stale = match crate::series_cache::get_series_fresh(&st.pool, id).await? {
        Some((m, true)) => return Ok(m),
        Some((m, false)) => Some(m), // stale — refresh below, keep as fallback
        None => None,                // miss
    };

    // Single-flight: collapse concurrent misses/refreshes for this id.
    let lock = st.series_inflight.lock_handle(id);
    let _guard = lock.lock().await;

    // Re-check after acquiring: a task we queued behind may have just refreshed it.
    if let Some((m, true)) = crate::series_cache::get_series_fresh(&st.pool, id).await? {
        return Ok(m);
    }

    match st.suwayomi.series(id).await {
        Ok(m) => {
            let _ = crate::series_cache::put_series(&st.pool, &m).await;
            // The live-fetched manga REPLACES the cached row we were holding, and the wire
            // shape has no `latestChapterAt` — so from here on the only newest-chapter
            // time we have is the one in `suwayomi_series` (migration 0050), one SELECT
            // away. Without this, every series older than the 6h metadata TTL lost its
            // real release time on refresh and the display path fell back to the poll
            // clock. Leaves an already-populated manga alone and yields None for a
            // never-chaptered series — never a clock value.
            let mut m = m;
            crate::series_cache::hydrate_latest_chapter_at(&st.pool, &mut m).await;
            Ok(m)
        }
        // Refetch failed: serve the stale cached row rather than erroring the reader.
        Err(e) => match stale {
            Some(m) => Ok(m),
            None => Err(e),
        },
    }
}

/// S1: resolve one series' chapter list DB-first — cached rows when present AND
/// fresh, else revalidate upstream under a single-flight lock and cache. An empty
/// cache is a miss (a series with genuinely zero chapters re-checks the source until
/// scanned). A refetch failure falls back to the stale cached rows. The returned
/// rows carry GLOBAL chapter state; the per-viewer `suwayomi_progress` overlay is
/// applied downstream by the caller and is unaffected by this freshness gate.
async fn resolve_chapters_cached(
    st: &AppState,
    manga_id: i64,
) -> anyhow::Result<Vec<SuwayomiChapter>> {
    let cached = crate::series_cache::get_chapters(&st.pool, manga_id).await?;
    if !cached.is_empty() {
        let last = crate::series_cache::chapters_last_fetched(&st.pool, manga_id).await?;
        if crate::series_cache::is_fresh(last.as_deref(), crate::series_cache::CHAPTERS_TTL_SECS) {
            return Ok(cached);
        }
    }

    // Single-flight: collapse concurrent misses/refreshes for this manga id.
    let lock = st.chapters_inflight.lock_handle(manga_id);
    let _guard = lock.lock().await;

    // Re-check after acquiring: a queued-behind task may have just refreshed.
    let cached = crate::series_cache::get_chapters(&st.pool, manga_id).await?;
    if !cached.is_empty() {
        let last = crate::series_cache::chapters_last_fetched(&st.pool, manga_id).await?;
        if crate::series_cache::is_fresh(last.as_deref(), crate::series_cache::CHAPTERS_TTL_SECS) {
            return Ok(cached);
        }
    }

    match st.suwayomi.chapters(manga_id).await {
        Ok(list) => {
            let _ = crate::series_cache::put_chapters(&st.pool, manga_id, &list).await;
            Ok(list)
        }
        // Refetch failed: serve stale cached rows if we have any, else propagate.
        Err(e) => {
            if !cached.is_empty() {
                Ok(cached)
            } else {
                Err(e)
            }
        }
    }
}

async fn map_series_list(st: &AppState, list: Vec<SuwayomiManga>) -> Vec<Series> {
    // Batched: O(1) grouped queries per lookup instead of ~5 per series (see
    // `map_series_batch`). Output order/values are identical to the old per-item map.
    map_series_batch(st, list).await
}

/// Load a batch of series by their (normalised) view keys, preserving order and
/// dropping any that no longer resolve. Numeric keys resolve to Suwayomi series (DB
/// cache first, live fetch on a miss); `w_` keys to canonical works. Turns a Trending
/// ranking (`views::trending_keys`) into displayable series. Bounded (≤ the row size),
/// so the per-key loads never fan out across the whole catalogue.
async fn series_by_keys(st: &AppState, keys: &[String]) -> Vec<Series> {
    let mut out: Vec<Option<Series>> = Vec::with_capacity(keys.len());
    let mut pending: Vec<(usize, SuwayomiManga)> = Vec::new();
    for key in keys {
        if key.starts_with("w_") {
            if let Ok(Some(work)) = catalog::load_canonical_work(&st.pool, key).await {
                let chapters = catalog::load_canonical_chapters(&st.pool, key)
                    .await
                    .unwrap_or_default();
                out.push(Some(
                    map_canonical_series(
                        &st.pool,
                        None,
                        work,
                        catalog::main_chapter_count_str(&chapters) as i32,
                    )
                    .await,
                ));
            } else {
                out.push(None);
            }
        } else if let Ok(n) = key.parse::<i64>() {
            // Resolve DB-first through the shared cached path (single-flight collapsing,
            // result caching, and stale-row fallback) rather than a naive live fetch —
            // this runs on the discovery/home hot path, up to `limit` keys.
            match resolve_series_cached(st, n).await {
                Ok(m) => {
                    pending.push((out.len(), m));
                    out.push(None); // slot filled by the batched map below
                }
                Err(_) => out.push(None),
            }
        } else {
            out.push(None);
        }
    }
    let (indices, mangas): (Vec<usize>, Vec<SuwayomiManga>) = pending.into_iter().unzip();
    for (idx, series) in indices.into_iter().zip(map_series_batch(st, mangas).await) {
        out[idx] = Some(series);
    }
    out.into_iter().flatten().collect()
}

/// Map a canonical `work` (MangaDex-mirrored) onto the shared `Series` shape so the
/// reader reuses its existing series/reader components (CATALOGUE.md §6). The series
/// `id` is the work id (its `w_` prefix distinguishes it from a numeric Suwayomi id,
/// so the reader routes it down the canonical path). Cover URLs point at
/// `uploads.mangadex.org` — the client resolves them through the Worker proxy.
/// Fields Komika doesn't mirror for MangaDex works (genres, ratings, library/scan
/// state) are empty/defaulted; reading is fully functional without them.
async fn map_canonical_series(
    pool: &SqlitePool,
    // `isMarked` is now resolved per-viewer in the ComplexObject impl, so the
    // caller's user id is no longer needed here; kept in the signature so the
    // several call sites don't need to change.
    _user_id: Option<&str>,
    work: catalog::CanonicalWork,
    chapter_count: i32,
) -> Series {
    // Prefer the DB-cached cover (served from our own origin, off the Worker) and
    // fall back to the proxy-ready MangaDex URL when no blob is cached yet.
    let cover_url = crate::cover::work_cover_url(
        &work.work_id,
        work.cover_cached_version,
        work.mangadex_id.as_deref(),
        work.cover_file_name.as_deref(),
    );
    let title = work
        .title_override
        .clone()
        .or_else(|| work.primary_title.clone())
        .unwrap_or_default();
    let mut alt_titles = work.alt_titles;
    alt_titles.retain(|t| t != &title);
    let status = work
        .status
        .as_deref()
        .and_then(komika_status)
        .unwrap_or(SeriesStatus::Unknown);
    // Rating reuses the string-keyed `reviews` aggregate (a `w_` id round-trips with
    // no schema change). Library membership (`isMarked`) is resolved per-viewer in
    // the `#[ComplexObject]` impl against `user_library`, not computed here.
    let rating = rating_summary(pool, &work.work_id).await;
    // chapterCount is the English reader-list count (deduped to one row per number
    // by `load_canonical_chapters`) — Komika serves only English chapters, so that is
    // the number the details page must show. The cross-source aggregate
    // (`aggregate_chapter_count`) is only a FALLBACK for works whose MangaDex spine has
    // 0 English chapters but whose Suwayomi source carries some: without the English
    // spine there is nothing else to count. When the English mirror is non-empty we
    // never fall back, because the Suwayomi branch of the aggregate is not
    // language-filtered (`suwayomi_chapter` has no `lang` column) and would inflate the
    // count with non-English / half / re-numbered chapters (e.g. Tsukimichi: 120 → 151).
    let chapter_count = if chapter_count > 0 {
        chapter_count
    } else {
        catalog::aggregate_chapter_count(pool, &work.work_id)
            .await
            .unwrap_or(0)
            .max(0) as i32
    };
    // Publish time of the newest English chapter — the real chapter-recency signal,
    // exposed as `latest_chapter_at` to match the Suwayomi path. Falls back to the
    // metadata timestamp only when no English chapter is mirrored yet. Computed here
    // (before the struct literal moves `work.work_id`).
    let latest_chapter_at = catalog::latest_english_chapter_at(pool, &work.work_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| work.updated_at.clone());
    let genres = catalog::work_effective_genres(pool, &work.work_id).await;
    let comic_type = resolve_comic_type(
        work.content_type_override.as_deref(),
        work.original_language.as_deref(),
        &genres,
        &title,
    );
    Series {
        id: ID(work.work_id),
        title,
        alt_titles,
        author: work.author,
        artist: work.artist,
        description: work.description_override.clone().or(work.description),
        genres,
        r#type: comic_type,
        status,
        cover_url,
        source_id: "mangadex".to_string(),
        chapter_count,
        is_nsfw: work.is_nsfw_override.unwrap_or(work.is_nsfw),
        rating,
        scan: ScanPolicy {
            avg_interval_hours: 0.0,
            override_interval_hours: None,
            poll_every_minutes: 30,
            paused: false,
            status_override: None,
            paused_override: None,
            poll_every_minutes_override: None,
            last_scanned_at: None,
            next_scan_at: None,
        },
        created_at: work.created_at,
        // Keep `updated_at` as the newest-chapter time on the canonical path too, so the
        // existing canonicalUpdates ordering (which reads it) is unchanged. New readers
        // should prefer `latest_chapter_at`; both hold the same value here.
        updated_at: latest_chapter_at.clone(),
        latest_chapter_at,
        // `series_scan_state` is keyed on numeric Suwayomi ids; a canonical `w_` work
        // never has one, so detection time is always null here. Matches what the old
        // per-row resolver returned for these ids.
        detected_at: None,
    }
}

/// The per-work `work` columns a Browse card needs that `browse_catalogue` does not store.
/// Everything migration 0069 DOES store (title, cover, format, status, chapter count,
/// release time, NSFW flag, catalogue-entry time) is read off the browse row and never
/// looked up again — `created_at` moved onto that row in 0069, so it is no longer here.
#[derive(sqlx::FromRow, Default, Clone)]
struct BrowseWorkExtras {
    id: String,
    author: Option<String>,
    artist: Option<String>,
    description: Option<String>,
    description_override: Option<String>,
}

/// Hydrate a page of `browse_catalogue` rows into `Series`, in FIVE grouped queries for
/// the whole page.
///
/// WHY NOT `map_canonical_series`. That function costs 7-10 STRICTLY SERIAL queries per
/// work (`load_canonical_work` alone is 3, plus chapters, plus
/// `latest_english_chapter_at`, plus genres, plus the rating). At `BROWSE_PAGE_SIZE` = 30
/// that is 210-300 serial round trips for one page; the FTS path, which does exactly that,
/// measured 0.83-8.5 s cold. Migration 0064 exists precisely so a feed page does not have to
/// re-derive per row, 0068 finished the job for the two columns Browse sorts and filters on,
/// and 0069 extended the same row shape to the whole catalogue. So: 1 page scan + 1 memoized
/// COUNT (both in `browse`) + the five below.
///
/// `Series.id` IS `browse_catalogue.reader_id`, NEVER `work_id`. 1,897 of the 115,567 rows
/// have no MangaDex anchor, and `canonicalSeries` hard-rejects those
/// (`if work.mangadex_id.is_none() { return Err("No such work") }`), so handing the grid a
/// bare `w_…` for those works would produce 1,897 cards that 404 on click. 0064 chose
/// `reader_id` for exactly this and 0069 keeps the rule: a MangaDex-anchored work carries its
/// `w_…`, everything else its numeric Suwayomi id, and each opens the page that can actually
/// serve it.
async fn map_browse_rows(st: &AppState, rows: Vec<crate::browse::BrowseRow>) -> Vec<Series> {
    if rows.is_empty() {
        return Vec::new();
    }
    let work_ids: Vec<String> = rows.iter().map(|r| r.work_id.clone()).collect();
    let ph = in_placeholders(work_ids.len());

    // (1) Genres, with the admin > MangaDex > Suwayomi tier precedence already applied —
    //     three grouped queries inside, and the only thing here that is not a single
    //     statement. Same call every other batched feed makes, so a Browse card's chips
    //     match the same work's chips on the home rows.
    let genres = catalog::work_effective_genres_batch(&st.pool, &work_ids).await;
    // (2) Ratings, keyed by work id across both `reviews.series_id` shapes.
    let ratings = rating_summary_by_work_batch(&st.pool, &work_ids).await;
    // (2b) The LIVE NSFW flag, re-read from `work` rather than trusted from the row.
    //
    // `browse_catalogue.is_nsfw` is a MATERIALIZED copy of
    // `COALESCE(is_nsfw_override, is_nsfw)`, and the SQL gate gates on that copy. Taking
    // the copy's value here too made `filter_nsfw` on the browse branch a TAUTOLOGY: it
    // re-tested the very column the WHERE had already pinned, so it could never drop a
    // row, and the comment there claiming it catches a "momentarily stale" copy was
    // false. That matters because the copy really can go stale — `catalog::mark_work_nsfw`
    // escalates `work.is_nsfw` from a source-level signal on the ingest/link path without
    // a `resync_feed_nsfw` follow-up (now added), leaving up to one catalogue-sync
    // interval, 6h by default, in which an opted-out viewer would be served the work.
    //
    // Reading `work` restores the defence the FTS branch already has (its rows go through
    // `map_canonical_series`, which reads the live COALESCE). `nsfw_work_ids` fails
    // CLOSED — a query error yields an empty set, so this cannot itself unmask anything
    // — and the SQL gate remains the primary filter; this only catches the stale window.
    //
    // Costs one indexed `IN` query per page, and can leave a page shorter than `total`
    // when it does fire. That is the right trade for a content gate: the alternative is
    // serving the row.
    let nsfw_live = nsfw_work_ids(&st.pool, &work_ids).await;
    // (3) The `work` columns 0064 does not store.
    let extras_sql = format!(
        "SELECT id, author, artist, description, description_override \
         FROM work WHERE id IN ({ph})"
    );
    let mut q = sqlx::query_as::<_, BrowseWorkExtras>(&extras_sql);
    for id in &work_ids {
        q = q.bind(id);
    }
    let extras: HashMap<String, BrowseWorkExtras> = q
        .fetch_all(&st.pool)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "browse: work extras query failed"))
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect();
    // (4) Alt titles. The reader's `SeriesFields` fragment selects `altTitles`, so dropping
    //     them here would be a silent regression against the feed this replaces (which got
    //     them from `map_series_batch`'s own alias batch).
    let alias_sql =
        format!("SELECT work_id, raw_title FROM work_alias WHERE work_id IN ({ph}) ORDER BY work_id, raw_title");
    let mut qa = sqlx::query_as::<_, (String, String)>(&alias_sql);
    for id in &work_ids {
        qa = qa.bind(id);
    }
    let mut alts: HashMap<String, Vec<String>> = HashMap::new();
    for (wid, raw) in qa
        .fetch_all(&st.pool)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "browse: alt titles query failed"))
        .unwrap_or_default()
    {
        alts.entry(wid).or_default().push(raw);
    }
    // (5) The scanner's detection time for the numeric cards. `series_scan_state` is
    //     keyed on the NUMERIC Suwayomi id, so only non-`w_` reader_ids can match — a
    //     canonical card's `detectedAt` was always null under the per-row resolver and
    //     stays null here. One grouped SELECT replaces one query per card whenever the
    //     grid selects `detectedAt`.
    let detected: HashMap<String, String> = {
        let scan_ids: Vec<String> = rows
            .iter()
            .filter(|r| !r.reader_id.starts_with("w_"))
            .map(|r| r.reader_id.clone())
            .collect();
        if scan_ids.is_empty() {
            HashMap::new()
        } else {
            let sql = format!(
                "SELECT series_id, last_new_chapter_at FROM series_scan_state \
                 WHERE last_new_chapter_at IS NOT NULL AND series_id IN ({})",
                in_placeholders(scan_ids.len())
            );
            let mut qs = sqlx::query_as::<_, (String, String)>(&sql);
            for id in &scan_ids {
                qs = qs.bind(id);
            }
            qs.fetch_all(&st.pool)
                .await
                .inspect_err(|e| tracing::warn!(error = %e, "browse: detected_at query failed"))
                .unwrap_or_default()
                .into_iter()
                .collect()
        }
    };

    rows.into_iter()
        .map(|r| {
            let ex = extras.get(&r.work_id).cloned().unwrap_or_default();
            // `cover_url` is a ready origin path; the Suwayomi fallback has to be
            // absolutized at READ time because `image_base_url` is runtime config, not data
            // (migration 0064). Same call `map_series` and `updates_feed` make.
            let cover_url = match r.cover_url.as_deref() {
                Some(u) if !u.is_empty() => u.to_string(),
                _ => st.suwayomi.abs(r.suwayomi_thumbnail.as_deref()),
            };
            let mut alt_titles = alts.get(&r.work_id).cloned().unwrap_or_default();
            alt_titles.retain(|t| t != &r.title);
            // The card's recency label. `released_at` IS the newest real upstream release
            // across both halves — the clock 0064 exists to normalize — so it is both
            // `latestChapterAt` and `updatedAt` here, matching what
            // `map_canonical_series` does (it sets `updated_at` to the same value so the
            // existing `canonicalUpdates` ordering is unchanged).
            //
            // NULL for the 67,000 works with no dated chapter (migration 0069), which is a
            // state the two fields express differently. `latestChapterAt` is documented as
            // "empty when the series has no dated chapter cached yet", so it becomes `""` and
            // the reader's `relTime` renders nothing rather than the epoch. `updatedAt` is
            // `String!` and has no empty contract, so it falls back to the work's
            // catalogue-entry time — which is the last thing we actually know about the work,
            // and is why 0069 carries `created_at` on the row.
            let released = r.released_at.map(epoch_ms_to_iso).unwrap_or_default();
            Series {
                // NEVER `work_id` — see the doc comment.
                id: ID(r.reader_id.clone()),
                title: r.title,
                alt_titles,
                author: ex.author,
                artist: ex.artist,
                description: ex.description_override.or(ex.description),
                genres: genres.get(&r.work_id).cloned().unwrap_or_default(),
                // A NULL `comic_type` cannot happen on a committed generation (the rebuild
                // fills it inside its transaction and the scanner's incremental writer
                // fills its own rows), but the column is nullable, so default rather than
                // unwrap.
                r#type: r
                    .comic_type
                    .as_deref()
                    .and_then(comic_type_from_word)
                    .unwrap_or(ComicType::Manga),
                status: r
                    .status
                    .as_deref()
                    .and_then(komika_status)
                    .unwrap_or(SeriesStatus::Unknown),
                cover_url,
                // Derived from the id SHAPE, which is exactly what decides the destination:
                // a `w_…` card opens the canonical (MangaDex-mirrored) page, a numeric one
                // opens the Suwayomi path. The specific Suwayomi `source_id` is not stored
                // on the feed row and is not worth a sixth query for a field the grid does
                // not render.
                source_id: if r.reader_id.starts_with("w_") {
                    "mangadex".to_string()
                } else {
                    "suwayomi".to_string()
                },
                // THE SAME EXPRESSION the CHAPTERS sort orders by. If this came from
                // anywhere else — `main_chapter_count`, the aggregate, `chapter_count` —
                // then "sort by chapters" would visibly disagree with the badge on the card.
                chapter_count: r.en_chapter_count as i32,
                // LIVE flag, not `r.is_nsfw` — see `nsfw_live` above. The OR keeps the
                // materialized value authoritative when it is the stricter of the two, so
                // this can only ever ADD a flag, never clear one.
                is_nsfw: r.is_nsfw || nsfw_live.contains(&r.work_id),
                rating: ratings
                    .get(&r.work_id)
                    .cloned()
                    .unwrap_or_else(RatingSummary::empty),
                // Not stored on the feed and not a Browse concern: scan policy belongs to
                // the series page and the admin console. Same defaults
                // `map_canonical_series` uses.
                scan: ScanPolicy {
                    avg_interval_hours: 0.0,
                    override_interval_hours: None,
                    poll_every_minutes: 30,
                    paused: false,
                    status_override: None,
                    paused_override: None,
                    poll_every_minutes_override: None,
                    last_scanned_at: None,
                    next_scan_at: None,
                },
                created_at: r.created_at.clone(),
                updated_at: if released.is_empty() {
                    r.created_at
                } else {
                    released.clone()
                },
                latest_chapter_at: released,
                detected_at: detected.get(&r.reader_id).cloned(),
            }
        })
        .collect()
}

/// Map a mirrored MangaDex chapter onto the shared `Chapter` shape. The chapter `id`
/// is the MangaDex chapter uuid (the key `canonicalPages` fetches pages with);
/// `series_id` is the work id so navigation stays on the canonical path. Per-user
/// reading state (`progress`, keyed by the chapter uuid) drives resume-at-last-chapter;
/// anonymous / never-read chapters default to unread (CR6).
fn map_canonical_chapter(
    work_id: &str,
    c: catalog::CanonicalChapter,
    progress: Option<(i32, bool)>,
) -> Chapter {
    let number = c
        .number
        .as_deref()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    let (last_page_read, read) = progress.unwrap_or((0, false));
    Chapter {
        id: ID(c.external_id),
        series_id: ID(work_id.to_string()),
        number,
        title: c.title,
        page_count: 0, // unknown until the at-home page list is fetched
        uploaded_at: c.published_at,
        scanlator: None,
        read,
        last_page_read,
        bookmarked: false,
        is_downloaded: false,
    }
}

/// Per-user read state for every chapter of one canonical work, keyed by the MangaDex
/// chapter uuid. One query per series (not per chapter), matching the cost `map_series`
/// already pays. Anonymous viewers get an empty map (all chapters read as unread).
async fn canonical_progress_map(
    pool: &SqlitePool,
    user_id: Option<&str>,
    work_id: &str,
) -> HashMap<String, (i32, bool)> {
    let Some(uid) = user_id else {
        return HashMap::new();
    };
    let rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT chapter_id, last_page_read, read FROM canonical_progress \
         WHERE user_id = ? AND work_id = ?",
    )
    .bind(uid)
    .bind(work_id)
    .fetch_all(pool)
    .await
    // Log (don't swallow) DB errors; still fall back to an empty map gracefully.
    .inspect_err(|e| tracing::warn!(error = %e, work_id, "canonical_progress_map query failed"))
    .unwrap_or_default();
    rows.into_iter()
        .map(|(cid, lpr, rd)| (cid, (lpr as i32, rd != 0)))
        .collect()
}

/// Per-user read state for every chapter of one numeric Suwayomi series, keyed by the
/// numeric chapter id. One query per series (mirrors `canonical_progress_map`).
/// Anonymous viewers get an empty map (all chapters read as unread).
async fn suwayomi_progress_map(
    pool: &SqlitePool,
    user_id: Option<&str>,
    series_id: i64,
) -> HashMap<i64, (i32, bool)> {
    let Some(uid) = user_id else {
        return HashMap::new();
    };
    let rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT chapter_id, last_page_read, read FROM suwayomi_progress \
         WHERE user_id = ? AND series_id = ?",
    )
    .bind(uid)
    .bind(series_id.to_string())
    .fetch_all(pool)
    .await
    // Log (don't swallow) DB errors; still fall back to an empty map gracefully.
    .inspect_err(|e| tracing::warn!(error = %e, series_id, "suwayomi_progress_map query failed"))
    .unwrap_or_default();
    rows.into_iter()
        .filter_map(|(cid, lpr, rd)| {
            cid.parse::<i64>()
                .ok()
                .map(|id| (id, (lpr as i32, rd != 0)))
        })
        .collect()
}

// ---- DB row shapes for social joins ----------------------------------------

#[derive(sqlx::FromRow)]
struct ReviewJoin {
    id: String,
    series_id: String,
    score: i64,
    body: String,
    has_spoiler: i64,
    created_at: String,
    updated_at: String,
    author_id: String,
    author_username: String,
    author_avatar: Option<String>,
}

impl From<ReviewJoin> for Review {
    fn from(r: ReviewJoin) -> Self {
        Review {
            id: ID(r.id),
            series_id: ID(r.series_id),
            author: UserRef {
                id: ID(r.author_id),
                username: r.author_username,
                avatar_url: r.author_avatar,
            },
            score: r.score as i32,
            body: r.body,
            has_spoiler: r.has_spoiler != 0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// Validate a comment target type, returning the canonical `&'static str` to bind.
/// Guards the polymorphic `comments.target_type` against arbitrary values.
fn validate_comment_target(target_type: &str) -> Result<&'static str> {
    match target_type {
        "chapter" => Ok("chapter"),
        "series" => Ok("series"),
        other => Err(Error::new(format!(
            "invalid comment target type: {other:?} (expected \"chapter\" or \"series\")"
        ))),
    }
}

#[derive(sqlx::FromRow)]
struct CommentJoin {
    id: String,
    target_type: String,
    target_id: String,
    parent_id: Option<String>,
    body: String,
    has_spoiler: i64,
    created_at: String,
    author_id: String,
    author_username: String,
    author_avatar: Option<String>,
    media_id: Option<String>,
    media_width: Option<i64>,
    media_height: Option<i64>,
    likes: i64,
    dislikes: i64,
    my_vote: i64,
}

impl From<CommentJoin> for Comment {
    fn from(c: CommentJoin) -> Self {
        Comment {
            id: ID(c.id),
            target_type: c.target_type,
            target_id: ID(c.target_id),
            parent_id: c.parent_id.map(ID),
            author: UserRef {
                id: ID(c.author_id),
                username: c.author_username,
                avatar_url: c.author_avatar,
            },
            body: c.body,
            has_spoiler: c.has_spoiler != 0,
            media_url: c.media_id.as_deref().map(crate::media::comment_media_url),
            media_width: c.media_width.map(|n| n as i32),
            media_height: c.media_height.map(|n| n as i32),
            created_at: c.created_at,
            likes: c.likes as i32,
            dislikes: c.dislikes as i32,
            my_vote: c.my_vote as i32,
        }
    }
}

#[derive(sqlx::FromRow)]
struct AdminUserRow {
    id: String,
    username: String,
    email: String,
    avatar_url: Option<String>,
    is_admin: i64,
    is_banned: i64,
    created_at: String,
}

impl From<AdminUserRow> for AdminUser {
    fn from(r: AdminUserRow) -> Self {
        AdminUser {
            id: ID(r.id),
            username: r.username,
            email: r.email,
            avatar_url: r.avatar_url,
            is_admin: r.is_admin != 0,
            is_banned: r.is_banned != 0,
            created_at: r.created_at,
        }
    }
}

/// Load the editable profile fields not carried on the auth `User` row loader
/// (`display_name`, `bio`, `created_at`). Falls back to empties on any lookup
/// failure so building a session never fails on a cold profile.
async fn user_profile_fields(
    pool: &SqlitePool,
    user_id: &str,
) -> (Option<String>, Option<String>, String) {
    sqlx::query_as::<_, (Option<String>, Option<String>, String)>(
        "SELECT display_name, bio, created_at FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    // Log (don't swallow) DB errors; still fall back to empties gracefully.
    .inspect_err(|e| tracing::warn!(error = %e, user_id, "user_profile_fields query failed"))
    .ok()
    .flatten()
    .unwrap_or((None, None, String::new()))
}

/// Build the client-facing `SessionUser` for a resolved auth `User`, loading its
/// profile fields. Every session-returning path funnels through this so the
/// shape stays consistent.
async fn build_session_user(pool: &SqlitePool, u: &User, show_nsfw: bool) -> SessionUser {
    let (display_name, bio, joined_at) = user_profile_fields(pool, &u.id).await;
    SessionUser {
        id: ID(u.id.clone()),
        username: u.username.clone(),
        display_name,
        bio,
        avatar_url: u.avatar_url.clone(),
        is_admin: u.is_admin != 0,
        show_nsfw,
        joined_at,
    }
}

/// Record one entry in a user's activity feed. Best-effort: a failed insert is
/// logged and swallowed so it can never fail the user's actual action (posting
/// a review/comment, adding to the library).
async fn log_activity(
    pool: &SqlitePool,
    user_id: &str,
    kind: &str,
    target_type: Option<&str>,
    target_id: Option<&str>,
) {
    let res = sqlx::query(
        "INSERT INTO user_activity (id, user_id, kind, target_type, target_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(kind)
    .bind(target_type)
    .bind(target_id)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, kind, "failed to record user activity");
    }
}

// ---- Catalogue / dedup (CATALOGUE.md §4, §6) -------------------------------

/// Outcome of running the dedup matcher for a newly-added Tier-2 source series.
/// `decision` is one of `auto_merge` | `review` | `new`.
#[derive(SimpleObject)]
pub struct MatchResult {
    pub decision: String,
    /// The canonical work the series was linked to (its own new work for `new`).
    pub work_id: String,
    /// The matched canonical work (for `auto_merge` / `review`); absent for `new`.
    pub matched_work_id: Option<String>,
    pub score: Option<f64>,
    pub method: Option<String>,
    pub source_series_id: String,
}

/// One row of the canonical updates feed: a mirrored MangaDex work with its most
/// recent stored chapter (CATALOGUE.md §6). Served from the `chapter` mirror, not a
/// live Suwayomi round-trip. `work_id` opens the work through the canonical reader
/// path (`canonicalSeries`); `cover_url` is a proxy-ready MangaDex cover thumbnail.
#[derive(SimpleObject, sqlx::FromRow)]
pub struct CanonicalUpdate {
    pub work_id: String,
    pub mangadex_id: String,
    pub title: Option<String>,
    pub is_nsfw: bool,
    /// Proxy-ready cover thumbnail URL (`uploads.mangadex.org`), or null if the cover
    /// fileName hasn't been synced yet. The client resolves it through the Worker.
    pub cover_url: Option<String>,
    /// Latest stored chapter number (string — chapters can be "10.5").
    pub latest_chapter: Option<String>,
    pub latest_chapter_title: Option<String>,
    /// Publish time of the latest stored chapter (falls back to ingest time).
    pub latest_at: Option<String>,
}

/// One row of the reader's merged Updates feed (`updatesFeed`), read from the
/// materialized `feed_series_updates` table (migration 0064).
///
/// Declared here rather than in `types.rs` alongside `SeriesPage`, next to the other
/// feed/catalogue objects `mod.rs` already owns (`CanonicalUpdate`, `MatchResult`,
/// `MergeQueuePage`) — it pairs with the resolver and the `FeedSeriesUpdateRow` mapping
/// below, which is where every question about it gets answered.
///
/// It is deliberately NOT a `Series`: the feed is 48k rows and a `Series` costs several
/// per-row lookups to assemble, while the grid renders six scalars. `id` is the
/// READER-OPENABLE id — a `w_…` canonical work id when the work is MangaDex-anchored,
/// else the numeric Suwayomi series id — so the card's link works either way (see the
/// `reader_id` note in the migration).
#[derive(SimpleObject, Clone)]
pub struct UpdateFeedRow {
    pub id: ID,
    /// The canonical work this row is one-per-of; the feed's dedupe key. Distinct from
    /// `id` for a Suwayomi-only work.
    pub work_id: String,
    pub title: String,
    pub cover_url: Option<String>,
    /// Effective format, materialized at refresh time so the format facet is a server
    /// filter over the whole feed rather than over the 20 rows of one page.
    pub r#type: Option<ComicType>,
    /// Chapter NUMBER of the newest mirrored chapter ("10.5"); null on scanner-only rows.
    pub latest_chapter: Option<String>,
    pub latest_chapter_title: Option<String>,
    /// Total chapters known for the series; null on mirror-only rows. The reader labels
    /// `Ch. {latestChapter ?? chapterCount}`, which is what each half already showed.
    pub chapter_count: Option<i32>,
    /// The real upstream release time of the newest chapter, ISO-8601. THE sort key, and
    /// the same instant the card labels with — never our detection time.
    pub released_at: String,
    /// When our scanner noticed, ISO-8601; null on mirror-only rows. Tooltip only.
    pub detected_at: Option<String>,
    /// Aggregate user rating 0-10, or null when unrated. Resolved per page (20 rows), not
    /// materialized: review averages move independently of the feed.
    pub rating: Option<f64>,
    pub is_nsfw: bool,
}

/// A page of the merged Updates feed — the same four-field envelope as `SeriesPage`.
#[derive(SimpleObject, Clone)]
pub struct UpdateFeedPage {
    pub items: Vec<UpdateFeedRow>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

/// DB row shape of `feed_series_updates` (migration 0064). Not a GraphQL type — the
/// resolver maps it to `UpdateFeedRow`, resolving the cover fallback (which needs runtime
/// Suwayomi config) and converting `released_at` back to ISO-8601.
#[derive(sqlx::FromRow)]
struct FeedSeriesUpdateRow {
    work_id: String,
    reader_id: String,
    title: String,
    cover_url: Option<String>,
    suwayomi_thumbnail: Option<String>,
    comic_type: Option<String>,
    latest_chapter: Option<String>,
    latest_chapter_title: Option<String>,
    chapter_count: Option<i64>,
    released_at: i64,
    detected_at: Option<String>,
    is_nsfw: bool,
}

/// Epoch milliseconds → ISO-8601 UTC, for a column that is only numeric because the two
/// clocks it merges are stored in incompatible TEXT encodings (see migration 0064).
///
/// An out-of-range value yields the epoch rather than panicking: this is a display
/// timestamp on a cache row, and a corrupt one must not take down the whole feed.
fn epoch_ms_to_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp_millis(0).expect("epoch is in range"))
        .to_rfc3339()
}

/// A pending mid-confidence match awaiting manual admin review.
///
/// `sourceSeriesId` is a `source_series` row id, NOT a work id — it is useless for
/// looking the entry up. `sourceWorkId` is the work that series belongs to, and is
/// what the console links/expands on. Both sides carry a cover URL so the reviewer
/// can eyeball the pair before deciding; either may be empty (a Suwayomi-only work
/// with no cached cover has nothing to show).
#[derive(SimpleObject)]
pub struct MergeCandidate {
    pub id: String,
    pub source_series_id: String,
    pub source_work_id: String,
    pub candidate_work_id: String,
    pub candidate_title: Option<String>,
    pub source_title: Option<String>,
    pub source_cover_url: String,
    pub candidate_cover_url: String,
    pub score: f64,
    pub method: String,
    pub status: String,
    pub created_at: String,
}

/// Raw `merge_queue` row: both works' cover inputs, resolved to URLs on the way out
/// (the same shape `cover_issues` uses — `work_cover_url` needs the cached version,
/// the MangaDex anchor and the cover file name, none of which belong in the GraphQL
/// type).
#[derive(sqlx::FromRow)]
struct MergeCandidateRow {
    id: String,
    source_series_id: String,
    source_work_id: String,
    candidate_work_id: String,
    candidate_title: Option<String>,
    source_title: Option<String>,
    source_cover_version: Option<i64>,
    source_cover_file_name: Option<String>,
    source_mangadex_id: Option<String>,
    candidate_cover_version: Option<i64>,
    candidate_cover_file_name: Option<String>,
    candidate_mangadex_id: Option<String>,
    score: f64,
    method: String,
    status: String,
    created_at: String,
}

/// One side of a dedup decision, expanded — everything the reviewer needs to tell
/// two similar-looking entries apart without leaving the queue. Admin-only, and
/// deliberately NOT `canonicalSeries`: that resolver refuses any work without a
/// MangaDex anchor and NSFW-gates on viewer prefs, which is most of what lands in
/// this queue. Reads straight off `work` so a Suwayomi-only entry still expands.
#[derive(SimpleObject)]
pub struct WorkReviewDetail {
    pub work_id: ID,
    pub title: Option<String>,
    pub cover_url: String,
    pub author: Option<String>,
    pub artist: Option<String>,
    /// `Int` (i32); the DB column is INTEGER — cast on read.
    pub year: Option<i32>,
    pub status: Option<String>,
    pub content_rating: Option<String>,
    pub original_language: Option<String>,
    pub is_nsfw: bool,
    pub description: Option<String>,
    /// Distinct chapter numbers mirrored across every source of this work — a
    /// coarse "how much is here" signal, not the reader's chapter count.
    pub chapter_count: i32,
    /// Alternative titles from `work_alias`, capped: a heavily-merged work can
    /// carry hundreds and the panel only needs enough to recognise the entry.
    pub alt_titles: Vec<String>,
    /// Every catalogued source mapping, NSFW included (admin-gated resolver).
    pub sources: Vec<WorkSource>,
}

/// Raw `work_review_detail` row (the computed cover URL and the joined lists are
/// assembled by the resolver).
#[derive(sqlx::FromRow)]
struct WorkReviewDetailRow {
    title: Option<String>,
    author: Option<String>,
    artist: Option<String>,
    year: Option<i64>,
    status: Option<String>,
    content_rating: Option<String>,
    original_language: Option<String>,
    is_nsfw: i64,
    description: Option<String>,
    cover_cached_version: Option<i64>,
    cover_file_name: Option<String>,
    mangadex_id: Option<String>,
}

/// A page of dedup review candidates (mirrors the other admin `*Page` envelopes).
#[derive(SimpleObject)]
pub struct MergeQueuePage {
    pub items: Vec<MergeCandidate>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

/// A work whose cover the crawl couldn't process (admin "Bugs" panel). `reason` is
/// a machine code (`too_large` / `unsupported` / `empty` / `encode` / `store`);
/// `coverUrl` is the current best-effort fallback (may be empty for a Suwayomi-only
/// work with no cached cover) so the admin can see what's there before replacing it.
#[derive(SimpleObject)]
pub struct CoverIssue {
    pub work_id: ID,
    pub title: Option<String>,
    pub cover_url: String,
    pub reason: String,
    pub detail: Option<String>,
    pub attempts: i32,
    pub first_seen: String,
    pub last_seen: String,
}

/// A page of cover issues (mirrors the other admin `*Page` envelopes).
#[derive(SimpleObject)]
pub struct CoverIssuePage {
    pub items: Vec<CoverIssue>,
    pub page: i32,
    pub has_next_page: bool,
    pub total: Option<i32>,
}

/// Row shape for the cover-issue listing join (DB columns; `cover_url` is derived).
#[derive(sqlx::FromRow)]
struct CoverIssueRow {
    work_id: String,
    title: Option<String>,
    reason: String,
    detail: Option<String>,
    attempts: i64,
    first_seen: String,
    last_seen: String,
    cover_cached_version: Option<i64>,
    mangadex_id: Option<String>,
    cover_file_name: Option<String>,
}

/// Compute the home-feed rails from scratch — the [`DISCOVERY_CACHE`] miss handler.
///
/// Split from the resolver so the read-through logic (key choice, single-flight,
/// store) sits directly beside the NSFW-posture resolution it must agree with. Every
/// input here is GLOBAL state — the persisted catalogue (`series_cache`), the view
/// tables behind Trending, the live Suwayomi fallback for a fresh install — and the
/// only viewer-dependent input is `show_nsfw`, passed explicitly because it is BOTH a
/// compute input and the cache key.
///
/// NSFW GATING: the SQL predicates below and the `filter_nsfw` backstops all derive
/// from this one parameter. A caller that computed with one posture but cached under
/// another would be the leak scenario — impossible while the resolver passes the same
/// bound variable to both (see `feed_cache_key`).
async fn compute_discovery(st: &AppState, show_nsfw: bool) -> Result<Vec<DiscoveryFeed>> {
    // S1: serve from the DB cache once the catalogue has been persisted (the
    // admin "save everything" / ingest / scan populate it), so the home page
    // reads from SQLite instead of a live source browse. Fall back to a live
    // browse only while the cache is still empty (fresh install), caching what
    // it fetches so the next load is fast.
    // `recent` = titles most recently ADDED to our catalogue (ordered by
    // first-persist time), distinct from `latest` (upstream recently-updated).
    // Resolved BEFORE the queries so the NSFW gate can be pushed into SQL ahead of
    // LIMIT, rather than trimming an already-truncated page (see series_cache).
    let (popular, latest, recent) = if crate::series_cache::count(&st.pool)
        .await
        .map_err(gql_err)?
        > 0
    {
        let lib = crate::series_cache::library(&st.pool, PAGE_SIZE, show_nsfw)
            .await
            .map_err(gql_err)?;
        let recent = crate::series_cache::recently_added(&st.pool, PAGE_SIZE, show_nsfw)
            .await
            .map_err(gql_err)?;
        (lib, Vec::new(), recent)
    } else {
        // Cold path (pre-cache, fresh install only): overlap the two independent
        // source fetches — Popular still propagates its error via `?`, Latest stays
        // best-effort. The catalogue is normally populated, so this branch is rare.
        let (popular_res, latest_res) = tokio::join!(
            st.suwayomi.fetch_source(FetchType::Popular, 1, None),
            st.suwayomi.fetch_source(FetchType::Latest, 1, None),
        );
        let popular = popular_res.map_err(gql_err)?.1;
        for m in &popular {
            let _ = crate::series_cache::put_series(&st.pool, m).await;
        }
        let latest = latest_res.map(|r| r.1).unwrap_or_default();
        // Pre-cache (fresh install) there's no catalogue-insertion history yet;
        // the source "Latest" is the best available proxy for "newly added".
        let recent = latest.clone();
        (popular, latest, recent)
    };

    // `show_nsfw` is resolved above, before the cached queries. The cached path has
    // already gated in SQL; these calls are what protect the COLD path (a live
    // source browse on a fresh install), where no SQL gate ran. Re-filtering an
    // already-filtered list is a no-op, so both paths stay safe.
    let popular = filter_nsfw(show_nsfw, map_series_list(st, popular).await);
    let latest = filter_nsfw(show_nsfw, map_series_list(st, latest).await);
    let recent = filter_nsfw(show_nsfw, map_series_list(st, recent).await);

    // Trending = the 10 most-viewed series over the LAST 24 HOURS (the real
    // popularity signal from `recordView`, replacing the old "first 6 of Popular").
    // During cold start — before reads accumulate — this is empty, so pad with
    // Popular to keep the row populated; it becomes fully view-ranked as views come
    // in. Dedup by id AND title: a series whose views split across two identities
    // (its `w_` work and its numeric source — see `views::view_key`) would otherwise
    // resolve to two cards with the same title, so title-dedup collapses them.
    let trending = {
        let keys: Vec<String> = crate::views::trending_keys(&st.pool, 10)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let ranked = filter_nsfw(show_nsfw, series_by_keys(st, &keys).await);
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut seen_titles: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut items: Vec<Series> = Vec::with_capacity(10);
        for s in ranked.into_iter().chain(popular.iter().cloned()) {
            if items.len() >= 10 {
                break;
            }
            let title_key = s.title.trim().to_lowercase();
            let dup = seen_ids.contains(&s.id.0)
                || (!title_key.is_empty() && seen_titles.contains(&title_key));
            if dup {
                continue;
            }
            seen_ids.insert(s.id.0.clone());
            if !title_key.is_empty() {
                seen_titles.insert(title_key);
            }
            items.push(s);
        }
        items
    };

    let mut feeds = vec![
        DiscoveryFeed {
            kind: DiscoveryFeedKind::Popular,
            title: "Popular".into(),
            genre: None,
            items: popular.clone(),
        },
        DiscoveryFeed {
            kind: DiscoveryFeedKind::Trending,
            title: "Trending".into(),
            genre: None,
            items: trending,
        },
    ];
    if !latest.is_empty() {
        feeds.push(DiscoveryFeed {
            kind: DiscoveryFeedKind::RecentlyUpdated,
            title: "Latest Updates".into(),
            genre: None,
            items: latest.clone(),
        });
    }
    if !recent.is_empty() {
        feeds.push(DiscoveryFeed {
            kind: DiscoveryFeedKind::RecentlyAdded,
            title: "Latest Added".into(),
            genre: None,
            items: recent,
        });
    }
    Ok(feeds)
}

/// P2.1: the whole series/reader page in ONE round trip. The reader used to fire
/// `canonicalSeries` + `canonicalChapters` + `workSources` in parallel and then a
/// per-translator `chapters` N+1 (6–12 sequential HTTP round trips per page view),
/// with `load_work_following_redirect` re-run by every one of them. This bundle
/// follows the redirect ONCE and answers everything from the effective work.
///
/// Declared here rather than in `types.rs` because the name `SeriesPage` is taken
/// there by the pagination envelope; this is the page-view bundle.
#[derive(SimpleObject)]
#[graphql(complex)]
pub struct SeriesPageBundle {
    /// The EFFECTIVE canonical id (the survivor after a `work_redirect` merge) —
    /// every id-keyed write the client does next must use this, not the requested id.
    pub work_id: ID,
    /// The canonical (MangaDex-mirrored) series. Null for a federation-only work
    /// (no MangaDex anchor) — the page then renders from a source's own series.
    pub series: Option<Series>,
    /// The canonical spine's chapters (empty when `series` is null).
    pub spine_chapters: Vec<Chapter>,
    /// Every catalogued source mapping WITH its chapters and source-series metadata,
    /// so the translator picker needs no follow-up requests.
    pub sources: Vec<SeriesPageSourceEntry>,
}

/// One source mapping of a [`SeriesPageBundle`], carrying what the old per-source
/// round trips fetched: the mapping itself, its chapter list, and its series row.
#[derive(SimpleObject)]
pub struct SeriesPageSourceEntry {
    pub source: WorkSource,
    /// The source's own chapters. Empty for the MangaDex mapping (the spine carries
    /// those) and for a source whose fetch failed — the old client treated a
    /// per-source failure as an empty list, and the bundle keeps that contract.
    pub chapters: Vec<Chapter>,
    /// The source's series metadata; null when unavailable (fetch failure, or the
    /// non-English serve guard) — the old `series(id)` catch-to-null, server-side.
    pub series: Option<Series>,
}

#[ComplexObject]
impl SeriesPageBundle {
    /// S2 cross-source aggregation (same rows as `aggregatedChapters`), resolved
    /// lazily so the reader route can skip it. No redirect re-follow and no NSFW
    /// re-gate: the bundle only exists for a work that already passed both.
    async fn aggregated(&self, ctx: &Context<'_>) -> Result<Vec<AggregatedChapter>> {
        let st = state(ctx);
        let rows = catalog::work_source_chapters(&st.pool, &self.work_id.0)
            .await
            .map_err(gql_err)?;
        let mut chapters = group_aggregated_chapters(rows);
        let ov = chapter_overrides(&st.pool, &self.work_id.0).await;
        if !ov.is_empty() {
            chapters.retain(|c| !matches!(ov.get(&chapter_key(c.number)), Some((true, _))));
            for c in &mut chapters {
                if let Some((_, Some(t))) = ov.get(&chapter_key(c.number)) {
                    c.title = Some(t.clone());
                }
            }
        }
        Ok(chapters)
    }
}

// ---- Query -----------------------------------------------------------------

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    /// Curated discovery feeds over the federated catalog.
    ///
    /// Read-through over the process-wide [`DISCOVERY_CACHE`]: the payload is
    /// viewer-invariant given the NSFW posture (the cache key), yet cost tens of ms of
    /// CPU per render, so it was the dominant origin cost. Per-viewer fields on the
    /// cards (`isMarked`, `libraryStatus`, `isFavorite`) are resolved lazily per
    /// request AFTER this returns, so a cached rail still reflects the signed-in
    /// viewer's own library. A cold/expired key refills under a single-flight lock
    /// ([`DISCOVERY_INFLIGHT`]) so concurrent misses collapse onto one computation.
    async fn discovery(&self, ctx: &Context<'_>) -> Result<Vec<DiscoveryFeed>> {
        let st = state(ctx);
        // The NSFW posture is BOTH a compute input (SQL gates + `filter_nsfw`) and the
        // cache key — resolving it once here keeps the two from ever disagreeing.
        let show_nsfw = viewer_show_nsfw(ctx).await;
        let key = feed_cache_key(show_nsfw);
        if let Some(hit) = cached_feed(&DISCOVERY_CACHE, key, FEED_TTL) {
            return Ok((*hit).clone());
        }
        // Single-flight the refill: every task that missed awaits ONE per-key lock,
        // then re-checks — exactly one recomputation serves the whole burst.
        let lock = DISCOVERY_INFLIGHT.lock_handle(i64::from(show_nsfw));
        let _guard = lock.lock().await;
        if let Some(hit) = cached_feed(&DISCOVERY_CACHE, key, FEED_TTL) {
            return Ok((*hit).clone());
        }
        let feeds = compute_discovery(st, show_nsfw).await?;
        store_feed(&DISCOVERY_CACHE, key, feeds.clone());
        Ok(feeds)
    }

    /// The reader's Updates feed: library series the adaptive scanner has detected new
    /// chapters for, ordered by the REAL upstream release time of that chapter
    /// (`suwayomi_series.latest_chapter_at`, migration 0050) — newest release first.
    ///
    /// MEMBERSHIP is our scanner's (`series_scan_state.last_new_chapter_at IS NOT
    /// NULL`, written by `scanner::scan_series`), so the feed still means "series WE
    /// noticed a new chapter on" and not Suwayomi's source "Latest" endpoint. But the
    /// ORDER is upstream's, because that is the clock the reader prints on every card.
    /// Detection time is still exposed, unordered, as `Series.detectedAt`.
    async fn updates(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 1)] page: i32,
    ) -> Result<SeriesPage> {
        let st = state(ctx);
        let show_nsfw = viewer_show_nsfw(ctx).await;
        let offset = (page.max(1) as i64 - 1) * PAGE_SIZE;
        // NSFW is filtered in SQL (like canonical_updates) rather than after the page
        // slice, so `total`/`has_next` count only the rows the viewer can see — no skew
        // where a page under-fills yet reports another page (N3). A series is NSFW when
        // its Suwayomi source_series links to a work flagged NSFW.
        //
        // The flag is `COALESCE(is_nsfw_override, is_nsfw)` — the SAME expression
        // `canonical_is_nsfw`/`canonical_is_nsfw_batch` use. Testing raw `is_nsfw` leaked
        // every admin-marked series, because both admin "mark NSFW" mutations
        // (`markSourceNsfw`, `updateSeriesMetadata`) write ONLY `is_nsfw_override`.
        //
        // Keyed on `CAST(suwayomi_series.id AS TEXT)` because the query below is driven
        // from `suwayomi_series` (see the ORDER BY discussion). It was keyed on
        // `sss.series_id` while `series_scan_state` was the driving table.
        //
        // KEEP IN SYNC with `series_cache::NSFW_GATE_SQL`, which this is now identical to
        // token-for-token (only the continuation indentation differs). Both take one bind
        // (`show_nsfw as i64`) and gate the same table for different modules; there is no
        // compile-time link between them, so if you change one, change the other.
        const NSFW_FILTER: &str = "(? = 1 OR NOT EXISTS ( \
             SELECT 1 FROM source_series ss JOIN work w ON w.id = ss.work_id \
             WHERE ss.source_type = 'suwayomi' AND ss.source_key = CAST(suwayomi_series.id AS TEXT) \
               AND COALESCE(w.is_nsfw_override, w.is_nsfw) = 1))";
        // Membership: our scanner has recorded a new-chapter detection for this series.
        // This used to be the DRIVING table and the ORDER BY key; it is now only a
        // predicate, because ordering by it was the bug (see below).
        //
        // `INDEXED BY` is required, not decorative: the only other candidate is the
        // `series_id` PRIMARY KEY autoindex, which answers the equality but not the
        // IS NOT NULL, so every probe fell through to a random table fetch — 846 ms
        // cold / 12.6 ms warm for this test alone at the last page. The planner picks
        // that autoindex even with ANALYZE run and even when handed a two-column or
        // non-partial alternative (measured), so the choice has to be forced. The
        // partial index (migration 0063) carries only the ~1,316 detected rows, 0.02 MiB,
        // and takes the same probe to 20 ms cold / 3.8 ms warm.
        const DETECTED: &str = "EXISTS ( \
             SELECT 1 FROM series_scan_state sss INDEXED BY idx_scan_state_detected_series \
             WHERE sss.series_id = CAST(suwayomi_series.id AS TEXT) \
               AND sss.last_new_chapter_at IS NOT NULL)";
        // Ordered by the REAL UPSTREAM RELEASE TIME of the newest chapter
        // (`suwayomi_series.latest_chapter_at`), newest first — which is precisely the
        // timestamp the reader prints on each card. It used to order by our DETECTION
        // time instead, and the two are uncorrelated: measured live, the top of the feed
        // was labelled "36d", position 10 "74d", and the label column was in no order at
        // all. Sorting by discovery is defensible for a "what's new to us" list, but it
        // is NOT what this feed renders, and a visible ordering that contradicts its own
        // visible labels is a bug however you justify the key.
        //
        // The two clocks cannot be merged with COALESCE, either: `latest_chapter_at` is
        // 13-digit epoch-millis TEXT and `last_new_chapter_at` is ISO-8601 TEXT, so under
        // BINARY collation every '2...' fallback sorts above every '1...' real value.
        //
        // Driven FROM `suwayomi_series` so the whole ORDER BY is served by
        // idx_suwayomi_series_latest_chapter (in_library, latest_chapter_at DESC,
        // id DESC) with no temp B-tree; `id DESC` is the tiebreaker specifically because
        // it is that index's third column (the old `series_id ASC` forced a sort). The
        // tiebreaker is mandatory, not cosmetic: production has 34 groups of rows sharing
        // one `latest_chapter_at`, covering 143 of the 1,316 members, and without a total
        // order LIMIT/OFFSET can repeat or skip rows between pages.
        //
        // NULLS LAST is spelled out rather than left to SQLite's NULL-is-smallest
        // default, both to say it on purpose and because it is a correctness claim: a row
        // with no release time cannot honour the "sort key == visible label" contract in
        // ANY position, so the bottom is the only honest place for it. Not excluded (the
        // series is genuinely in the feed and keeps its `detectedAt`), not COALESCEd (see
        // the encoding mismatch above). 0 of the 1,316 current members are affected;
        // 2,312 of the 13,847 in-library series have a NULL here, none of them detected.
        //
        // `in_library = 1` matches the index's leading column and is also the honest
        // membership test — the feed is the reader's library. Fetch one extra row to
        // compute has_next without a second round-trip.
        let ids: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT CAST(id AS TEXT) FROM suwayomi_series \
             WHERE in_library = 1 AND {DETECTED} AND {NSFW_FILTER} \
             ORDER BY latest_chapter_at DESC NULLS LAST, id DESC LIMIT ? OFFSET ?"
        ))
        .bind(show_nsfw as i64)
        .bind(PAGE_SIZE + 1)
        .bind(offset)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;
        // Counts EXACTLY the row set the ids query pages over — same three predicates,
        // `in_library = 1` included. It previously counted every dated `series_scan_state`
        // row, so a scan-state row for a series no longer in the library inflated `total`
        // and `has_next` above what the pages could ever return.
        let total: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM suwayomi_series \
             WHERE in_library = 1 AND {DETECTED} AND {NSFW_FILTER}"
        ))
        .bind(show_nsfw as i64)
        .fetch_one(&st.pool)
        .await
        .map_err(gql_err)?;
        let has_next = ids.len() as i64 > PAGE_SIZE;

        // Hydrate each id DB-first (S1) — cached row when present, live-fetch on a
        // miss. A series that has since been removed from the source is skipped
        // rather than failing the whole feed. NSFW is already filtered in SQL above.
        // Collect the resolved mangas first, then map the whole page in ONE batched
        // pass (O(1) grouped queries per lookup) instead of ~5 queries per series.
        let mut resolved = Vec::new();
        for id in ids.into_iter().take(PAGE_SIZE as usize) {
            let Ok(n) = id.parse::<i64>() else { continue };
            match resolve_series_cached(st, n).await {
                Ok(m) => resolved.push(m),
                Err(e) => {
                    tracing::warn!(series_id = id, error = %e, "updates: skipping unresolvable series")
                }
            }
        }
        // `latestChapterAt` (the field the reader renders as "released N ago") is filled
        // from `suwayomi_series` inside `map_series_batch` now — it was here, covering only
        // this one feed while federated search and cold browse still showed the poll time.
        let items = map_series_batch(st, resolved).await;
        // Rust-side backstop mirroring `discovery`: the SQL gate above already filtered,
        // so this is a no-op on the happy path — but it means an uncatalogued or
        // newly-flagged series can never slip through to an opted-out viewer.
        let items = filter_nsfw(show_nsfw, items);
        Ok(SeriesPage {
            items,
            page,
            has_next_page: has_next,
            total: Some(total as i32),
        })
    }

    /// Canonical updates feed: recently-updated mirrored MangaDex works with their
    /// latest stored chapter, newest first (CATALOGUE.md §6). Served from the `chapter`
    /// mirror (no live Suwayomi round-trip) and NSFW-filtered by the viewer's
    /// preference. Data-only — see `CanonicalUpdate`.
    ///
    /// `includeNsfw` is the admin-console escape hatch (see
    /// `viewer_show_nsfw_or_admin`): honoured ONLY for an admin, ignored for everyone
    /// else, so the reader's per-viewer gate is unchanged.
    async fn canonical_updates(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 1)] page: i32,
        #[graphql(
            desc = "Admin console only: include NSFW-flagged works regardless of the \
                    admin's own show_nsfw preference. Ignored for non-admin viewers."
        )]
        include_nsfw: Option<bool>,
    ) -> Result<Vec<CanonicalUpdate>> {
        let st = state(ctx);
        let show_nsfw = viewer_show_nsfw_or_admin(ctx, include_nsfw).await;
        let offset = (page.max(1) as i64 - 1) * PAGE_SIZE;
        // Reads the materialized `feed_updates` table (migration 0051, refreshed in the
        // background by catalog::refresh_feed_updates) instead of grouping the whole
        // 800k-row `chapter` table per request. The far-future `published_at` guard and
        // the per-group newest-chapter selection are baked into the refresh.
        //
        // The query is BRANCHED on the NSFW preference rather than `WHERE (? = 1 OR
        // is_nsfw = 0)`: that OR is opaque to the planner, so it can't tell `is_nsfw`
        // is constant and falls back to a temp B-tree sort over the whole table. The
        // anonymous branch pins `is_nsfw = 0`, which lets the composite index
        // (is_nsfw, latest_at DESC, work_id DESC) serve BOTH the filter and the order
        // with no sort — and the anonymous path is the hot, edge-cacheable one.
        let base = "SELECT work_id, mangadex_id, title, is_nsfw, cover_url, \
                    latest_chapter, latest_chapter_title, latest_at FROM feed_updates";
        let sql = if show_nsfw {
            format!("{base} ORDER BY latest_at DESC, work_id DESC LIMIT ? OFFSET ?")
        } else {
            format!(
                "{base} WHERE is_nsfw = 0 ORDER BY latest_at DESC, work_id DESC LIMIT ? OFFSET ?"
            )
        };
        let rows = sqlx::query_as::<_, CanonicalUpdate>(&sql)
            .bind(PAGE_SIZE)
            .bind(offset)
            .fetch_all(&st.pool)
            .await
            .map_err(gql_err)?;
        Ok(rows)
    }

    /// The reader's merged Updates feed, paginated server-side: one row per canonical
    /// work, newest REAL upstream release first, over the materialized
    /// `feed_series_updates` table (migration 0064).
    ///
    /// This supersedes the reader merging page 1 of `updates` with page 1 of
    /// `canonicalUpdates` and capping the union at 60 cards. That merge could not be
    /// paginated at all: a page of the merged list is not page N of either feed, so any
    /// boundary either skips a row or emits it twice (the grid keys its `{#each}` on the
    /// id, and Svelte 5 throws `each_key_duplicate` in PRODUCTION), and the title-dedupe
    /// that ran after the two pages arrived left short pages that made `total` /
    /// `hasNextPage` wrong. Both source feeds stay — `updates` is still the Suwayomi
    /// library feed and `canonicalUpdates` is still the mirror data feed.
    ///
    /// `type` filters by FORMAT server-side, which is only possible because
    /// `comic_type` is materialized (it is otherwise a per-read derivation with no column
    /// to filter on). NSFW is gated by the viewer's own preference — no `includeNsfw`
    /// escape hatch, because this is a reader surface, not an admin one.
    async fn updates_feed(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 1)] page: i32,
        #[graphql(
            desc = "Filter to one format. Server-side over the WHOLE feed, so `total` \
                    and `hasNextPage` describe the filtered set. WEBTOON is folded into \
                    MANHWA and COMIC into MANGA, matching how every reader surface \
                    renders them."
        )]
        r#type: Option<ComicType>,
    ) -> Result<UpdateFeedPage> {
        let st = state(ctx);
        let show_nsfw = viewer_show_nsfw(ctx).await;
        let page = page.max(1);
        let offset = (page as i64 - 1) * PAGE_SIZE;
        // Collapse to the three stored words. Asking for WEBTOON returns the manhwa set
        // rather than nothing, which is the useful reading of the request — the refresh
        // stores the collapsed word precisely so the filter is one indexed equality.
        let type_word = r#type.map(|t| match t {
            ComicType::Manhwa | ComicType::Webtoon => "MANHWA",
            ComicType::Manhua => "MANHUA",
            ComicType::Manga | ComicType::Comic => "MANGA",
        });
        // FOUR SQL SHAPES, not one parameterized one. `WHERE (? = 1 OR is_nsfw = 0)` is
        // opaque to the planner — it cannot tell `is_nsfw` is constant, so it sorts the
        // whole 48k-row table through a temp B-tree to satisfy the ORDER BY.
        // `canonical_updates` documents the same thing and branches for the same reason.
        // The `type` filter is branched too, so as an equality it becomes an index prefix.
        //
        // Migration 0064 carries one index per shape — idx_fsu_order,
        // idx_fsu_type_order, idx_fsu_all_order, idx_fsu_type_all_order respectively.
        // Measured on a copy of production (48,409 rows), EXPLAIN QUERY PLAN for all four
        // is a single `SEARCH/SCAN … USING INDEX idx_fsu_*` with NO
        // `USE TEMP B-TREE FOR ORDER BY`, and page 1 costs 0.06 ms warm / ~30 ms cold on
        // every one of them. Unlike `graphql::updates` this needs no `INDEXED BY` hint:
        // avoiding the sort is enough for the planner to pick the right one here.
        let where_sql = match (show_nsfw, type_word.is_some()) {
            (false, false) => "WHERE is_nsfw = 0",
            (false, true) => "WHERE is_nsfw = 0 AND comic_type = ?",
            (true, false) => "",
            (true, true) => "WHERE comic_type = ?",
        };
        let sql = format!(
            "SELECT work_id, reader_id, title, cover_url, suwayomi_thumbnail, comic_type, \
                    latest_chapter, latest_chapter_title, chapter_count, released_at, \
                    detected_at, is_nsfw \
             FROM feed_series_updates {where_sql} \
             ORDER BY released_at DESC, work_id DESC LIMIT ? OFFSET ?"
        );
        let mut q = sqlx::query_as::<_, FeedSeriesUpdateRow>(&sql);
        if let Some(w) = type_word {
            q = q.bind(w);
        }
        let rows = q
            .bind(PAGE_SIZE)
            .bind(offset)
            .fetch_all(&st.pool)
            .await
            .map_err(gql_err)?;
        // Counts exactly the row set the page query walks — same WHERE, so a filtered
        // feed reports its own total and the pager's "showing 41-60 of N" is honest.
        // Index-only on all four shapes (`… USING COVERING INDEX …`), 0.15-3.5 ms warm,
        // so it is NOT memoized the way `series_cache`'s catalogue count had to be.
        let count_sql = format!("SELECT COUNT(*) FROM feed_series_updates {where_sql}");
        let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
        if let Some(w) = type_word {
            cq = cq.bind(w);
        }
        let total: i64 = cq.fetch_one(&st.pool).await.map_err(gql_err)?;

        // Ratings are resolved for the <=20 ids of this page in ONE grouped query, keyed
        // by `reader_id` because that is the id `reviews.series_id` holds (a `w_…` for a
        // canonical work, the numeric id for a Suwayomi series). Not materialized:
        // review averages move independently of the feed's refresh, and an unrated work
        // must come back as null rather than the "0.0" star the old scanner-half card
        // rendered from `RatingSummary::empty()`.
        let reader_ids: Vec<String> = rows.iter().map(|r| r.reader_id.clone()).collect();
        let ratings = rating_summary_batch(&st.pool, &reader_ids).await;

        let items = rows
            .into_iter()
            .map(|r| {
                // `cover_url` is a ready origin path; the Suwayomi fallback has to be
                // absolutized at READ time because `image_base_url` is runtime config,
                // not data (see the migration). Same call `map_series` makes.
                let cover_url = match r.cover_url.as_deref() {
                    Some(u) if !u.is_empty() => Some(u.to_string()),
                    _ => {
                        let abs = st.suwayomi.abs(r.suwayomi_thumbnail.as_deref());
                        (!abs.is_empty()).then_some(abs)
                    }
                };
                let rating = ratings
                    .get(&r.reader_id)
                    .filter(|s| s.count > 0)
                    .map(|s| s.average);
                UpdateFeedRow {
                    id: ID(r.reader_id),
                    work_id: r.work_id,
                    title: r.title,
                    cover_url,
                    r#type: r.comic_type.as_deref().and_then(comic_type_from_word),
                    latest_chapter: r.latest_chapter,
                    latest_chapter_title: r.latest_chapter_title,
                    chapter_count: r.chapter_count.map(|n| n as i32),
                    // Back to ISO-8601 for the wire. The column is epoch millis only
                    // because the two source clocks are stored in incompatible TEXT
                    // encodings and had to be normalized to be comparable (see 0064);
                    // every other timestamp this API emits is ISO, and the reader parses
                    // it with `Date.parse`.
                    released_at: epoch_ms_to_iso(r.released_at),
                    detected_at: r.detected_at,
                    rating,
                    is_nsfw: r.is_nsfw,
                }
            })
            .collect::<Vec<_>>();

        Ok(UpdateFeedPage {
            items,
            page,
            // From `total`, not from a short page: the LIMIT is exactly PAGE_SIZE here
            // (no +1 probe row), and the count shares the page query's WHERE.
            has_next_page: (page as i64) * PAGE_SIZE < total,
            total: Some(total as i32),
        })
    }

    /// Canonical reader path — a MangaDex-mirrored `work` as a `Series` (CATALOGUE.md §6).
    /// `workId` is the `w_`-prefixed canonical id (distinct from numeric Suwayomi ids).
    /// NSFW works are hidden unless the viewer opted in (same gate as the feeds). Reuses
    /// the `Series` shape so the reader's existing components render it unchanged.
    ///
    /// An id retired by a merge is followed through `work_redirect` to its survivor
    /// (see `load_work_following_redirect`), and the returned `Series.id` is the NEW id
    /// so a stale bookmark self-corrects instead of 404-ing forever.
    async fn canonical_series(&self, ctx: &Context<'_>, work_id: ID) -> Result<Series> {
        let st = state(ctx);
        let work = load_work_following_redirect(&st.pool, &work_id.0)
            .await?
            .ok_or_else(|| Error::new("No such work"))?;
        // Everything below reads the EFFECTIVE id (the survivor after a redirect), not
        // the requested one.
        let work_id = ID(work.work_id.clone());
        // The canonical path is MangaDex-anchored by contract: a backfilled
        // `w_<numeric>` work has no mangadex source (mangadex_id = None) → empty cover
        // and zero chapters. Reject it as not-found rather than serving a shell (CR3).
        if work.mangadex_id.is_none() {
            return Err(Error::new("No such work"));
        }
        if work.is_nsfw_override.unwrap_or(work.is_nsfw) && !viewer_show_nsfw(ctx).await {
            return Err(Error::new("No such work"));
        }
        let chapters = catalog::load_canonical_chapters(&st.pool, &work_id.0)
            .await
            .map_err(gql_err)?;
        let user = current_user(ctx).await;
        Ok(map_canonical_series(
            &st.pool,
            user.as_ref().map(|u| u.id.as_str()),
            work,
            catalog::main_chapter_count_str(&chapters) as i32,
        )
        .await)
    }

    /// Chapters of a canonical work, from the stored `chapter` mirror, deduped to one
    /// row per number (English preferred) and ordered ascending (CATALOGUE.md §6). Same
    /// NSFW gate as `canonicalSeries`, and the same `work_redirect` following.
    async fn canonical_chapters(&self, ctx: &Context<'_>, work_id: ID) -> Result<Vec<Chapter>> {
        let st = state(ctx);
        // Follow a merge redirect, exactly as `canonicalSeries` does. The reader loads a
        // canonical series page by firing `canonicalSeries`, `canonicalChapters`,
        // `aggregatedChapters` and `workSources` in PARALLEL, all with the id from the
        // URL — so following the redirect in only one of them turned a stale bookmark
        // from a clean 404 into a worse failure: title and cover rendered, and the page
        // had no chapters and no translators.
        let work = load_work_following_redirect(&st.pool, &work_id.0)
            .await?
            .ok_or_else(|| Error::new("No such work"))?;
        let work_id = ID(work.work_id.clone());
        // MangaDex-anchored by contract; reject a non-anchored backfilled work (CR3).
        if work.mangadex_id.is_none() {
            return Err(Error::new("No such work"));
        }
        if work.is_nsfw_override.unwrap_or(work.is_nsfw) && !viewer_show_nsfw(ctx).await {
            return Err(Error::new("No such work"));
        }
        let chapters = catalog::load_canonical_chapters(&st.pool, &work_id.0)
            .await
            .map_err(gql_err)?;
        let user = current_user(ctx).await;
        let progress =
            canonical_progress_map(&st.pool, user.as_ref().map(|u| u.id.as_str()), &work_id.0)
                .await;
        Ok(chapters
            .into_iter()
            .map(|c| {
                let p = progress.get(&c.external_id).copied();
                map_canonical_chapter(&work_id.0, c, p)
            })
            .collect())
    }

    /// S2: the aggregated chapter list of a canonical work — every chapter number
    /// available across ALL its sources (installed Suwayomi sources + the MangaDex
    /// mirror), deduped by number, each carrying the per-source availability so the
    /// reader can pick a translator. This is how Solo Leveling shows its asurascans
    /// chapters even though its MangaDex spine has none. Ascending by number.
    async fn aggregated_chapters(
        &self,
        ctx: &Context<'_>,
        work_id: ID,
    ) -> Result<Vec<AggregatedChapter>> {
        let st = state(ctx);
        // Merge-redirect following, as in `canonicalSeries`/`canonicalChapters` — the
        // reader fires all three with the same (possibly retired) id.
        let work = load_work_following_redirect(&st.pool, &work_id.0)
            .await?
            .ok_or_else(|| Error::new("No such work"))?;
        let work_id = ID(work.work_id.clone());
        if work.is_nsfw_override.unwrap_or(work.is_nsfw) && !viewer_show_nsfw(ctx).await {
            return Err(Error::new("No such work"));
        }
        let rows = catalog::work_source_chapters(&st.pool, &work_id.0)
            .await
            .map_err(gql_err)?;
        // (the NSFW gate above uses the effective flag incl. admin override)
        let mut chapters = group_aggregated_chapters(rows);
        // Apply admin chapter overrides: drop soft-hidden chapters and apply renames
        // (non-destructive — the underlying cached rows are untouched).
        let ov = chapter_overrides(&st.pool, &work_id.0).await;
        if !ov.is_empty() {
            chapters.retain(|c| !matches!(ov.get(&chapter_key(c.number)), Some((true, _))));
            for c in &mut chapters {
                if let Some((_, Some(t))) = ov.get(&chapter_key(c.number)) {
                    c.title = Some(t.clone());
                }
            }
        }
        Ok(chapters)
    }

    /// P2.1: the series/reader page in ONE round trip — see [`SeriesPageBundle`].
    /// Follows `work_redirect` once, applies the NSFW gate once, and returns the
    /// canonical series + spine chapters + every source mapping with its chapters
    /// and metadata. Errors "No such work" exactly where `canonicalSeries` +
    /// `workSources` together would have rendered nothing: unknown id, or an NSFW
    /// work for an opted-out viewer. A federation-only work (no MangaDex anchor)
    /// IS served, with `series: null` — unlike `canonicalSeries`, which 404s it —
    /// because its source mappings are still readable.
    async fn series_page(&self, ctx: &Context<'_>, work_id: ID) -> Result<SeriesPageBundle> {
        let st = state(ctx);
        let work = load_work_following_redirect(&st.pool, &work_id.0)
            .await?
            .ok_or_else(|| Error::new("No such work"))?;
        let effective_id = work.work_id.clone();
        if work.is_nsfw_override.unwrap_or(work.is_nsfw) && !viewer_show_nsfw(ctx).await {
            return Err(Error::new("No such work"));
        }
        let show_nsfw = viewer_show_nsfw(ctx).await;
        let user = current_user(ctx).await;
        let user_id = user.as_ref().map(|u| u.id.as_str());

        // Canonical spine — only when MangaDex-anchored. `load_canonical_chapters` is
        // loaded ONCE and feeds both the chapter list and the series' chapterCount
        // (the old resolvers each loaded it independently).
        let (series, spine_chapters) = if work.mangadex_id.is_some() {
            let chapters = catalog::load_canonical_chapters(&st.pool, &effective_id)
                .await
                .map_err(gql_err)?;
            let count = catalog::main_chapter_count_str(&chapters) as i32;
            let progress = canonical_progress_map(&st.pool, user_id, &effective_id).await;
            let mapped: Vec<Chapter> = chapters
                .into_iter()
                .map(|c| {
                    let p = progress.get(&c.external_id).copied();
                    map_canonical_chapter(&effective_id, c, p)
                })
                .collect();
            let s = map_canonical_series(&st.pool, user_id, work, count).await;
            (Some(s), mapped)
        } else {
            (None, Vec::new())
        };

        // Source mappings (already redirect-resolved, so no re-follow needed) with
        // each one's chapters + metadata — this replaces the client's per-translator
        // `chapters` N+1 and the selected translator's `series` follow-up. A work has
        // few mappings, so the loop is bounded; per-source failures degrade to
        // empty/null exactly as the client's old catch handlers did.
        let source_rows = load_work_sources(&st.pool, &effective_id, show_nsfw).await?;
        // First pass: per-source chapters + the raw cached series row. The series
        // row comes FIRST because the non-English serve guard (same as `series(id)`)
        // must veto the chapters too — the old `chapters(seriesId)` path happened to
        // serve a non-English source's chapters, but only because it couldn't see
        // the language; here we can, so nothing of such a source is served.
        let mut assembled: Vec<(WorkSource, Vec<Chapter>, Option<SuwayomiManga>)> =
            Vec::with_capacity(source_rows.len());
        for src in source_rows {
            let mut chapters: Vec<Chapter> = Vec::new();
            let mut manga: Option<SuwayomiManga> = None;
            // The MangaDex mapping's content is the spine; duplicating it per-source
            // would double the payload for nothing.
            if src.source_type != "mangadex" {
                if let Ok(n) = src.source_key.parse::<i64>() {
                    // A failed series fetch keeps meta None but still serves the
                    // chapters (fail open, matching the old client's independent
                    // per-source requests); a non-English source serves neither.
                    let non_english = match resolve_series_cached(st, n).await {
                        Ok(m) => {
                            let ne = m
                                .source
                                .as_ref()
                                .and_then(|s| s.lang.as_deref())
                                .is_some_and(|l| l != "en");
                            if !ne {
                                manga = Some(m);
                            }
                            ne
                        }
                        Err(_) => false,
                    };
                    if !non_english {
                        if let Ok(list) = resolve_chapters_cached(st, n).await {
                            let progress = suwayomi_progress_map(&st.pool, user_id, n).await;
                            chapters = list
                                .into_iter()
                                .map(|c| {
                                    let p = progress.get(&c.id).copied();
                                    map_chapter(c, p)
                                })
                                .collect();
                        }
                    }
                }
            }
            assembled.push((src, chapters, manga));
        }
        // Second pass: ONE batched metadata mapping across every source. Mapping
        // each source through `map_series` individually would run ~9 point queries
        // per source; `map_series_batch` does grouped queries once for the lot.
        let mangas: Vec<SuwayomiManga> =
            assembled.iter().filter_map(|(_, _, m)| m.clone()).collect();
        let mut metas = map_series_batch(st, mangas).await.into_iter();
        let sources = assembled
            .into_iter()
            .map(|(source, chapters, manga)| SeriesPageSourceEntry {
                source,
                chapters,
                series: manga.and_then(|_| metas.next()),
            })
            .collect();

        Ok(SeriesPageBundle {
            work_id: ID(effective_id),
            series,
            spine_chapters,
            sources,
        })
    }

    /// Admin: the raw metadata-override state of a series' canonical work, so the
    /// series-detail editor can show what is pinned vs derived. `workId` is null when
    /// the series isn't catalogued yet (nothing can be pinned without a work).
    async fn series_admin_meta(&self, ctx: &Context<'_>, series_id: ID) -> Result<SeriesAdminMeta> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let Some(work_id) = resolve_work_id(&st.pool, &series_id.0).await else {
            return Ok(SeriesAdminMeta {
                work_id: None,
                title_override: None,
                description_override: None,
                content_type_override: None,
                is_nsfw_override: None,
                tags: Vec::new(),
                has_curated_tags: false,
            });
        };
        #[derive(sqlx::FromRow)]
        struct Row {
            title_override: Option<String>,
            description_override: Option<String>,
            content_type_override: Option<String>,
            is_nsfw_override: Option<i64>,
        }
        let row = sqlx::query_as::<_, Row>(
            "SELECT title_override, description_override, content_type_override, is_nsfw_override \
             FROM work WHERE id = ?",
        )
        .bind(&work_id)
        .fetch_one(&st.pool)
        .await
        .map_err(gql_err)?;
        let curated: Vec<String> =
            sqlx::query_scalar("SELECT tag FROM work_tag WHERE work_id = ? ORDER BY ord, tag")
                .bind(&work_id)
                .fetch_all(&st.pool)
                .await
                .unwrap_or_default();
        let has_curated_tags = !curated.is_empty();
        let tags = if has_curated_tags {
            curated
        } else {
            catalog::work_effective_genres(&st.pool, &work_id).await
        };
        Ok(SeriesAdminMeta {
            work_id: Some(ID(work_id)),
            title_override: row.title_override,
            description_override: row.description_override,
            content_type_override: row
                .content_type_override
                .as_deref()
                .and_then(comic_type_from_word),
            is_nsfw_override: row.is_nsfw_override.map(|v| v != 0),
            tags,
            has_curated_tags,
        })
    }

    /// Admin: a work's aggregated chapters WITH their override state (hidden/renamed),
    /// UNFILTERED — the series-detail editor needs to see and un-hide soft-hidden
    /// chapters, unlike the reader's `aggregatedChapters`.
    async fn work_chapters_admin(
        &self,
        ctx: &Context<'_>,
        work_id: ID,
    ) -> Result<Vec<AdminChapter>> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let rows = catalog::work_source_chapters(&st.pool, &work_id.0)
            .await
            .map_err(gql_err)?;
        let aggregated = group_aggregated_chapters(rows);
        let ov = chapter_overrides(&st.pool, &work_id.0).await;
        Ok(aggregated
            .into_iter()
            .map(|c| {
                let key = chapter_key(c.number);
                let (hidden, title_override) = ov.get(&key).cloned().unwrap_or((false, None));
                let effective_title = title_override.clone().or_else(|| c.title.clone());
                AdminChapter {
                    number: c.number,
                    key,
                    source_title: c.title,
                    title_override,
                    effective_title,
                    hidden,
                    source_count: c.sources.len() as i32,
                }
            })
            .collect())
    }

    /// Ordered page URLs for a mirrored MangaDex chapter, via MangaDex@Home
    /// (CATALOGUE.md §5). `chapterId` is the MangaDex chapter uuid. The URLs are
    /// `*.mangadex.network` hosts the client resolves through the Worker proxy (never
    /// hotlinked). NSFW-gated by the owning work when the chapter is in the mirror.
    async fn canonical_pages(&self, ctx: &Context<'_>, chapter_id: ID) -> Result<Vec<Page>> {
        let st = state(ctx);
        // Gate on the owning work's NSFW flag when we know the chapter; an unknown
        // chapter (not in the mirror) is allowed through to the at-home fetch, which
        // fails cleanly if the id is bogus.
        if let Some(true) = catalog::chapter_owner_is_nsfw(&st.pool, &chapter_id.0)
            .await
            .map_err(gql_err)?
        {
            if !viewer_show_nsfw(ctx).await {
                return Err(Error::new("No such chapter"));
            }
        }
        let urls = st.mangadex.at_home(&chapter_id.0).await.map_err(gql_err)?;
        Ok(urls
            .into_iter()
            .enumerate()
            .map(|(index, source_url)| Page {
                index: index as i32,
                source_url,
                width: None,
                height: None,
            })
            .collect())
    }

    /// The catalogued source mappings for one canonical work, plus each source's
    /// extension coordinates (§2.2) — what a native client needs to install the right
    /// extension and fetch chapters. The MangaDex-native mapping sorts first, then the
    /// rest by recency. Public (mirrors the catalogue reads); an opted-out viewer simply
    /// doesn't see NSFW source mappings.
    async fn work_sources(&self, ctx: &Context<'_>, work_id: ID) -> Result<Vec<WorkSource>> {
        let st = state(ctx);
        let show_nsfw = viewer_show_nsfw(ctx).await;
        // Gate on the OWNING WORK, not just the per-source rows. VERIFIED LEAK: for a
        // work `canonicalSeries` refuses to serve anonymously, this returned the full
        // mapping including the MangaDex UUID — the per-`source_series` gate below only
        // hides NSFW *sources*, never an NSFW *work* served from an SFW source.
        if !show_nsfw && work_is_nsfw(&st.pool, &work_id.0).await {
            return Ok(Vec::new());
        }
        let sources = load_work_sources(&st.pool, &work_id.0, show_nsfw).await?;
        if !sources.is_empty() {
            return Ok(sources);
        }
        // Empty — which is also what a merged-away id looks like here, because
        // `merge_works` repoints the loser's `source_series` rows at the survivor. The
        // reader fires this alongside `canonicalSeries` (which DOES follow the redirect)
        // with the id from the URL, so leaving it un-followed rendered a canonical page
        // with a title and cover but no translator to read from.
        //
        // Consulted only on the empty result, so a live id pays no extra query, and the
        // survivor is re-gated on its OWN NSFW flag before anything is returned.
        let redirected = match catalog::redirect_work_id(&st.pool, &work_id.0).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(work_id = %work_id.0, error = %e, "workSources: redirect lookup failed");
                None
            }
        };
        let Some(new_id) = redirected else {
            return Ok(sources);
        };
        if !show_nsfw && work_is_nsfw(&st.pool, &new_id).await {
            return Ok(Vec::new());
        }
        load_work_sources(&st.pool, &new_id, show_nsfw).await
    }

    /// Batched `workSources`: one `WorkSourceGroup` per requested id, in input order. A
    /// work with no (visible) sources yields an empty `sources` list. NSFW gating is
    /// identical to `workSources`.
    async fn work_sources_batch(
        &self,
        ctx: &Context<'_>,
        work_ids: Vec<ID>,
    ) -> Result<Vec<WorkSourceGroup>> {
        // Public endpoint (H5): cap the input so one anonymous request can't fan out
        // to thousands of serial queries and drain the connection pool.
        const MAX_WORK_IDS: usize = 200;
        if work_ids.len() > MAX_WORK_IDS {
            return Err(Error::new(format!(
                "Too many work ids (max {MAX_WORK_IDS})"
            )));
        }
        let st = state(ctx);
        let show_nsfw = viewer_show_nsfw(ctx).await;
        // Single IN(...) query instead of a serial per-id loop.
        let ids: Vec<String> = work_ids.iter().map(|w| w.0.clone()).collect();
        // Work-level gate, identical to `workSources` (see the leak note there): drop
        // every source of an NSFW work for an opted-out viewer, in one grouped query.
        let nsfw_works = if show_nsfw {
            std::collections::HashSet::new()
        } else {
            nsfw_work_ids(&st.pool, &ids).await
        };
        let mut by_work = load_work_sources_batch(&st.pool, &ids, show_nsfw).await?;
        let groups = work_ids
            .into_iter()
            .map(|work_id| {
                let sources = if nsfw_works.contains(&work_id.0) {
                    Vec::new()
                } else {
                    by_work.remove(&work_id.0).unwrap_or_default()
                };
                WorkSourceGroup { work_id, sources }
            })
            .collect();
        Ok(groups)
    }

    /// Catalogue search + Browse (S4).
    ///
    /// An EMPTY query browses the whole BROWSABLE canonical catalogue — 115,567 works, from
    /// the materialized `browse_catalogue` (migration 0069) — with every filter and sort
    /// applied IN SQL, so `total` and `hasNextPage` describe the filtered set rather than a
    /// slice of it. It used to read `suwayomi_series WHERE in_library = 1`, which was 13,847
    /// works, keyed by numeric Suwayomi id, with one fixed ORDER BY and no format/status/
    /// rating filter possible at all; then `feed_series_updates`, which was 48,567 — the
    /// works with a chapter we can DATE.
    ///
    /// Those two numbers differ by 67,000 works and they are not the obscure tail: MangaDex
    /// removes chapters when a series is licensed or claimed, so "no chapters upstream"
    /// correlates with POPULAR (`Boku no Hero Academia`, `Nausicaä of the Valley of the Wind`,
    /// `Houseki no Kuni`), and 3,239 of them already carry a Suwayomi source that will supply
    /// chapters. `hasChapters: true` is the filter for a reader who only wants what they can
    /// read right now; absent, they are included and sorted last on the recency ordering.
    ///
    /// A TEXT query does full-text search over the canonical `work` catalogue instead (see
    /// below); the structural filters do not apply on that path.
    ///
    /// `genres` matches ANY of the given genres and is capped at
    /// `browse::MAX_GENRE_FILTERS`. `minRating`/`maxRating` filter by the work's aggregate
    /// user rating (0–10; `minRating > 0` excludes unrated works).
    ///
    /// NSFW: `contentRating` NARROWS within the viewer's NSFW posture and can never widen
    /// it — for an opted-out viewer `EROTICA` and `PORNOGRAPHIC` return exactly the
    /// `SUGGESTIVE` set. `includeNsfw` is the admin-console escape hatch (see
    /// `viewer_show_nsfw_or_admin`): honoured ONLY for an admin, ignored for everyone else.
    /// Without it an opted-out admin cannot find — let alone unflag — the ~2,500 mainstream
    /// works currently mis-flagged NSFW.
    #[allow(clippy::too_many_arguments)]
    async fn search(
        &self,
        ctx: &Context<'_>,
        query: String,
        #[graphql(default = 1)] page: i32,
        genres: Option<Vec<String>>,
        min_rating: Option<f64>,
        max_rating: Option<f64>,
        #[graphql(
            desc = "Browse only (empty query): restrict to these formats. WEBTOON folds \
                    into MANHWA and COMIC into MANGA, matching how every reader surface \
                    renders them. Applied over the WHOLE catalogue, so `total` describes \
                    the filtered set."
        )]
        types: Option<Vec<ComicType>>,
        #[graphql(desc = "Browse only (empty query): restrict to one publication status.")]
        status: Option<SeriesStatus>,
        #[graphql(
            desc = "Browse only (empty query): ordering. Defaults to TRENDING (24h views), \
                    which falls back to newest-first for everything unviewed."
        )]
        sort: Option<BrowseSort>,
        #[graphql(
            desc = "Browse only (empty query): the content-rating ceiling, cumulative. \
                    The viewer's NSFW gate DOMINATES this: it can only narrow within \
                    what the gate already allows, never widen past it, so for an \
                    opted-out viewer EROTICA and PORNOGRAPHIC return the SUGGESTIVE set."
        )]
        content_rating: Option<ContentRatingFilter>,
        #[graphql(
            desc = "Admin console only: include NSFW-flagged works regardless of the \
                    admin's own show_nsfw preference. Ignored for non-admin viewers."
        )]
        include_nsfw: Option<bool>,
        #[graphql(
            desc = "Browse only (empty query): `true` returns only works we know a chapter \
                    for, `false` only works we know none for. Omit for the whole browsable \
                    catalogue, which is the default and includes the 67k works whose \
                    chapters were removed upstream (a licensed series) or come from a \
                    non-MangaDex source."
        )]
        has_chapters: Option<bool>,
    ) -> Result<SeriesPage> {
        let st = state(ctx);
        let trimmed = query.trim();
        let show_nsfw = viewer_show_nsfw_or_admin(ctx, include_nsfw).await;
        if trimmed.is_empty() {
            // Collapse to the three words `feed_series_updates.comic_type` stores. Asking
            // for WEBTOON returns the manhwa set rather than nothing, which is the useful
            // reading of the request — the refresh stores the collapsed word precisely so
            // the filter is one indexed equality. Only the FIRST format is honoured: the
            // column holds one value, so an OR over several would forfeit the index prefix
            // (the exact degradation migration 0064's header is about), and the argument is
            // a list only so the client does not need a schema change to gain multi-select
            // once a strategy for it exists.
            let type_word = types
                .as_deref()
                .and_then(|t| t.first().copied())
                .map(collapsed_comic_type_word);
            let status_word = status.map(status_word);
            let rating_filter = content_rating.unwrap_or(ContentRatingFilter::All);
            let tier = content_rating_tier(rating_filter);
            let nsfw_only = content_rating_nsfw_only(rating_filter);
            let genre_list = crate::browse::canonical_genres(&genres.unwrap_or_default());
            let bq = crate::browse::BrowseQuery {
                show_nsfw,
                type_word,
                status_word,
                ratings: tier,
                nsfw_only,
                genres: &genre_list,
                min_rating,
                max_rating,
                has_chapters,
                // TRENDING by default — the only ordering that reflects what people are
                // reading. It degrades to NEWEST for everything unviewed, so a cold view
                // table cannot render Browse empty.
                sort: sort.unwrap_or(BrowseSort::Trending),
                page: page.max(1) as i64,
            };
            let (total, rows) = crate::browse::browse_catalogue(&st.pool, &bq)
                .await
                .map_err(gql_err)?;
            // Rust-side backstop mirroring `discovery`. The SQL gate is `is_nsfw = 0` on a
            // MATERIALIZED copy of `COALESCE(is_nsfw_override, is_nsfw)`; this re-filters
            // on `Series.is_nsfw`, which `map_browse_rows` resolves from the LIVE `work`
            // row (`nsfw_work_ids`) precisely so that this is a real check and not a
            // restatement of the WHERE clause.
            //
            // It was the latter until 2026-07-27: `map_browse_rows` copied the row's own
            // materialized flag, so this filter re-tested the column the gate had already
            // pinned and could not drop anything — while the copy genuinely could be stale
            // (see `mark_work_nsfw`'s callers). There is a VERIFIED past leak on the
            // sibling FTS branch below; this is the same class.
            let items = filter_nsfw(show_nsfw, map_browse_rows(st, rows).await);
            // From `total`, not from a short page: the LIMIT is exactly BROWSE_PAGE_SIZE (no
            // +1 probe row) and the COUNT shares the page query's WHERE verbatim.
            let has_next = (page.max(1) as i64) * BROWSE_PAGE_SIZE < total;
            return Ok(SeriesPage {
                items,
                page,
                has_next_page: has_next,
                total: Some(total as i32),
            });
        }
        // AD-5: text query → full-text search over the canonical `work` catalogue
        // (migration 0052), returning `w_` works ranked by bm25. This replaces the old
        // live fan-out to a Suwayomi source, which was slow, nondeterministic (results
        // depended on which of ~24 sources answered within 8s), ranked only by exact
        // title, and WROTE new rows into the catalogue as a read side effect. Because
        // results are canonical works, opening one shows the translator/source picker —
        // consistent with the home/updates canonical rows.
        //
        // Genre/rating filters are intentionally NOT applied on this path: canonical
        // genre coverage is sparse until MangaDex tags are ingested (AD-1/B7), so a
        // post-fetch genre filter would wrongly empty the results, and any post-fetch
        // filter would corrupt the SQL-computed `total`/pagination. Title-match is the
        // dominant intent of a text query; the browse (empty-query) path keeps filters.
        //
        // SAME `BROWSE_PAGE_SIZE` as the browse branch, and every `has_next` below uses it
        // too. Both branches answer the same `search` field, so the client pages them with
        // one pager and one page number — if they diverged, page 2 of a text search would
        // start 10 rows past where page 1 ended.
        //
        // Interaction with `catalog::RANK_WINDOW` (500), VERIFIED: `search_works_fts` caps
        // `total` at the window and binds `page_size.min(RANK_WINDOW)`, and its `cand` CTE
        // takes the 500 bm25-best matches before the ranking re-sort. So the LAST reachable
        // page is the one whose offset is still inside the window, and it is SHORT because
        // 500 is not a multiple of 30: offset 480 returns rows 481..500, i.e. exactly 20
        // rows on page 17, after which `has_next` is false (17*30 = 510 > 500) and page 18
        // early-returns empty. At the old page size of 20 the same thing happened at page
        // 25 (offset 480, 20 rows) — 500 is a multiple of 20, so the last page was FULL
        // rather than short. Nothing breaks: `total` still bounds `has_next`, so the pager
        // never offers page 18, and a short final page is a state it already handles (the
        // over-range contract is "echo the page, return no rows").
        let (total, ids) = catalog::search_works_fts(
            &st.pool,
            trimmed,
            show_nsfw,
            page.max(1) as i64,
            BROWSE_PAGE_SIZE,
        )
        .await
        .map_err(gql_err)?;
        // Mapping one result costs ~7 serial queries (`load_canonical_work` alone is 3),
        // so a 20-result page used to issue ~140 STRICTLY SERIAL round-trips — measured
        // at 0.83–8.5s cold on the anonymous path. A true batch would need grouped
        // loaders in `catalog`; pipelining them with bounded concurrency gets most of the
        // win here without changing the catalogue layer. `buffered` (not
        // `buffer_unordered`) preserves the bm25 ranking exactly, and the concurrency is
        // held below the pool's 8 connections so a search can't starve everything else.
        const SEARCH_MAP_CONCURRENCY: usize = 4;
        let items: Vec<Series> = {
            use futures::StreamExt as _;
            futures::stream::iter(ids.into_iter().map(|id| async move {
                // A per-result load failure drops that ROW rather than the whole page
                // (the serial version failed the request); it is logged, never silent.
                let work = catalog::load_canonical_work(&st.pool, &id)
                    .await
                    .inspect_err(|e| tracing::warn!(work_id = %id, error = %e, "search: work load failed"))
                    .ok()??;
                // Anchored by construction (the index only holds mangadex-linked works);
                // guard anyway so a concurrent unlink can't surface a chapterless shell.
                work.mangadex_id.as_ref()?;
                let chapters = catalog::load_canonical_chapters(&st.pool, &id)
                    .await
                    .inspect_err(|e| tracing::warn!(work_id = %id, error = %e, "search: chapter load failed"))
                    .ok()?;
                let count = catalog::main_chapter_count_str(&chapters) as i32;
                Some(map_canonical_series(&st.pool, None, work, count).await)
            }))
            .buffered(SEARCH_MAP_CONCURRENCY)
            .filter_map(|s| async move { s })
            .collect()
            .await
        };
        // Rust-side backstop mirroring `discovery`. VERIFIED LEAK: anonymous
        // `search(query:"", page:17)` returned works with `isNsfw: true` while
        // `canonicalSeries` on the same work correctly refused. `Series.is_nsfw` here is
        // `COALESCE(is_nsfw_override, is_nsfw)` (see `map_canonical_series`), so this
        // catches admin-marked works the FTS SQL gate misses.
        let items = filter_nsfw(show_nsfw, items);
        let has_next = (page.max(1) as i64) * BROWSE_PAGE_SIZE < total;
        Ok(SeriesPage {
            items,
            page,
            has_next_page: has_next,
            total: Some(total as i32),
        })
    }

    /// The genre facets Browse renders as filter chips (S4), most common first then
    /// alphabetical — read from the materialized `feed_genre_facet` table (migration 0068)
    /// and gated by the viewer's own NSFW posture.
    ///
    /// EACH COUNT IS NOW THE COUNT THE FILTER RETURNS. It was not: the facets came from
    /// JSON-parsing all 13,847 `suwayomi_series.genre` blobs on every request (300-465 ms,
    /// 301 distinct entries, top entry literally "Japanese", no NSFW gate) while the genre
    /// FILTER ran against `work_tag` over the canonical catalogue — so a chip labelled
    /// "Action · 4,102" returned a different number of results. Both sides now derive from
    /// `work_tag` joined to `feed_series_updates`, in the same transaction that rebuilds the
    /// feed.
    ///
    /// Empty until MangaDex tags are ingested into `work_tag` (migration 0066 added the
    /// column and the reverse index; the table holds 0 rows in production today). An empty
    /// chip list is the deliberate choice over 301 wrong ones.
    async fn genre_facets(&self, ctx: &Context<'_>) -> Result<Vec<GenreFacet>> {
        let st = state(ctx);
        // Same read-through shape as `discovery`: the facet table is viewer-invariant
        // given the NSFW posture, which is the whole cache key (see [`GENRE_FACETS_CACHE`]).
        let show_nsfw = viewer_show_nsfw(ctx).await;
        let key = feed_cache_key(show_nsfw);
        let facets = if let Some(hit) = cached_feed(&GENRE_FACETS_CACHE, key, FEED_TTL) {
            (*hit).clone()
        } else {
            // Single-flight the refill so a cold key under load recomputes once.
            let lock = GENRE_FACETS_INFLIGHT.lock_handle(i64::from(show_nsfw));
            let _guard = lock.lock().await;
            if let Some(hit) = cached_feed(&GENRE_FACETS_CACHE, key, FEED_TTL) {
                (*hit).clone()
            } else {
                let facets = crate::browse::genre_facets(&st.pool, show_nsfw)
                    .await
                    .map_err(gql_err)?;
                store_feed(&GENRE_FACETS_CACHE, key, facets.clone());
                facets
            }
        };
        Ok(facets
            .into_iter()
            .map(|(genre, count)| GenreFacet {
                genre,
                count: count as i32,
            })
            .collect())
    }

    /// Federated multi-extension catalogue search (S3). Fans the query out to
    /// every installed Suwayomi source (bounded concurrency + per-source timeout,
    /// failures skipped), runs each hit through the dedup matcher against the
    /// native catalogue, PERSISTS the matching/top-N source mappings so results
    /// consolidate under one canonical work, and returns deduped canonical entries
    /// each with its per-source translator list. User-facing: NSFW sources/results
    /// are hidden unless the viewer opted in (same posture as `search`).
    async fn search_all_sources(
        &self,
        ctx: &Context<'_>,
        query: String,
        #[graphql(default = 1)] page: i32,
    ) -> Result<FederatedSearchPage> {
        // C1: authenticated only — this endpoint fans out to many sources AND
        // persists (library enrollment + dedup writes), so anonymous callers are
        // rejected outright (no anonymous writes).
        let user = require_user(ctx).await?;
        let trimmed = query.trim().to_string();
        if trimmed.is_empty() {
            return Err(Error::new("query must not be empty"));
        }
        let st = ctx.data_unchecked::<std::sync::Arc<AppState>>().clone();
        // C1: per-user rate limit so an authed client can't hammer the engine /
        // MangaDex through the fan-out + persist path. Atomic check-and-record:
        // the old separate is_limited + record pair let N concurrent searches all
        // pass the read before any hit landed (TOCTOU over-admission).
        if let Err(retry) = st.federated_limiter.check(&format!("fed:{}", user.id)) {
            return Err(Error::new(format!(
                "Too many source searches — retry in {retry}s"
            )));
        }
        let show_nsfw = user_show_nsfw(&st.pool, &user.id).await;
        federated_search(&st, &trimmed, page.max(1), show_nsfw, Some(user)).await
    }

    async fn series(&self, ctx: &Context<'_>, id: ID) -> Result<Series> {
        let st = state(ctx);
        let n = id.0.parse::<i64>().map_err(gql_err)?;
        // Suwayomi ids are sequential integers, so gate before the source round-trip:
        // an opted-out viewer must not read the detail of an NSFW series by id (N2).
        if canonical_is_nsfw(&st.pool, &id.0).await && !viewer_show_nsfw(ctx).await {
            return Err(Error::new("No such series"));
        }
        // S1: serve from the DB cache; only live-fetch (and cache) on a miss.
        let m = resolve_series_cached(st, n).await.map_err(gql_err)?;
        // English-only serve guard (defense in depth): Browse never lists non-English
        // series (the cache refuses them), but a direct id load could still reach one
        // via an old bookmark before the purge sweeps it. Treat it as not-found so no
        // non-English detail (or its chapter list, reached from here) is served.
        if m.source
            .as_ref()
            .and_then(|s| s.lang.as_deref())
            .is_some_and(|l| l != "en")
        {
            return Err(Error::new("No such series"));
        }
        Ok(map_series(st, m).await)
    }

    async fn chapters(&self, ctx: &Context<'_>, series_id: ID) -> Result<Vec<Chapter>> {
        let st = state(ctx);
        let n = series_id.0.parse::<i64>().map_err(gql_err)?;
        // Gate the chapter list on the owning series' NSFW flag (same as `series`, N2).
        if canonical_is_nsfw(&st.pool, &series_id.0).await && !viewer_show_nsfw(ctx).await {
            return Err(Error::new("No such series"));
        }
        // S1: serve from the DB cache; only live-fetch (and cache) on a miss.
        let list = resolve_chapters_cached(st, n).await.map_err(gql_err)?;
        // Overlay the VIEWER's per-user read state (`suwayomi_progress`) — the cached
        // `suwayomi_chapter` read flags are global and no longer authoritative (CR6).
        let user = current_user(ctx).await;
        let progress =
            suwayomi_progress_map(&st.pool, user.as_ref().map(|u| u.id.as_str()), n).await;
        Ok(list
            .into_iter()
            .map(|c| {
                let p = progress.get(&c.id).copied();
                map_chapter(c, p)
            })
            .collect())
    }

    async fn pages(&self, ctx: &Context<'_>, chapter_id: ID) -> Result<Vec<Page>> {
        let st = state(ctx);
        let n = chapter_id.0.parse::<i64>().map_err(gql_err)?;
        // The NSFW flag lives on the owning series/work, not the chapter, and Suwayomi
        // chapters aren't mirrored locally — so resolve the manga id from the source and
        // gate the page images exactly like `series`/`chapters` (N2). An unknown chapter
        // is allowed through to the (cleanly-failing) page fetch, mirroring canonicalPages.
        if let Some(manga_id) = st.suwayomi.chapter_manga_id(n).await.map_err(gql_err)? {
            if canonical_is_nsfw(&st.pool, &manga_id.to_string()).await
                && !viewer_show_nsfw(ctx).await
            {
                return Err(Error::new("No such chapter"));
            }
        }
        let urls = st.suwayomi.pages(n).await.map_err(gql_err)?;
        Ok(urls
            .into_iter()
            .enumerate()
            .map(|(index, source_url)| Page {
                index: index as i32,
                source_url,
                width: None,
                height: None,
            })
            .collect())
    }

    async fn library(&self, ctx: &Context<'_>) -> Result<Vec<Series>> {
        let st = state(ctx);
        // "Your Library" is PER-USER: only the series the viewer has added
        // (`user_library`), newest-added first. An anonymous visitor has no library —
        // return empty rather than the whole catalogue. (This replaced returning
        // Suwayomi's shared in-library set, which showed every visitor the same
        // ~571-series "library".)
        let Some(user) = current_user(ctx).await else {
            return Ok(Vec::new());
        };
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT series_id FROM user_library WHERE user_id = ? ORDER BY created_at DESC",
        )
        .bind(&user.id)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;
        // Build the library in the stored (newest-added-first) order. Canonical works
        // fill their slot immediately; numeric Suwayomi series reserve a slot and are
        // mapped together in ONE batched pass afterwards (O(1) grouped queries per
        // lookup instead of ~5 per series), preserving the interleaved order.
        let mut out: Vec<Option<Series>> = Vec::with_capacity(ids.len());
        let mut pending: Vec<(usize, SuwayomiManga)> = Vec::new();
        for sid in ids {
            if sid.starts_with("w_") {
                // Canonical (MangaDex-mirrored) work.
                if let Some(work) = catalog::load_canonical_work(&st.pool, &sid)
                    .await
                    .map_err(gql_err)?
                {
                    let chapters = catalog::load_canonical_chapters(&st.pool, &sid)
                        .await
                        .map_err(gql_err)?;
                    out.push(Some(
                        map_canonical_series(
                            &st.pool,
                            Some(user.id.as_str()),
                            work,
                            catalog::main_chapter_count_str(&chapters) as i32,
                        )
                        .await,
                    ));
                }
            } else if let Ok(n) = sid.parse::<i64>() {
                // Numeric Suwayomi series — DB cache first, live fetch on a miss.
                let m = match crate::series_cache::get_series(&st.pool, n)
                    .await
                    .map_err(gql_err)?
                {
                    Some(m) => Some(m),
                    None => st.suwayomi.series(n).await.ok(),
                };
                if let Some(m) = m {
                    pending.push((out.len(), m));
                    out.push(None); // slot filled by the batched map below
                }
            }
        }
        // Map all numeric Suwayomi series at once, then drop each into its slot.
        let (indices, mangas): (Vec<usize>, Vec<SuwayomiManga>) = pending.into_iter().unzip();
        for (idx, series) in indices.into_iter().zip(map_series_batch(st, mangas).await) {
            out[idx] = Some(series);
        }
        let out: Vec<Series> = out.into_iter().flatten().collect();
        // Hide NSFW series from the library too, unless the viewer opted in (N2).
        Ok(filter_nsfw(viewer_show_nsfw(ctx).await, out))
    }

    /// Per-series read progress for the viewer's library, in a couple of grouped
    /// queries. The Library and Profile screens use this to shelve series by progress
    /// (reading / completed / plan) without fetching each series' chapter list — the
    /// N-round-trip fan-out that hung both pages. Empty for anonymous viewers.
    ///
    /// Numeric Suwayomi series read counts come from the viewer's per-user
    /// `suwayomi_progress`; canonical `w_` works from the per-user `canonical_progress`.
    /// A series with no progress rows is omitted; the client then treats it as unread
    /// and uses `chapterCount` for the total.
    async fn library_progress(&self, ctx: &Context<'_>) -> Result<Vec<SeriesProgress>> {
        let st = state(ctx);
        let Some(user) = current_user(ctx).await else {
            return Ok(Vec::new());
        };
        // Numeric Suwayomi series in the viewer's library — read count from the viewer's
        // own `suwayomi_progress` (no longer the shared `suwayomi_chapter.is_read`).
        // `total` is left 0 so the client falls back to the series' `chapterCount`
        // (only chapters the viewer has touched have progress rows).
        let mut out: Vec<SeriesProgress> = sqlx::query_as::<_, (String, i64)>(
            "SELECT sp.series_id, COALESCE(SUM(sp.read), 0) AS read \
             FROM suwayomi_progress sp \
             JOIN user_library ul ON ul.series_id = sp.series_id AND ul.user_id = sp.user_id \
             WHERE sp.user_id = ? \
             GROUP BY sp.series_id",
        )
        .bind(&user.id)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?
        .into_iter()
        .map(|(id, read)| SeriesProgress {
            id: ID(id),
            read: read as i32,
            total: 0,
        })
        .collect();
        // Canonical works in the viewer's library — read from per-user progress;
        // `total` is left 0 so the client falls back to the work's `chapterCount`.
        let canon = sqlx::query_as::<_, (String, i64)>(
            "SELECT cp.work_id, COALESCE(SUM(cp.read), 0) AS read \
             FROM canonical_progress cp \
             JOIN user_library ul ON ul.series_id = cp.work_id AND ul.user_id = cp.user_id \
             WHERE cp.user_id = ? \
             GROUP BY cp.work_id",
        )
        .bind(&user.id)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;
        for (work_id, read) in canon {
            out.push(SeriesProgress {
                id: ID(work_id),
                read: read as i32,
                total: 0,
            });
        }
        Ok(out)
    }

    async fn reviews(
        &self,
        ctx: &Context<'_>,
        series_id: ID,
        #[graphql(default = 1)] page: i32,
    ) -> Result<ReviewPage> {
        let st = state(ctx);
        let offset = (page.max(1) as i64 - 1) * PAGE_SIZE;
        let rows: Vec<ReviewJoin> = sqlx::query_as(
            "SELECT r.id, r.series_id, r.score, r.body, r.has_spoiler, r.created_at, r.updated_at, \
             u.id AS author_id, u.username AS author_username, u.avatar_url AS author_avatar \
             FROM reviews r JOIN users u ON u.id = r.user_id \
             WHERE r.series_id = ? AND u.is_banned = 0 ORDER BY r.created_at DESC, r.id DESC LIMIT ? OFFSET ?",
        )
        .bind(series_id.0.clone())
        .bind(PAGE_SIZE + 1)
        .bind(offset)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM reviews r JOIN users u ON u.id = r.user_id \
             WHERE r.series_id = ? AND u.is_banned = 0",
        )
        .bind(series_id.0.clone())
        .fetch_one(&st.pool)
        .await
        .map_err(gql_err)?;
        let has_next = rows.len() as i64 > PAGE_SIZE;
        let items = rows
            .into_iter()
            .take(PAGE_SIZE as usize)
            .map(Review::from)
            .collect();
        Ok(ReviewPage {
            items,
            page,
            has_next_page: has_next,
            total: Some(total as i32),
        })
    }

    /// The signed-in viewer's own review for a series, fetched by user identity
    /// so it's always retrievable regardless of pagination. The paginated
    /// `reviews` list only returns page 1 (`created_at DESC`, `PAGE_SIZE`); on a
    /// busy series the viewer's earlier review can fall off it, which would show
    /// them as unrated with an empty body. Returns `null` when the viewer has no
    /// review, or when the request is anonymous. No `is_banned` filter here: this
    /// returns the viewer's OWN review and the viewer is authenticated (a banned
    /// user can't sign in), so the ban filter is both unnecessary and wrong.
    async fn my_review(&self, ctx: &Context<'_>, series_id: ID) -> Result<Option<Review>> {
        let Some(user) = current_user(ctx).await else {
            return Ok(None);
        };
        let st = state(ctx);
        let row: Option<ReviewJoin> = sqlx::query_as(
            "SELECT r.id, r.series_id, r.score, r.body, r.has_spoiler, r.created_at, r.updated_at, \
             u.id AS author_id, u.username AS author_username, u.avatar_url AS author_avatar \
             FROM reviews r JOIN users u ON u.id = r.user_id \
             WHERE r.series_id = ? AND r.user_id = ?",
        )
        .bind(series_id.0.clone())
        .bind(&user.id)
        .fetch_optional(&st.pool)
        .await
        .map_err(gql_err)?;
        Ok(row.map(Review::from))
    }

    async fn comments(
        &self,
        ctx: &Context<'_>,
        target_type: String,
        target_id: ID,
        #[graphql(default = 1)] page: i32,
    ) -> Result<CommentPage> {
        let target_type = validate_comment_target(&target_type)?;
        let st = state(ctx);
        let offset = (page.max(1) as i64 - 1) * PAGE_SIZE;
        // The viewer's own vote per comment is selected inline; '' (anonymous) matches
        // no vote row, yielding my_vote = 0.
        let viewer_id = current_user(ctx).await.map(|u| u.id).unwrap_or_default();
        // Pagination is over ROOT comments (whole threads); a page returns those
        // roots plus *all* their descendants (flat, ascending) so the client can
        // assemble the full reply tree without extra round-trips. Banned authors'
        // subtrees are pruned at every depth (the recursion re-checks `is_banned`),
        // so a banned reply never orphans the branch beneath it in the response.
        // Like/dislike tallies and the viewer's vote are correlated subqueries so the
        // whole thread's votes come back in this one query (no per-comment fan-out).
        let rows: Vec<CommentJoin> = sqlx::query_as(
            "WITH RECURSIVE roots AS ( \
                 SELECT c.id, c.created_at FROM comments c JOIN users u ON u.id = c.user_id \
                 WHERE c.target_type = ? AND c.target_id = ? AND c.parent_id IS NULL \
                   AND u.is_banned = 0 \
                 ORDER BY c.created_at ASC, c.id ASC LIMIT ? OFFSET ? \
             ), \
             thread(id) AS ( \
                 SELECT id FROM roots \
                 UNION ALL \
                 SELECT c.id FROM comments c JOIN thread t ON c.parent_id = t.id \
                     JOIN users u ON u.id = c.user_id WHERE u.is_banned = 0 \
             ) \
             SELECT c.id, c.target_type, c.target_id, c.parent_id, c.body, c.has_spoiler, \
                    c.created_at, u.id AS author_id, u.username AS author_username, \
                    u.avatar_url AS author_avatar, \
                    m.id AS media_id, m.width AS media_width, m.height AS media_height, \
                    COALESCE((SELECT COUNT(*) FROM comment_votes v \
                              WHERE v.comment_id = c.id AND v.value = 1), 0) AS likes, \
                    COALESCE((SELECT COUNT(*) FROM comment_votes v \
                              WHERE v.comment_id = c.id AND v.value = -1), 0) AS dislikes, \
                    COALESCE((SELECT v.value FROM comment_votes v \
                              WHERE v.comment_id = c.id AND v.user_id = ?), 0) AS my_vote \
             FROM comments c JOIN users u ON u.id = c.user_id \
             LEFT JOIN comment_media m ON m.comment_id = c.id \
             WHERE c.id IN (SELECT id FROM thread) \
             ORDER BY c.created_at ASC",
        )
        .bind(target_type)
        .bind(target_id.0.clone())
        .bind(PAGE_SIZE)
        .bind(offset)
        .bind(&viewer_id)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;
        // `total` and `has_next_page` count root threads (the paginated unit).
        let total_roots: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM comments c JOIN users u ON u.id = c.user_id \
             WHERE c.target_type = ? AND c.target_id = ? AND c.parent_id IS NULL \
               AND u.is_banned = 0",
        )
        .bind(target_type)
        .bind(target_id.0.clone())
        .fetch_one(&st.pool)
        .await
        .map_err(gql_err)?;
        let has_next = offset + PAGE_SIZE < total_roots;
        let items = rows.into_iter().map(Comment::from).collect();
        Ok(CommentPage {
            items,
            page,
            has_next_page: has_next,
            total: Some(total_roots as i32),
        })
    }

    /// Aggregate health of the background scan scheduler (admin console).
    async fn scan_status(&self, ctx: &Context<'_>) -> Result<ScanStatus> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let (
            library_size,
            overdue_count,
            last_tick_at,
            scanned_ok,
            scanned_failed,
            last_success_at,
            stuck_ticks,
        ) = {
            // Recover from a poisoned lock rather than propagating the panic.
            let h = st.scan_health.lock().unwrap_or_else(|e| e.into_inner());
            (
                h.library_size as i32,
                h.overdue_count as i32,
                h.last_tick_at.clone(),
                h.scanned_ok as i32,
                h.scanned_failed as i32,
                h.last_success_at.clone(),
                h.consecutive_stuck_ticks as i32,
            )
        };
        // Earliest FUTURE next_scan_at across all tracked series. Restricting to `> now`
        // excludes already-due rows (the far-past DUE_NOW_SENTINEL and any overdue), so
        // this reports the next *upcoming* scan rather than the 1970 sentinel.
        let next_due_at: Option<String> = sqlx::query_scalar(
            "SELECT MIN(next_scan_at) FROM series_scan_state WHERE next_scan_at > ?",
        )
        .bind(Utc::now().to_rfc3339())
        .fetch_optional(&st.pool)
        .await
        .ok()
        .flatten();
        Ok(ScanStatus {
            library_size,
            overdue_count,
            last_tick_at,
            next_due_at,
            scanned_ok,
            scanned_failed,
            last_success_at,
            stuck_ticks,
        })
    }

    /// Admin user-management console: a paginated list of accounts, newest first.
    async fn users(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 1)] page: i32,
    ) -> Result<AdminUserPage> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let offset = (page.max(1) as i64 - 1) * PAGE_SIZE;
        let rows: Vec<AdminUserRow> = sqlx::query_as(
            "SELECT id, username, email, avatar_url, is_admin, is_banned, created_at \
             FROM users ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?",
        )
        .bind(PAGE_SIZE + 1)
        .bind(offset)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&st.pool)
            .await
            .map_err(gql_err)?;
        let has_next = rows.len() as i64 > PAGE_SIZE;
        let items = rows
            .into_iter()
            .take(PAGE_SIZE as usize)
            .map(AdminUser::from)
            .collect();
        Ok(AdminUserPage {
            items,
            page,
            has_next_page: has_next,
            total: Some(total as i32),
        })
    }

    /// Admin dedup review queue: pending mid-confidence matches, highest-confidence
    /// first, with the candidate work's title and the source series' current title for
    /// context. Paginated (`page` 1-based, `limit` clamped to 1..=200).
    ///
    /// PAGINATION, not the old 200-row cap. The cap truncated the queue to the 200
    /// NEWEST rows, so the oldest — and highest-confidence — duplicates were permanently
    /// unreachable. Removing it left the resolver returning the ENTIRE backlog in one
    /// response, which was ~1,026 rows and is now roughly 10,400 since refused
    /// consolidation pairs are routed here too. Ordering stays `score DESC` so the most
    /// certain merges surface first; every row is reachable by paging.
    ///
    /// `mc.candidate_work_id <> ss.work_id` filters SELF-REFERENTIAL candidates: rows
    /// whose source series already belongs to the candidate work. These are stale
    /// artifacts of a past merge (folding work A into B repoints A's sources to B,
    /// leaving any candidate that pointed at B now self-referential). They are no-ops
    /// that clogged the queue (~570 of ~1.5k). `merge_works` now cleans them on merge
    /// and migration 0054 purged the backlog; this guard is defence-in-depth. `total`
    /// counts the same filtered set, so the pager's row count matches what it lists.
    async fn merge_queue(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 1)] page: i32,
        #[graphql(default = 50)] limit: i32,
    ) -> Result<MergeQueuePage> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let page = page.max(1);
        let per = (limit as i64).clamp(1, 200);
        let offset = (page as i64 - 1) * per;
        // The filter is spelled once, for the page and its count, so they cannot drift.
        const FROM_WHERE: &str = "FROM merge_candidate mc \
             JOIN work cw ON cw.id = mc.candidate_work_id \
             JOIN source_series ss ON ss.id = mc.source_series_id \
             JOIN work sw ON sw.id = ss.work_id \
             WHERE mc.status = 'pending' AND mc.candidate_work_id <> ss.work_id";

        let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) {FROM_WHERE}"))
            .fetch_one(&st.pool)
            .await
            .map_err(gql_err)?;
        // `mc.id` breaks ties so paging is stable: score+created_at alone are not
        // unique across ~10k rows, and an unstable order duplicates/skips rows at page
        // boundaries.
        //
        // The two correlated subqueries fetch each side's MangaDex anchor, which
        // `work_cover_url` needs for the lazy-fetch fallback on an uncached work. They
        // run at most 2×50 times per page — an admin panel, not a hot path.
        let rows = sqlx::query_as::<_, MergeCandidateRow>(&format!(
            "SELECT mc.id, mc.source_series_id, sw.id AS source_work_id, mc.candidate_work_id, \
                    cw.primary_title AS candidate_title, sw.primary_title AS source_title, \
                    sw.cover_cached_version AS source_cover_version, \
                    sw.cover_file_name AS source_cover_file_name, \
                    (SELECT s2.source_key FROM source_series s2 \
                     WHERE s2.work_id = sw.id AND s2.source_type = 'mangadex' \
                     ORDER BY s2.created_at ASC, s2.id ASC LIMIT 1) AS source_mangadex_id, \
                    cw.cover_cached_version AS candidate_cover_version, \
                    cw.cover_file_name AS candidate_cover_file_name, \
                    (SELECT c2.source_key FROM source_series c2 \
                     WHERE c2.work_id = cw.id AND c2.source_type = 'mangadex' \
                     ORDER BY c2.created_at ASC, c2.id ASC LIMIT 1) AS candidate_mangadex_id, \
                    mc.score, mc.method, mc.status, mc.created_at \
             {FROM_WHERE} \
             ORDER BY mc.score DESC, mc.created_at DESC, mc.id ASC \
             LIMIT ? OFFSET ?"
        ))
        .bind(per + 1) // one extra row to compute has_next_page
        .bind(offset)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;

        let has_next_page = rows.len() as i64 > per;
        let items = rows
            .into_iter()
            .take(per as usize)
            .map(|r| MergeCandidate {
                source_cover_url: crate::cover::work_cover_url(
                    &r.source_work_id,
                    r.source_cover_version,
                    r.source_mangadex_id.as_deref(),
                    r.source_cover_file_name.as_deref(),
                ),
                candidate_cover_url: crate::cover::work_cover_url(
                    &r.candidate_work_id,
                    r.candidate_cover_version,
                    r.candidate_mangadex_id.as_deref(),
                    r.candidate_cover_file_name.as_deref(),
                ),
                id: r.id,
                source_series_id: r.source_series_id,
                source_work_id: r.source_work_id,
                candidate_work_id: r.candidate_work_id,
                candidate_title: r.candidate_title,
                source_title: r.source_title,
                score: r.score,
                method: r.method,
                status: r.status,
                created_at: r.created_at,
            })
            .collect();
        Ok(MergeQueuePage {
            items,
            page,
            has_next_page,
            total: Some(total as i32),
        })
    }

    /// Admin: expand one work for dedup review — metadata, cover, alt titles and
    /// source mappings, for the panel behind a row's title/cover in the review queue.
    ///
    /// Reads `work` directly rather than routing through `canonicalSeries` ON PURPOSE.
    /// `canonicalSeries` errors on any work with no MangaDex anchor and hides NSFW works
    /// from an opted-out viewer — between them that is most of this queue (Suwayomi-only
    /// entries are exactly the ones whose identity is unclear enough to need review), so
    /// reusing it would leave the reviewer staring at "No such work" on the rows that
    /// most need a look. Admin-gated, so NSFW sources are returned unfiltered.
    ///
    /// A merged-away id resolves through the redirect table, the same way the reader's
    /// paths do: a row can be stale by the time it's clicked (another admin, or a
    /// consolidation run, may have folded that work away mid-review).
    async fn work_review_detail(&self, ctx: &Context<'_>, work_id: ID) -> Result<WorkReviewDetail> {
        require_admin(ctx).await?;
        let st = state(ctx);
        const SELECT: &str = "SELECT COALESCE(w.title_override, w.primary_title) AS title, \
                    w.author, w.artist, w.year, w.status, w.content_rating, \
                    w.original_language, \
                    COALESCE(w.is_nsfw_override, w.is_nsfw) AS is_nsfw, \
                    COALESCE(w.description_override, w.description) AS description, \
                    w.cover_cached_version, w.cover_file_name, \
                    (SELECT ss.source_key FROM source_series ss \
                     WHERE ss.work_id = w.id AND ss.source_type = 'mangadex' \
                     ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1) AS mangadex_id \
             FROM work w WHERE w.id = ?";

        let mut id = work_id.0.clone();
        let mut row = sqlx::query_as::<_, WorkReviewDetailRow>(SELECT)
            .bind(&id)
            .fetch_optional(&st.pool)
            .await
            .map_err(gql_err)?;
        if row.is_none() {
            if let Ok(Some(new_id)) = catalog::redirect_work_id(&st.pool, &id).await {
                row = sqlx::query_as::<_, WorkReviewDetailRow>(SELECT)
                    .bind(&new_id)
                    .fetch_optional(&st.pool)
                    .await
                    .map_err(gql_err)?;
                id = new_id;
            }
        }
        let row = row.ok_or_else(|| Error::new("No such work."))?;

        const MAX_ALT_TITLES: i64 = 24;
        let alt_titles: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT raw_title FROM work_alias WHERE work_id = ? \
             ORDER BY raw_title LIMIT ?",
        )
        .bind(&id)
        .bind(MAX_ALT_TITLES)
        .fetch_all(&st.pool)
        .await
        .unwrap_or_default();

        // DISTINCT number: the same chapter mirrored from three sources is one
        // chapter to a reviewer comparing how complete two entries are.
        let chapter_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT c.number) FROM chapter c \
             JOIN source_series ss ON ss.id = c.source_series_id \
             WHERE ss.work_id = ?",
        )
        .bind(&id)
        .fetch_one(&st.pool)
        .await
        .unwrap_or(0);

        let sources = load_work_sources(&st.pool, &id, true).await?;

        Ok(WorkReviewDetail {
            cover_url: crate::cover::work_cover_url(
                &id,
                row.cover_cached_version,
                row.mangadex_id.as_deref(),
                row.cover_file_name.as_deref(),
            ),
            work_id: ID(id),
            title: row.title,
            author: row.author,
            artist: row.artist,
            year: row.year.map(|y| y as i32),
            status: row.status,
            content_rating: row.content_rating,
            original_language: row.original_language,
            is_nsfw: row.is_nsfw != 0,
            description: row.description,
            chapter_count: chapter_count as i32,
            alt_titles,
            sources,
        })
    }

    /// Admin "Bugs" panel: works whose cover the crawl could not process, most
    /// recent first, paginated. Each carries the failure reason + a best-effort
    /// current cover URL so the admin can retry or upload a replacement.
    async fn cover_issues(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 1)] page: i32,
    ) -> Result<CoverIssuePage> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let page = page.max(1);
        const PER: i64 = 50;
        let offset = (page as i64 - 1) * PER;

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_cover_issue")
            .fetch_one(&st.pool)
            .await
            .map_err(gql_err)?;

        // Join the work for its title + the MangaDex anchor needed to build the
        // fallback cover URL (one extra fetch per page is fine for an admin panel).
        let rows = sqlx::query_as::<_, CoverIssueRow>(
            "SELECT i.work_id, w.primary_title AS title, i.reason, i.detail, \
                    i.attempts, i.first_seen, i.last_seen, w.cover_cached_version, \
                    (SELECT ss.source_key FROM source_series ss \
                     WHERE ss.work_id = w.id AND ss.source_type = 'mangadex' \
                     ORDER BY ss.created_at ASC, ss.id ASC LIMIT 1) AS mangadex_id, \
                    w.cover_file_name \
             FROM work_cover_issue i \
             JOIN work w ON w.id = i.work_id \
             ORDER BY i.last_seen DESC \
             LIMIT ? OFFSET ?",
        )
        .bind(PER + 1) // fetch one extra to compute has_next_page
        .bind(offset)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;

        let has_next_page = rows.len() as i64 > PER;
        let items = rows
            .into_iter()
            .take(PER as usize)
            .map(|r| CoverIssue {
                cover_url: crate::cover::work_cover_url(
                    &r.work_id,
                    r.cover_cached_version,
                    r.mangadex_id.as_deref(),
                    r.cover_file_name.as_deref(),
                ),
                work_id: ID(r.work_id),
                title: r.title,
                reason: r.reason,
                detail: r.detail,
                attempts: r.attempts as i32,
                first_seen: r.first_seen,
                last_seen: r.last_seen,
            })
            .collect();

        Ok(CoverIssuePage {
            items,
            page,
            has_next_page,
            total: Some(total as i32),
        })
    }

    async fn session(&self, ctx: &Context<'_>) -> Result<Option<Session>> {
        let Some(tok) = token(ctx) else {
            return Ok(None);
        };
        match current_user(ctx).await {
            Some(u) => {
                let show_nsfw = user_show_nsfw(&state(ctx).pool, &u.id).await;
                Ok(Some(Session {
                    token: tok,
                    user: build_session_user(&state(ctx).pool, &u, show_nsfw).await,
                }))
            }
            None => Ok(None),
        }
    }

    /// The signed-in user's recent activity feed (newest first). Empty when
    /// signed out. `limit` is clamped to [1, 50].
    async fn my_activity(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 20)] limit: i32,
    ) -> Result<Vec<Activity>> {
        let Some(user) = current_user(ctx).await else {
            return Ok(vec![]);
        };
        let limit = limit.clamp(1, 50) as i64;
        let rows = sqlx::query_as::<_, ActivityRow>(
            "SELECT id, kind, target_type, target_id, created_at \
             FROM user_activity WHERE user_id = ? \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(&user.id)
        .bind(limit)
        .fetch_all(&state(ctx).pool)
        .await
        .map_err(gql_err)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// The viewer's inbound notifications, newest-first (the bell feed). Empty for
    /// anonymous viewers. Each carries the actor, a short excerpt of the viewer's own
    /// comment, and the thread target for deep-linking.
    async fn notifications(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 1)] page: i32,
    ) -> Result<Vec<Notification>> {
        let Some(user) = current_user(ctx).await else {
            return Ok(vec![]);
        };
        let st = state(ctx);
        let offset = (page.max(1) as i64 - 1) * PAGE_SIZE;
        // For a `chapter`-target notification, resolve the OWNING series id so the
        // client can deep-link to the reader (`/read/<seriesId>?ch=<targetId>`): a
        // numeric id maps via `suwayomi_chapter.manga_id`, a MangaDex uuid via its
        // `source_series.work_id`. The `NOT GLOB '*[^0-9]*'` guard keeps a uuid that
        // happens to start with a digit out of the numeric branch. Null for `series`
        // targets (targetId already is the series) and unresolvable chapters.
        let rows: Vec<NotificationRow> = sqlx::query_as(
            "SELECT n.id, n.kind, n.count, n.created_at, n.read_at, n.target_type, \
                    n.target_id, \
                    CASE WHEN n.target_type = 'chapter' THEN COALESCE( \
                        (SELECT CAST(sc.manga_id AS TEXT) FROM suwayomi_chapter sc \
                         WHERE n.target_id <> '' AND n.target_id NOT GLOB '*[^0-9]*' \
                           AND sc.id = CAST(n.target_id AS INTEGER)), \
                        (SELECT ss.work_id FROM chapter ch \
                         JOIN source_series ss ON ss.id = ch.source_series_id \
                         WHERE ch.external_id = n.target_id AND ss.source_type = 'mangadex') \
                    ) END AS series_id, \
                    n.comment_id, a.id AS actor_id, a.username AS actor_username, \
                    a.avatar_url AS actor_avatar, substr(c.body, 1, 140) AS comment_excerpt \
             FROM notifications n \
             LEFT JOIN users a ON a.id = n.actor_id \
             LEFT JOIN comments c ON c.id = n.comment_id \
             WHERE n.user_id = ? \
             ORDER BY n.created_at DESC, n.id DESC LIMIT ? OFFSET ?",
        )
        .bind(&user.id)
        .bind(PAGE_SIZE)
        .bind(offset)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;
        Ok(rows.into_iter().map(Notification::from).collect())
    }

    /// Count of the viewer's UNREAD notifications — drives the bell badge. 0 for
    /// anonymous viewers.
    async fn unread_notification_count(&self, ctx: &Context<'_>) -> Result<i32> {
        let Some(user) = current_user(ctx).await else {
            return Ok(0);
        };
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id = ? AND read_at IS NULL",
        )
        .bind(&user.id)
        .fetch_one(&state(ctx).pool)
        .await
        .map_err(gql_err)?;
        Ok(n as i32)
    }

    /// Admin "Sources & Extensions" console (EXT-1): every Keiyoushi/Mihon
    /// extension known to the Suwayomi engine, installed or not. On first use it
    /// seeds the curated Keiyoushi index as the default store (idempotent) and
    /// refreshes the list when the engine has never fetched one. NSFW extensions
    /// are hidden unless the admin opted in via `show_nsfw` (CATALOGUE.md §2).
    async fn extensions(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = false)] installed_only: bool,
        lang: Option<String>,
        nsfw: Option<bool>,
        // Re-fetch the store indexes before listing so `hasUpdate`/versions are
        // fresh (a network round-trip per store). Default false: list from the
        // engine's cached index.
        #[graphql(
            default = false,
            desc = "Re-fetch the store indexes before listing so hasUpdate/versions are fresh."
        )]
        refresh: bool,
    ) -> Result<Vec<ExtensionInfo>> {
        require_admin(ctx).await?;
        let st = state(ctx);
        ensure_default_extension_store(st).await?;
        if refresh {
            st.suwayomi.refresh_extensions().await.map_err(gql_err)?;
        }
        let mut list = st.suwayomi.list_extensions().await.map_err(gql_err)?;
        // Fresh engine: the store may be registered but its index never fetched.
        if list.is_empty() && !refresh {
            st.suwayomi.refresh_extensions().await.map_err(gql_err)?;
            list = st.suwayomi.list_extensions().await.map_err(gql_err)?;
        }
        // Admin management view: an admin curating the extension store must always be
        // able to SEE NSFW extensions (incl. MangaDex) to manage them, independent of
        // their reader-side `show_nsfw` browsing preference — otherwise turning NSFW off
        // for safe browsing would hide the very sources they administer. Explicit
        // filtering is still available via the `nsfw` argument. (The reader's public
        // browse/search stays gated by the viewer's `show_nsfw`.)
        // One query for the subscribed set, then badge each row — cheaper than a
        // per-extension lookup and keeps the map pure.
        let subscribed = catalog::subscribed_extension_set(&st.pool)
            .await
            .map_err(gql_err)?;
        Ok(
            filter_extensions(list, installed_only, lang.as_deref(), nsfw, true)
                .into_iter()
                .map(|e| {
                    let mut info = map_extension_info(st, e);
                    info.subscribed = subscribed.contains(&info.pkg_name);
                    info
                })
                .collect(),
        )
    }

    /// Admin: the installed Suwayomi sources — the picker that feeds
    /// `sourceBrowse(sourceId)` (EXT-1). This is admin-only management tooling, so
    /// every installed source is listed (incl. NSFW sources like MangaDex) regardless
    /// of the admin's reader-side `show_nsfw` preference; each carries its `is_nsfw`
    /// flag for the UI to badge. The public reader browse/search keeps its NSFW gate.
    async fn sources(&self, ctx: &Context<'_>) -> Result<Vec<SourceInfo>> {
        // Admin management view: always list every installed source (incl. NSFW ones
        // like MangaDex) so an admin can manage them regardless of their reader-side
        // `show_nsfw` browsing preference. `require_admin` is the access gate here; the
        // NSFW flag stays on each `SourceInfo` (is_nsfw) for the UI to badge. The public
        // reader browse/search keeps its per-viewer NSFW gate.
        require_admin(ctx).await?;
        let st = state(ctx);
        let list = st.suwayomi.list_sources().await.map_err(gql_err)?;
        Ok(list
            .into_iter()
            .map(|s| {
                let icon = st.suwayomi.abs(s.icon_url.as_deref());
                SourceInfo {
                    id: ID(s.id),
                    name: s.name,
                    lang: s.lang,
                    is_nsfw: s.is_nsfw,
                    icon_url: (!icon.is_empty()).then_some(icon),
                    pkg_name: s.pkg_name,
                }
            })
            .collect())
    }

    /// Admin: "add all from source" ingest jobs, newest first (S1). Pass
    /// `active: true` for only the currently-running ones. Poll this for job
    /// progress — counters are flushed after every page.
    async fn source_ingest_jobs(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = false)] active: bool,
    ) -> Result<Vec<SourceIngestJob>> {
        require_admin(ctx).await?;
        let st = state(ctx);
        Ok(crate::ingest::list_jobs(&st.pool, active)
            .await
            .map_err(gql_err)?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    /// Admin catalogue provenance, batched: for each Suwayomi series id, the
    /// canonical work it's linked to and every `source_series` mapping on that
    /// work with its extension coordinates — what the console needs to render an
    /// "extension"/source column next to search results. One group per requested
    /// id, in input order; an uncatalogued series yields `workId: null` and an
    /// empty `sources` list. NSFW mappings are hidden for an opted-out admin
    /// (same posture as `workSources`).
    async fn series_sources_batch(
        &self,
        ctx: &Context<'_>,
        series_ids: Vec<ID>,
    ) -> Result<Vec<SeriesSourceGroup>> {
        require_admin(ctx).await?;
        if series_ids.len() > 200 {
            return Err(Error::new("At most 200 ids per seriesSourcesBatch call"));
        }
        let st = state(ctx);
        let show_nsfw = viewer_show_nsfw(ctx).await;
        if series_ids.is_empty() {
            return Ok(Vec::new());
        }

        // X2: one lookup for every requested key (was one query_scalar per id).
        // A series key can map twice — the migration-0005 backfill minted
        // placeholder works with a BLANK source_id, and a later real ingest links
        // the same key under its real source id. Fetch all rows and pick, per key,
        // the real mapping (then most-recent) so provenance never surfaces the shell.
        let keys: Vec<String> = series_ids.iter().map(|id| id.0.clone()).collect();
        let placeholders = std::iter::repeat_n("?", keys.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT source_key, work_id, (source_id != '') AS is_real, last_seen \
             FROM source_series \
             WHERE source_type = 'suwayomi' AND source_key IN ({placeholders})"
        );
        let mut q = sqlx::query_as::<_, (String, String, i64, String)>(&sql);
        for k in &keys {
            q = q.bind(k);
        }
        let rows = q.fetch_all(&st.pool).await.map_err(gql_err)?;
        // Best work_id per key: real mapping first, then latest last_seen.
        let mut best: HashMap<String, (i64, String, String)> = HashMap::new();
        for (key, wid, is_real, last_seen) in rows {
            let cand = (is_real, last_seen, wid);
            match best.get(&key) {
                Some((r, ls, _)) if (*r, ls.as_str()) >= (cand.0, cand.1.as_str()) => {}
                _ => {
                    best.insert(key, cand);
                }
            }
        }
        let key_to_work: HashMap<String, String> =
            best.into_iter().map(|(k, (_, _, wid))| (k, wid)).collect();

        // X2: one batched work-sources load for all distinct linked works.
        let distinct_works: Vec<String> = {
            let mut v: Vec<String> = key_to_work.values().cloned().collect();
            v.sort();
            v.dedup();
            v
        };
        let sources_by_work = load_work_sources_batch(&st.pool, &distinct_works, show_nsfw).await?;

        // Assemble in input order.
        let groups = series_ids
            .into_iter()
            .map(|series_id| {
                let work_id = key_to_work.get(&series_id.0).cloned();
                let sources = work_id
                    .as_ref()
                    .and_then(|w| sources_by_work.get(w).cloned())
                    .unwrap_or_default();
                SeriesSourceGroup {
                    series_id,
                    work_id: work_id.map(ID),
                    sources,
                }
            })
            .collect();
        Ok(groups)
    }

    /// Admin bulk-ingest picker (EXT-1): browse/search one Suwayomi source's
    /// catalogue. Every returned manga is persisted by Suwayomi and gets the
    /// internal id `bulkAddSourceSeries` consumes (GQL-SCHEMA-FINDINGS.md §A0).
    /// An NSFW source is refused unless the admin opted in (show_nsfw posture).
    async fn source_browse(
        &self,
        ctx: &Context<'_>,
        source_id: ID,
        #[graphql(name = "type")] ty: SourceBrowseType,
        #[graphql(default = 1)] page: i32,
        query: Option<String>,
    ) -> Result<SourceBrowsePage> {
        let user = require_admin(ctx).await?;
        let st = state(ctx);
        if !user_show_nsfw(&st.pool, &user.id).await {
            let (_, source_nsfw, _) = st
                .suwayomi
                .source_meta(&source_id.0)
                .await
                .map_err(gql_err)?;
            if source_nsfw {
                return Err(Error::new(
                    "This source is NSFW — enable NSFW in your settings to browse it",
                ));
            }
        }
        let (has_next, mangas) = st
            .suwayomi
            .browse_source(&source_id.0, ty.into(), page, query.as_deref())
            .await
            .map_err(gql_err)?;
        let items = mangas
            .into_iter()
            .map(|m| {
                let thumb = st.suwayomi.abs(m.thumbnail_url.as_deref());
                SourceBrowseEntry {
                    suwayomi_manga_id: ID(m.id.to_string()),
                    title: m.title,
                    thumbnail_url: (!thumb.is_empty()).then_some(thumb),
                    in_library: m.in_library,
                }
            })
            .collect();
        Ok(SourceBrowsePage {
            items,
            page,
            has_next_page: has_next,
        })
    }
}

/// The curated Keiyoushi extension index — the hardcoded default store
/// (CATALOGUE.md §Tier-2: curated, not crawled; `keiyoushi.github.io` 404s, the
/// raw `repo` branch is the working host). This is the `index.pb` URL Keiyoushi's
/// README publishes: the older `index.min.json` on the same branch still
/// resolves but was reduced to two "update your app" stub entries when they
/// migrated, so seeding it would yield an empty catalogue.
const KEIYOUSHI_INDEX_URL: &str = "https://github.com/keiyoushi/extensions/raw/repo/index.pb";

/// Seed the Keiyoushi index as the default extension store when the engine has
/// NONE configured. Presence can't be checked by URL equality (Suwayomi
/// canonicalizes the index URL on add), so "any store exists"
/// is the idempotency signal — an operator who removed the default on purpose
/// and added another store is left alone.
async fn ensure_default_extension_store(st: &AppState) -> Result<()> {
    let count = st.suwayomi.extension_store_count().await.map_err(gql_err)?;
    if count == 0 {
        let name = st
            .suwayomi
            .add_extension_store(KEIYOUSHI_INDEX_URL)
            .await
            .map_err(gql_err)?;
        tracing::info!(store = name, "seeded default Keiyoushi extension store");
    }
    Ok(())
}

/// The `repo` branch base every Keiyoushi index URL canonicalizes to, once
/// `github.com/…/raw/…` is rewritten to the direct raw host.
const KEIYOUSHI_REPO_BASE: &str = "https://raw.githubusercontent.com/keiyoushi/extensions/repo";

/// Our own icon URL for a Keiyoushi extension: `/ext-icons/{pkgName}.png`,
/// served by `serve_ext_icon` from the snapshot vendored in `assets/ext-icons/`.
///
/// We host these rather than link Keiyoushi's because Keiyoushi emptied the
/// `icon/` directory on their `repo` branch during the `index.pb` migration —
/// only two stub entries remain, so the `repo/icon/{pkgName}.png` URL this used
/// to emit now 404s for every real extension. Pointing at their replacement
/// layout would work today but leaves the product one upstream reshuffle away
/// from losing every icon again.
///
/// Relative on purpose: the caller absolutizes it against the public image base
/// (`SUWAYOMI_PUBLIC_URL`), which is the browser-reachable origin this server is
/// behind — the same treatment Suwayomi cover/page URLs get.
///
/// `None` for a package that isn't a Keiyoushi one (e.g. a locally-built apk):
/// the snapshot won't contain it, so the caller falls back to the engine icon
/// endpoint, which CAN serve it by unpacking the installed apk.
fn keiyoushi_icon_url(pkg_name: &str) -> Option<String> {
    crate::ext_icon::keiyoushi_source_url(pkg_name)?;
    Some(format!("/ext-icons/{pkg_name}.png"))
}

/// Derive a browser-reachable, store-hosted icon URL for an extension from its
/// repo index URL. GitHub `/raw/` paths are rewritten to the direct
/// `raw.githubusercontent.com` host to skip the 302 redirect. Keiyoushi — the
/// default store — is served from our own origin (see `keiyoushi_icon_url`);
/// other Mihon-format repos keep hosting icons next to their index at
/// `icon/{pkgName}.png`. `None` when the repo URL doesn't end in an index file
/// (unknown layout — caller falls back to the engine icon endpoint).
fn store_icon_url(repo: &str, pkg_name: &str) -> Option<String> {
    let (base, index) = repo.rsplit_once('/')?;
    if !index.starts_with("index.") {
        return None;
    }
    let base = match base.strip_prefix("https://github.com/") {
        Some(rest) if rest.contains("/raw/") => format!(
            "https://raw.githubusercontent.com/{}",
            rest.replacen("/raw/", "/", 1)
        ),
        _ => base.to_string(),
    };
    if base == KEIYOUSHI_REPO_BASE {
        return keiyoushi_icon_url(pkg_name);
    }
    Some(format!("{base}/icon/{pkg_name}.png"))
}

/// Map a Suwayomi extension row onto the GraphQL `ExtensionInfo`. The icon URL
/// prefers the STORE-hosted icon (derived from the repo index URL — for the
/// default Keiyoushi store that is our own `/ext-icons/…`): the engine's own
/// `/api/v1/extension/icon/…` endpoint serves icons too, but only for INSTALLED
/// extensions and only where the engine host is browser-reachable — in
/// deployments where Suwayomi stays internal, those icons never load in the
/// admin UI. The engine URL remains the fallback for extensions without a
/// derivable store icon (e.g. locally-uploaded APKs). Both are absolutized
/// against the public image base.
fn map_extension_info(st: &AppState, mut e: crate::suwayomi::ExtensionListEntry) -> ExtensionInfo {
    let store_icon = e
        .repo
        .as_deref()
        .and_then(|r| store_icon_url(r, &e.pkg_name))
        .map(|u| st.suwayomi.abs(Some(&u)));
    let icon = store_icon.or_else(|| {
        // L1: `store_icon_url` only knows the GitHub raw / `index.*` layout. For a
        // repo it can't map, we fall back to the engine icon endpoint — which only
        // loads where the Suwayomi host is browser-reachable. Log it so a
        // non-loading icon in a non-GitHub-repo deployment is diagnosable rather
        // than silent.
        if e.repo.is_some() {
            tracing::debug!(
                pkg = %e.pkg_name,
                repo = e.repo.as_deref().unwrap_or(""),
                "extension icon: no store-hosted URL for this repo layout; falling back to engine icon"
            );
        }
        let a = st.suwayomi.abs(e.icon_url.as_deref());
        (!a.is_empty()).then_some(a)
    });
    e.icon_url = icon;
    e.into()
}

/// Pure filter for the `extensions` listing. The viewer's `show_nsfw` posture
/// wins over any explicit `nsfw` filter: an opted-out admin never sees NSFW
/// extensions, even asking for them (CATALOGUE.md §2).
fn filter_extensions(
    list: Vec<crate::suwayomi::ExtensionListEntry>,
    installed_only: bool,
    lang: Option<&str>,
    nsfw: Option<bool>,
    show_nsfw: bool,
) -> Vec<crate::suwayomi::ExtensionListEntry> {
    list.into_iter()
        .filter(|e| !installed_only || e.is_installed)
        .filter(|e| lang.is_none_or(|l| e.lang == l))
        .filter(|e| nsfw.is_none_or(|n| e.is_nsfw == n))
        .filter(|e| show_nsfw || !e.is_nsfw)
        .collect()
}

/// Fold per-id `bulkAddSourceSeries` outcomes into the summary counts.
fn summarize_bulk(entries: Vec<BulkAddEntry>) -> BulkAddResult {
    let total = entries.len() as i32;
    let (mut succeeded, mut failed) = (0, 0);
    let (mut new_works, mut auto_merged, mut queued_for_review, mut already_existing) =
        (0, 0, 0, 0);
    for e in &entries {
        match &e.result {
            Some(r) => {
                succeeded += 1;
                match r.decision.as_str() {
                    "new" => new_works += 1,
                    // Exact MangaDex-UUID consolidation counts as an auto-merge.
                    "auto_merge" | "mangadex_id" => auto_merged += 1,
                    "review" => queued_for_review += 1,
                    "existing" => already_existing += 1,
                    _ => {}
                }
            }
            None => failed += 1,
        }
    }
    BulkAddResult {
        entries,
        total,
        succeeded,
        failed,
        new_works,
        auto_merged,
        queued_for_review,
        already_existing,
    }
}

/// One `source_series` row LEFT-JOINed to its `source_extension` coordinates. The
/// extension columns are all `Option` because a MangaDex-native mapping (source_id
/// `'mangadex'`) has no extension row, and a Suwayomi source may not be catalogued yet.
#[derive(sqlx::FromRow)]
struct WorkSourceRow {
    work_id: String,
    source_type: String,
    source_id: String,
    source_key: String,
    source_url: Option<String>,
    is_nsfw: i64,
    pkg_name: Option<String>,
    repo_url: Option<String>,
    apk_name: Option<String>,
    version_code: Option<i64>,
    ext_lang: Option<String>,
}

/// Map a joined row to a `WorkSource`, honoring the opted-out NSFW filter (returns
/// `None` for an NSFW mapping a non-opted-in viewer must not see).
fn work_source_from_row(r: WorkSourceRow, show_nsfw: bool) -> Option<WorkSource> {
    if !show_nsfw && r.is_nsfw != 0 {
        return None;
    }
    // The LEFT JOIN matches iff the source has a catalogued extension.
    let extension = match (r.pkg_name, r.repo_url) {
        (Some(pkg_name), Some(repo_url)) => Some(SourceExtension {
            pkg_name,
            repo_url,
            apk_name: r.apk_name,
            version_code: r.version_code.map(|v| v as i32),
            lang: r.ext_lang.clone(),
        }),
        _ => None,
    };
    Some(WorkSource {
        source_type: r.source_type,
        source_id: r.source_id,
        source_key: r.source_key,
        source_url: r.source_url,
        is_nsfw: r.is_nsfw != 0,
        lang: r.ext_lang,
        extension,
    })
}

const WORK_SOURCE_SELECT: &str =
    "SELECT ss.work_id, ss.source_type, ss.source_id, ss.source_key, ss.source_url, ss.is_nsfw, \
            se.pkg_name, se.repo_url, se.apk_name, se.version_code, se.lang AS ext_lang \
     FROM source_series ss \
     LEFT JOIN source_extension se ON se.source_id = ss.source_id";

/// The AUTHORITATIVE Suwayomi `source_key`s for a work (F1) — one per source_id,
/// the mapping with the most cached chapters. Used to drop redundant same-source
/// mappings from `workSources`/translators so they agree with `aggregatedChapters`.
async fn authoritative_key_set(
    pool: &SqlitePool,
    work_id: &str,
) -> Result<std::collections::HashSet<String>> {
    Ok(catalog::authoritative_suwayomi_mappings(pool, work_id)
        .await
        .map_err(gql_err)?
        .into_iter()
        .map(|m| m.source_key)
        .collect())
}

/// Keep a work's source mappings, dropping redundant same-source Suwayomi mappings
/// (F1): a Suwayomi row survives only if its `source_key` is the authoritative one
/// for its `source_id`. MangaDex / non-Suwayomi rows always survive.
fn retain_authoritative(
    sources: Vec<WorkSource>,
    keys: &std::collections::HashSet<String>,
) -> Vec<WorkSource> {
    sources
        .into_iter()
        .filter(|s| s.source_type != "suwayomi" || keys.contains(&s.source_key))
        .collect()
}

/// Load a canonical work's source mappings, MangaDex-native first then by recency,
/// dropping NSFW rows for an opted-out viewer AND redundant same-source Suwayomi
/// mappings (F1 — only the authoritative, most-complete mapping per source survives,
/// so translators agree with `aggregatedChapters`). The extension is populated only
/// when the LEFT JOIN matched; `WorkSource.lang` is the extension's lang.
async fn load_work_sources(
    pool: &SqlitePool,
    work_id: &str,
    show_nsfw: bool,
) -> Result<Vec<WorkSource>> {
    let rows = sqlx::query_as::<_, WorkSourceRow>(&format!(
        "{WORK_SOURCE_SELECT} WHERE ss.work_id = ? \
         ORDER BY (ss.source_type = 'mangadex') DESC, ss.last_seen DESC"
    ))
    .bind(work_id)
    .fetch_all(pool)
    .await
    .map_err(gql_err)?;
    let mapped: Vec<WorkSource> = rows
        .into_iter()
        .filter_map(|r| work_source_from_row(r, show_nsfw))
        .collect();
    let keys = authoritative_key_set(pool, work_id).await?;
    Ok(retain_authoritative(mapped, &keys))
}

/// Batched `load_work_sources` (X2): one query for many work ids, returning a map
/// `work_id -> [WorkSource]`. Each work's list keeps the same MangaDex-first,
/// recency order as the single-work loader. Empty input → empty map.
async fn load_work_sources_batch(
    pool: &SqlitePool,
    work_ids: &[String],
    show_nsfw: bool,
) -> Result<HashMap<String, Vec<WorkSource>>> {
    if work_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = std::iter::repeat_n("?", work_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "{WORK_SOURCE_SELECT} WHERE ss.work_id IN ({placeholders}) \
         ORDER BY (ss.source_type = 'mangadex') DESC, ss.last_seen DESC"
    );
    let mut q = sqlx::query_as::<_, WorkSourceRow>(&sql);
    for wid in work_ids {
        q = q.bind(wid);
    }
    let rows = q.fetch_all(pool).await.map_err(gql_err)?;
    let mut map: HashMap<String, Vec<WorkSource>> = HashMap::new();
    for r in rows {
        let wid = r.work_id.clone();
        if let Some(ws) = work_source_from_row(r, show_nsfw) {
            map.entry(wid).or_default().push(ws);
        }
    }
    // F1: drop redundant same-source Suwayomi mappings per work (keep the
    // authoritative, most-complete one), consistent with the single-work loader.
    for (wid, sources) in map.iter_mut() {
        let keys = authoritative_key_set(pool, wid).await?;
        let taken = std::mem::take(sources);
        *sources = retain_authoritative(taken, &keys);
    }
    Ok(map)
}

// ---- Federated multi-extension search (S3) ---------------------------------

/// Deterministic relevance ranking for federated hits (X4): exact normalized-title
/// matches to `query` first, ties keeping the incoming (source-index) order — a
/// stable sort. Pure so the ordering is unit-testable without a live fan-out.
fn rank_federated_hits(hits: &mut [crate::suwayomi::SuwayomiManga], query: &str) {
    let nq = crate::catalog::normalize::normalize_title(query);
    hits.sort_by_key(|m| crate::catalog::normalize::normalize_title(&m.title) != nq);
}

/// Max installed sources fanned out per federated search. The MangaDex extension
/// alone exposes ~60 per-language sources; searching all of them for one query is
/// wasteful, so the fan-out dedupes to one source per extension (English-preferred)
/// and caps the total here.
const FEDERATED_MAX_SOURCES: usize = 24;
/// Bounded concurrency for the fan-out (polite toward the shared engine).
const FEDERATED_CONCURRENCY: usize = 6;
/// Per-source search timeout — a slow/hanging source is skipped, not awaited.
const FEDERATED_SOURCE_TIMEOUT_SECS: u64 = 8;
/// Anti-bloat policy (S3): a hit that does NOT match an existing work is persisted
/// only when it's among the first N ranked hits; matches always persist. Bounds how
/// many brand-new works a single user search can mint.
const FEDERATED_TOPN_NEW: usize = 20;
/// Hard ceiling on total persists per search, so a pathological fan-out can't run
/// hundreds of detail-fetch + library-write round-trips.
const FEDERATED_MAX_PERSIST: usize = 40;

/// Pick the sources to fan out to: drop NSFW ones for an opted-out viewer, then
/// keep ONE source per extension package (English-preferred, else first seen),
/// capped at `FEDERATED_MAX_SOURCES`. Deduping by pkg avoids the 60×-per-language
/// MangaDex explosion. Pure so it's unit-testable.
fn select_federated_sources(
    mut sources: Vec<crate::suwayomi::SuwayomiSource>,
    show_nsfw: bool,
) -> Vec<crate::suwayomi::SuwayomiSource> {
    sources.retain(|s| s.id != "0" && (show_nsfw || !s.is_nsfw));
    // Stable English-first ordering so the per-pkg pick is deterministic.
    sources.sort_by(|a, b| {
        let ae = (a.lang != "en") as u8;
        let be = (b.lang != "en") as u8;
        ae.cmp(&be).then_with(|| a.id.cmp(&b.id))
    });
    let mut seen_pkg: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in sources {
        // Key on the extension pkg when known, else the source id (a source with no
        // pkg is its own extension for dedup purposes).
        let key = s.pkg_name.clone().unwrap_or_else(|| s.id.clone());
        if seen_pkg.insert(key) {
            out.push(s);
            if out.len() >= FEDERATED_MAX_SOURCES {
                break;
            }
        }
    }
    out
}

/// Map a work's `WorkSource` rows onto `Translator`s, enriching each Suwayomi
/// mapping with its source's live display name + icon from `sources_by_id`
/// (built once per search from the installed-source list). The MangaDex spine
/// mapping becomes a "MangaDex" translator with no `suwayomiMangaId`.
fn work_sources_to_translators(
    st: &AppState,
    sources: Vec<WorkSource>,
    sources_by_id: &HashMap<String, crate::suwayomi::SuwayomiSource>,
) -> Vec<Translator> {
    sources
        .into_iter()
        .map(|ws| {
            if ws.source_type == "mangadex" {
                return Translator {
                    source_type: ws.source_type,
                    source_id: ws.source_id,
                    source_name: Some("MangaDex".to_string()),
                    lang: ws.lang,
                    suwayomi_manga_id: None,
                    extension_pkg_name: None,
                    extension_icon_url: None,
                };
            }
            let live = sources_by_id.get(&ws.source_id);
            // Prefer the STORE-hosted extension icon (browser-reachable even when
            // the engine host is internal), like the `extensions` surface; fall
            // back to the live source's engine icon (absolutized).
            let icon = ws
                .extension
                .as_ref()
                .and_then(|e| store_icon_url(&e.repo_url, &e.pkg_name))
                .map(|u| st.suwayomi.abs(Some(&u)))
                .or_else(|| {
                    live.and_then(|s| s.icon_url.as_deref())
                        .map(|u| st.suwayomi.abs(Some(u)))
                        .filter(|u| !u.is_empty())
                });
            Translator {
                source_name: live.map(|s| s.name.clone()),
                lang: live
                    .map(|s| s.lang.clone())
                    .or(ws.lang.clone())
                    .filter(|l| !l.is_empty()),
                extension_pkg_name: ws
                    .extension
                    .as_ref()
                    .map(|e| e.pkg_name.clone())
                    .or_else(|| live.and_then(|s| s.pkg_name.clone())),
                extension_icon_url: icon,
                // The Suwayomi manga id the reader fetches chapters with.
                suwayomi_manga_id: Some(ID(ws.source_key)),
                source_type: ws.source_type,
                source_id: ws.source_id,
            }
        })
        .collect()
}

/// The federated search core. Fans out, consolidates via dedup, and builds the
/// per-work translator lists. Separated from the resolver so the pieces
/// (`select_federated_sources`) stay unit-testable.
async fn federated_search(
    st: &std::sync::Arc<AppState>,
    query: &str,
    page: i32,
    show_nsfw: bool,
    user: Option<User>,
) -> Result<FederatedSearchPage> {
    // Installed sources → the fan-out set + a lookup for translator enrichment.
    let all_sources = st.suwayomi.list_sources().await.map_err(gql_err)?;
    let sources_by_id: HashMap<String, crate::suwayomi::SuwayomiSource> = all_sources
        .iter()
        .cloned()
        .map(|s| (s.id.clone(), s))
        .collect();
    let targets = select_federated_sources(all_sources, show_nsfw);
    let sources_queried = targets.len() as i32;

    // Fan out with bounded concurrency + a per-source timeout. A failing or slow
    // source is skipped (logged), never fatal. Each task carries its source's
    // selection INDEX so results can be reassembled deterministically regardless of
    // completion order (X4).
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(FEDERATED_CONCURRENCY));
    let mut set = tokio::task::JoinSet::new();
    for (idx, src) in targets.into_iter().enumerate() {
        let st2 = st.clone();
        let sem2 = sem.clone();
        let q = query.to_string();
        set.spawn(async move {
            let _permit = sem2.acquire().await.ok()?;
            let fut = st2
                .suwayomi
                .browse_source(&src.id, FetchType::Search, page, Some(&q));
            match tokio::time::timeout(
                std::time::Duration::from_secs(FEDERATED_SOURCE_TIMEOUT_SECS),
                fut,
            )
            .await
            {
                Ok(Ok((has_next, mangas))) => Some((idx, has_next, mangas)),
                Ok(Err(e)) => {
                    tracing::warn!(source_id = src.id, error = %e, "federated: source search failed");
                    None
                }
                Err(_) => {
                    tracing::warn!(source_id = src.id, "federated: source search timed out");
                    None
                }
            }
        });
    }
    // Gather per-source results, then reassemble in source-index order (X4: NOT
    // JoinSet completion order, which is nondeterministic and made which hits fell
    // inside the top-N vary per request).
    let mut per_source: Vec<(usize, Vec<crate::suwayomi::SuwayomiManga>)> = Vec::new();
    let mut any_has_next = false;
    while let Some(joined) = set.join_next().await {
        if let Ok(Some((idx, has_next, mangas))) = joined {
            any_has_next = any_has_next || has_next;
            per_source.push((idx, mangas));
        }
    }
    per_source.sort_by_key(|(idx, _)| *idx);
    // Flatten preserving source order + per-source position, deduped by manga id.
    let mut hits: Vec<crate::suwayomi::SuwayomiManga> = Vec::new();
    let mut seen_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for (_, mangas) in per_source {
        for m in mangas {
            if seen_ids.insert(m.id) {
                hits.push(m);
            }
        }
    }
    // Rank exact-title matches first (relevance), keeping source order within ties
    // — a stable sort over the already source-ordered list. This makes the top-N
    // cutoff deterministic and relevance-led rather than fastest-source-led (X4).
    rank_federated_hits(&mut hits, query);

    // Consolidate: persist matching / top-N hits so they resolve to canonical
    // works. The ranked order above makes the top-N deterministic (X4).
    let mut work_order: Vec<String> = Vec::new();
    let mut seen_works: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut persisted = 0usize;
    for (idx, m) in hits.iter().enumerate() {
        if persisted >= FEDERATED_MAX_PERSIST {
            break;
        }
        // Cheap match probe: does this title resolve to an existing work? (Title
        // only — the full corroborated dedup runs inside federated_ingest.)
        let cand = crate::dedup::Candidate {
            title: m.title.clone(),
            ..Default::default()
        };
        let probe = crate::dedup::resolve(&st.pool, &cand)
            .await
            .map_err(gql_err)?;
        let matched_work = match &probe {
            crate::dedup::Decision::AutoMerge { work_id, .. }
            | crate::dedup::Decision::Review { work_id, .. } => Some(work_id.clone()),
            crate::dedup::Decision::New => None,
        };
        // M1: never persist NOR library-enroll an NSFW hit for an opted-out viewer.
        // NSFW = genre signal on the hit, OR it matches an already-NSFW work.
        if !show_nsfw {
            let matched_nsfw = match &matched_work {
                Some(wid) => {
                    sqlx::query_scalar::<_, i64>("SELECT is_nsfw FROM work WHERE id = ?")
                        .bind(wid)
                        .fetch_optional(&st.pool)
                        .await
                        .map_err(gql_err)?
                        .unwrap_or(0)
                        != 0
                }
                None => false,
            };
            if genre_is_nsfw(&m.genre) || matched_nsfw {
                continue;
            }
        }
        // Anti-bloat: persist a match always; a non-match only within the top-N.
        if matched_work.is_none() && idx >= FEDERATED_TOPN_NEW {
            continue;
        }
        match federated_ingest(st, &m.id.to_string()).await {
            Ok(res) => {
                persisted += 1;
                if seen_works.insert(res.work_id.clone()) {
                    work_order.push(res.work_id);
                }
            }
            Err(e) => tracing::warn!(manga_id = m.id, error = %e, "federated: persist failed"),
        }
    }

    // Build one FederatedSeries per consolidated work, in discovery order.
    let uid = user.as_ref().map(|u| u.id.as_str());
    let mut items = Vec::new();
    for wid in work_order {
        let Some(work) = catalog::load_canonical_work(&st.pool, &wid)
            .await
            .map_err(gql_err)?
        else {
            continue;
        };
        // NSFW works are hidden from an opted-out viewer (same gate as feeds).
        if work.is_nsfw_override.unwrap_or(work.is_nsfw) && !show_nsfw {
            continue;
        }
        let chapters = catalog::load_canonical_chapters(&st.pool, &wid)
            .await
            .map_err(gql_err)?;
        let series = map_canonical_series(
            &st.pool,
            uid,
            work,
            catalog::main_chapter_count_str(&chapters) as i32,
        )
        .await;
        let translators = work_sources_to_translators(
            st,
            load_work_sources(&st.pool, &wid, show_nsfw).await?,
            &sources_by_id,
        );
        items.push(FederatedSeries {
            series,
            translators,
        });
    }

    // L2 (known limitation): `has_next_page` is a coarse OR across sources — true
    // if ANY queried source reported more results. Because each source paginates
    // independently and the results consolidate under canonical works, page N+1 can
    // re-yield works already seen on page N (from a source that had no more, while
    // another did). Treat it as "there may be more", not a clean cursor. A precise
    // cursor would need per-source page state carried across requests; deferred.
    Ok(FederatedSearchPage {
        items,
        page,
        has_next_page: any_has_next,
        sources_queried,
    })
}

/// Row loader for `user_activity`, mapped to the `Activity` GraphQL type.
#[derive(sqlx::FromRow)]
struct ActivityRow {
    id: String,
    kind: String,
    target_type: Option<String>,
    target_id: Option<String>,
    created_at: String,
}

impl From<ActivityRow> for Activity {
    fn from(r: ActivityRow) -> Self {
        Activity {
            id: ID(r.id),
            kind: r.kind,
            target_type: r.target_type,
            target_id: r.target_id.map(ID),
            created_at: r.created_at,
        }
    }
}

/// Row loader for `notifications` (joined to the actor user + source comment excerpt),
/// mapped to the `Notification` GraphQL type.
#[derive(sqlx::FromRow)]
struct NotificationRow {
    id: String,
    kind: String,
    count: Option<i64>,
    created_at: String,
    read_at: Option<String>,
    target_type: Option<String>,
    target_id: Option<String>,
    series_id: Option<String>,
    comment_id: Option<String>,
    actor_id: Option<String>,
    actor_username: Option<String>,
    actor_avatar: Option<String>,
    comment_excerpt: Option<String>,
}

impl From<NotificationRow> for Notification {
    fn from(r: NotificationRow) -> Self {
        Notification {
            id: ID(r.id),
            kind: r.kind,
            actor: r.actor_id.map(|id| UserRef {
                id: ID(id),
                username: r.actor_username.unwrap_or_default(),
                avatar_url: r.actor_avatar,
            }),
            comment_id: r.comment_id.map(ID),
            comment_excerpt: r.comment_excerpt,
            target_type: r.target_type,
            target_id: r.target_id.map(ID),
            series_id: r.series_id.map(ID),
            count: r.count.map(|n| n as i32),
            created_at: r.created_at,
            read: r.read_at.is_some(),
        }
    }
}

/// Reload a series (canonical `w_` work or numeric Suwayomi id) as a `Series`
/// whose per-viewer fields (`isMarked` / `libraryStatus` / `isFavorite`) resolve
/// against `user_id`. Shared by the library mutations to echo the updated state.
async fn reload_series(st: &AppState, user_id: &str, series_id: &str) -> Result<Series> {
    if series_id.starts_with("w_") {
        let work = catalog::load_canonical_work(&st.pool, series_id)
            .await
            .map_err(gql_err)?
            .ok_or_else(|| Error::new("No such work"))?;
        let chapters = catalog::load_canonical_chapters(&st.pool, series_id)
            .await
            .map_err(gql_err)?;
        return Ok(map_canonical_series(
            &st.pool,
            Some(user_id),
            work,
            catalog::main_chapter_count_str(&chapters) as i32,
        )
        .await);
    }
    let n = series_id.parse::<i64>().map_err(gql_err)?;
    // DB cache first, live source only on a miss — so filing a shelf / favouriting
    // a numeric series works even when the upstream source (Suwayomi) is offline
    // (the write has already landed; this only echoes the series back).
    let m = match crate::series_cache::get_series(&st.pool, n)
        .await
        .map_err(gql_err)?
    {
        Some(m) => m,
        None => st.suwayomi.series(n).await.map_err(gql_err)?,
    };
    Ok(map_series(st, m).await)
}

// ---- Mutation --------------------------------------------------------------

pub struct MutationRoot;

#[Object]
impl MutationRoot {
    async fn mark(&self, ctx: &Context<'_>, series_id: ID, marked: bool) -> Result<Series> {
        let st = state(ctx);
        // "Add to library" is a PER-USER action — it writes the viewer's own
        // `user_library`, not Suwayomi's shared in-library flag. Requires a session
        // (an anonymous visitor has no library to add to). Works for both numeric
        // Suwayomi ids and `w_` canonical ids.
        let user = require_user(ctx).await?;
        // Validate the id shape BEFORE writing, so a malformed id (neither a `w_`
        // canonical id nor a numeric Suwayomi id) can't persist an orphan
        // `user_library` row that later reads skip silently.
        if !series_id.0.starts_with("w_") && series_id.0.parse::<i64>().is_err() {
            return Err(Error::new("Invalid series id"));
        }
        if marked {
            let now = Utc::now().to_rfc3339();
            sqlx::query(
                "INSERT INTO user_library (user_id, series_id, created_at) VALUES (?, ?, ?) \
                 ON CONFLICT(user_id, series_id) DO NOTHING",
            )
            .bind(&user.id)
            .bind(&series_id.0)
            .bind(&now)
            .execute(&st.pool)
            .await
            .map_err(gql_err)?;
            log_activity(
                &st.pool,
                &user.id,
                "library_add",
                Some("series"),
                Some(&series_id.0),
            )
            .await;
        } else {
            sqlx::query("DELETE FROM user_library WHERE user_id = ? AND series_id = ?")
                .bind(&user.id)
                .bind(&series_id.0)
                .execute(&st.pool)
                .await
                .map_err(gql_err)?;
        }
        // Return the series with the (now-updated) per-viewer `isMarked` resolved by
        // the ComplexObject impl. Canonical `w_` works load from the mirror; numeric
        // ids load the source series.
        if series_id.0.starts_with("w_") {
            let work = catalog::load_canonical_work(&st.pool, &series_id.0)
                .await
                .map_err(gql_err)?
                .ok_or_else(|| Error::new("No such work"))?;
            let chapters = catalog::load_canonical_chapters(&st.pool, &series_id.0)
                .await
                .map_err(gql_err)?;
            return Ok(map_canonical_series(
                &st.pool,
                Some(user.id.as_str()),
                work,
                catalog::main_chapter_count_str(&chapters) as i32,
            )
            .await);
        }
        let n = series_id.0.parse::<i64>().map_err(gql_err)?;
        let m = st.suwayomi.series(n).await.map_err(gql_err)?;
        Ok(map_series(st, m).await)
    }

    /// File a series under an explicit shelf for the viewer ('reading' |
    /// 'completed' | 'onhold' | 'plan'), or clear it (`status: null`) to fall back
    /// to progress-derived shelving. Adds the series to the viewer's library if it
    /// isn't already there — filing a shelf implies membership. Per-user.
    async fn set_library_status(
        &self,
        ctx: &Context<'_>,
        series_id: ID,
        status: Option<String>,
    ) -> Result<Series> {
        let st = state(ctx);
        let user = require_user(ctx).await?;
        if let Some(s) = status.as_deref() {
            if !matches!(s, "reading" | "completed" | "onhold" | "plan") {
                return Err(Error::new(
                    "status must be one of: reading, completed, onhold, plan",
                ));
            }
        }
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO user_library (user_id, series_id, created_at, status) VALUES (?, ?, ?, ?) \
             ON CONFLICT(user_id, series_id) DO UPDATE SET status = excluded.status",
        )
        .bind(&user.id)
        .bind(&series_id.0)
        .bind(&now)
        .bind(&status)
        .execute(&st.pool)
        .await
        .map_err(gql_err)?;
        reload_series(st, &user.id, &series_id.0).await
    }

    /// Toggle whether the viewer has favourited a series. Adds it to the library if
    /// not already present — favouriting implies membership. Per-user.
    async fn set_favorite(
        &self,
        ctx: &Context<'_>,
        series_id: ID,
        favorite: bool,
    ) -> Result<Series> {
        let st = state(ctx);
        let user = require_user(ctx).await?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO user_library (user_id, series_id, created_at, is_favorite) VALUES (?, ?, ?, ?) \
             ON CONFLICT(user_id, series_id) DO UPDATE SET is_favorite = excluded.is_favorite",
        )
        .bind(&user.id)
        .bind(&series_id.0)
        .bind(&now)
        .bind(favorite as i64)
        .execute(&st.pool)
        .await
        .map_err(gql_err)?;
        reload_series(st, &user.id, &series_id.0).await
    }

    /// Record one view (a chapter open) for a series — the popularity signal behind
    /// Trending and the series-page view counts. Intentionally NO auth: every reader,
    /// signed-in or anonymous, counts (the product decision is to count everyone). The
    /// reader fires this once per chapter open. Best-effort: a counter write must never
    /// fail the read, so an error is logged and swallowed, and the mutation still
    /// returns `true`.
    ///
    /// Hardened (it is an unauthenticated WRITE): the id must be bounded in length and
    /// must resolve to a series we actually know about, and each `(client ip, series)`
    /// pair gets a small budget per window. Trending is top-10 by 24h views and the
    /// all-time leader has under 100 views, so without this a few hundred anonymous
    /// requests could place any series — or any junk id — on the home page.
    async fn record_view(&self, ctx: &Context<'_>, series_id: ID) -> Result<bool> {
        let st = state(ctx);
        let sid = series_id.0.as_str();
        // Bound the id before it ever reaches a query or a limiter key.
        const MAX_SERIES_ID_LEN: usize = 64;
        if sid.is_empty() || sid.len() > MAX_SERIES_ID_LEN {
            return Err(Error::new("invalid seriesId"));
        }
        // Per-(ip, series) budget. Kept as a module-global rather than an `AppState`
        // field so this fix touches no other file; see `VIEW_LIMITER`.
        if let Err(retry) = VIEW_LIMITER.check(&format!("view:{}:{}", client_ip(ctx), sid)) {
            return Err(Error::new(format!(
                "Too many views recorded for this series — retry in {retry}s"
            )));
        }
        // Reject ids that don't resolve to a known work or Suwayomi series, so the view
        // tables can't be seeded with arbitrary keys.
        if !known_series_id(&st.pool, sid).await {
            return Err(Error::new("No such series"));
        }
        if let Err(e) = crate::views::record(&st.pool, sid).await {
            tracing::warn!(series_id = %sid, error = %e, "recordView failed");
        }
        Ok(true)
    }

    async fn set_progress(
        &self,
        ctx: &Context<'_>,
        chapter_id: ID,
        last_page_read: i32,
        read: bool,
    ) -> Result<bool> {
        if last_page_read < 0 {
            return Err(Error::new("lastPageRead must be non-negative"));
        }
        let st = state(ctx);
        // Canonical chapter ids are MangaDex uuids (not all-digits); persist their
        // progress in `canonical_progress`. Numeric Suwayomi ids fall through (CR6).
        let is_numeric =
            !chapter_id.0.is_empty() && chapter_id.0.bytes().all(|b| b.is_ascii_digit());
        if !is_numeric {
            let user = require_user(ctx).await?;
            // The owning work, for per-series aggregation. If the chapter isn't in the
            // mirror, store with work_id = '' rather than erroring — it's per-user private.
            let work_id: String = sqlx::query_scalar(
                "SELECT ss.work_id FROM chapter c JOIN source_series ss ON ss.id = c.source_series_id \
                 WHERE c.external_id = ? AND ss.source_type = 'mangadex' LIMIT 1",
            )
            .bind(&chapter_id.0)
            .fetch_optional(&st.pool)
            .await
            .map_err(gql_err)?
            .unwrap_or_default();
            let now = Utc::now().to_rfc3339();
            sqlx::query(
                "INSERT INTO canonical_progress \
                   (user_id, chapter_id, work_id, last_page_read, read, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(user_id, chapter_id) DO UPDATE SET \
                   last_page_read = excluded.last_page_read, read = excluded.read, \
                   work_id = excluded.work_id, updated_at = excluded.updated_at",
            )
            .bind(&user.id)
            .bind(&chapter_id.0)
            .bind(&work_id)
            .bind(last_page_read as i64)
            .bind(read)
            .bind(&now)
            .execute(&st.pool)
            .await
            .map_err(gql_err)?;
            return Ok(true);
        }
        // Numeric Suwayomi chapter: per-user progress in `suwayomi_progress` (CR6).
        // Suwayomi is a content source only now — we no longer push read state to it.
        let n = chapter_id.0.parse::<i64>().map_err(gql_err)?;
        let user = require_user(ctx).await?;
        // The owning series id, for per-series aggregation in `libraryProgress`. If the
        // chapter isn't cached yet, store with series_id = '' rather than erroring —
        // the row is per-user private and the read state still round-trips.
        let series_id: Option<i64> =
            sqlx::query_scalar("SELECT manga_id FROM suwayomi_chapter WHERE id = ? LIMIT 1")
                .bind(n)
                .fetch_optional(&st.pool)
                .await
                .map_err(gql_err)?;
        let series_id = series_id.map(|s| s.to_string()).unwrap_or_default();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO suwayomi_progress \
               (user_id, chapter_id, series_id, last_page_read, read, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(user_id, chapter_id) DO UPDATE SET \
               last_page_read = excluded.last_page_read, read = excluded.read, \
               series_id = excluded.series_id, updated_at = excluded.updated_at",
        )
        .bind(&user.id)
        .bind(chapter_id.0.clone())
        .bind(&series_id)
        .bind(last_page_read as i64)
        .bind(read)
        .bind(&now)
        .execute(&st.pool)
        .await
        .map_err(gql_err)?;
        Ok(true)
    }

    async fn post_review(&self, ctx: &Context<'_>, input: PostReviewInput) -> Result<Review> {
        if !(1..=10).contains(&input.score) {
            return Err(Error::new("score must be between 1 and 10"));
        }
        // Cap the written body (mirrors the bio cap in `update_profile`) so a client
        // can't inflate the DB with multi-megabyte reviews.
        if input.body.trim().chars().count() > 4000 {
            return Err(Error::new("review must be at most 4000 characters"));
        }
        // An empty body is allowed: it represents a pure rating (the Series
        // rating widget) with no written review. The body-bearing reviews are
        // what the client shows as the comment thread.
        let user = require_user(ctx).await?;
        let st = state(ctx);
        // The target must exist. `reviews.series_id` is a free-form TEXT key (it carries
        // both `w_` work ids and numeric Suwayomi ids), so nothing in the schema stops a
        // signed-in user from creating rows against arbitrary ids — and those rows feed
        // the public rating aggregate.
        if !known_series_id(&st.pool, &input.series_id.0).await {
            return Err(Error::new("No such series"));
        }
        let now = Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();
        // One review per (series, user): upsert, keeping the original id/created_at.
        sqlx::query(
            "INSERT INTO reviews (id, series_id, user_id, score, body, has_spoiler, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(series_id, user_id) DO UPDATE SET \
               score = excluded.score, body = excluded.body, \
               has_spoiler = excluded.has_spoiler, updated_at = excluded.updated_at",
        )
        .bind(&id)
        .bind(input.series_id.0.clone())
        .bind(&user.id)
        .bind(input.score as i64)
        .bind(input.body.trim())
        .bind(input.has_spoiler)
        .bind(&now)
        .bind(&now)
        .execute(&st.pool)
        .await
        .map_err(gql_err)?;

        let row: ReviewJoin = sqlx::query_as(
            "SELECT r.id, r.series_id, r.score, r.body, r.has_spoiler, r.created_at, r.updated_at, \
             u.id AS author_id, u.username AS author_username, u.avatar_url AS author_avatar \
             FROM reviews r JOIN users u ON u.id = r.user_id \
             WHERE r.series_id = ? AND r.user_id = ?",
        )
        .bind(input.series_id.0.clone())
        .bind(&user.id)
        .fetch_one(&st.pool)
        .await
        .map_err(gql_err)?;
        log_activity(
            &st.pool,
            &user.id,
            "review",
            Some("series"),
            Some(&input.series_id.0),
        )
        .await;
        Ok(row.into())
    }

    async fn post_comment(&self, ctx: &Context<'_>, input: PostCommentInput) -> Result<Comment> {
        let target_type = validate_comment_target(&input.target_type)?;
        let body = input.body.trim().to_string();
        // A comment must carry text OR an image (an image-only comment is fine).
        if body.is_empty() && input.media_id.is_none() {
            return Err(Error::new("comment must have text or an image"));
        }
        // Cap the body (mirrors the bio cap in `update_profile`) to bound per-row
        // size and keep recursive thread reads cheap.
        if body.chars().count() > 4000 {
            return Err(Error::new("comment must be at most 4000 characters"));
        }
        let user = require_user(ctx).await?;
        let st = state(ctx);

        // The thread's target must exist. `comments.target_id` is a free-form TEXT key
        // with no FK, so without this a signed-in user can open threads on arbitrary
        // series/chapter ids.
        let target_exists = match target_type {
            "series" => known_series_id(&st.pool, &input.target_id.0).await,
            _ => known_chapter_id(&st.pool, &input.target_id.0).await,
        };
        if !target_exists {
            return Err(Error::new(format!("No such {target_type}")));
        }

        // A reply must point at an existing comment on the SAME target — this keeps
        // a thread's tree self-consistent and blocks cross-thread / cross-series
        // parent ids. The root of the tree is a NULL parent.
        if let Some(parent) = input.parent_id.as_ref() {
            let ok: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM comments WHERE id = ? AND target_type = ? AND target_id = ?",
            )
            .bind(&parent.0)
            .bind(target_type)
            .bind(input.target_id.0.clone())
            .fetch_optional(&st.pool)
            .await
            .map_err(gql_err)?;
            if ok.is_none() {
                return Err(Error::new("reply target not found on this thread"));
            }
        }

        let now = Utc::now().to_rfc3339();
        let id = uuid::Uuid::new_v4().to_string();
        let parent_id = input.parent_id.as_ref().map(|p| p.0.clone());

        // Insert the comment and (optionally) claim the staged image in one
        // transaction, so a comment never ends up referencing media it failed to
        // link (or media stays orphaned after the comment committed).
        let media: Option<(i64, i64)> = {
            let mut tx = st.pool.begin().await.map_err(gql_err)?;
            sqlx::query(
                "INSERT INTO comments \
                   (id, target_type, target_id, parent_id, user_id, body, has_spoiler, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(target_type)
            .bind(input.target_id.0.clone())
            .bind(&parent_id)
            .bind(&user.id)
            .bind(&body)
            .bind(input.has_spoiler)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .map_err(gql_err)?;

            let dims = if let Some(media_id) = input.media_id.as_ref() {
                // Claim the caller's own, not-yet-linked upload. RETURNING gives us
                // the stored dimensions for the response in the same statement.
                let row: Option<(i64, i64)> = sqlx::query_as(
                    "UPDATE comment_media SET comment_id = ? \
                     WHERE id = ? AND user_id = ? AND comment_id IS NULL \
                     RETURNING width, height",
                )
                .bind(&id)
                .bind(&media_id.0)
                .bind(&user.id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(gql_err)?;
                if row.is_none() {
                    // Bad/foreign/already-used media id: abort so the comment isn't
                    // committed with a phantom attachment.
                    return Err(Error::new("attached image not found"));
                }
                row
            } else {
                None
            };
            tx.commit().await.map_err(gql_err)?;
            dims
        };

        log_activity(
            &st.pool,
            &user.id,
            "comment",
            Some(target_type),
            Some(&input.target_id.0),
        )
        .await;
        // Notify the parent comment's author that someone replied — one notification
        // per reply, never to yourself. Fire-and-forget (must not fail the post).
        if let Some(pid) = parent_id.as_ref() {
            let parent_author: Option<String> =
                sqlx::query_scalar("SELECT user_id FROM comments WHERE id = ?")
                    .bind(pid)
                    .fetch_optional(&st.pool)
                    .await
                    .ok()
                    .flatten();
            if let Some(pa) = parent_author {
                if pa != user.id {
                    // Reference the NEW reply (id) as the notification's comment so the
                    // bell previews what the replier said, not the recipient's own text.
                    crate::notify::record(
                        &st.pool,
                        &pa,
                        "reply",
                        Some(&user.id),
                        Some(&id),
                        Some(target_type),
                        Some(&input.target_id.0),
                        None,
                    )
                    .await;
                }
            }
        }
        let media_url = input
            .media_id
            .as_ref()
            .map(|m| crate::media::comment_media_url(&m.0));
        Ok(Comment {
            id: ID(id),
            target_type: target_type.to_string(),
            target_id: input.target_id,
            parent_id: input.parent_id,
            author: UserRef {
                id: ID(user.id),
                username: user.username,
                avatar_url: user.avatar_url,
            },
            body,
            has_spoiler: input.has_spoiler,
            media_url,
            media_width: media.map(|(w, _)| w as i32),
            media_height: media.map(|(_, h)| h as i32),
            created_at: now,
            likes: 0,
            dislikes: 0,
            my_vote: 0,
        })
    }

    /// Like (1), dislike (-1), or clear (0) the viewer's vote on a comment. One vote
    /// per (comment, viewer); re-voting replaces it. Returns the fresh tallies so the
    /// client can update that comment without refetching the thread. A like that pushes
    /// the comment's like count onto a milestone notifies its author (never yourself).
    async fn vote_comment(
        &self,
        ctx: &Context<'_>,
        comment_id: ID,
        value: i32,
    ) -> Result<CommentVote> {
        if !(-1..=1).contains(&value) {
            return Err(Error::new("value must be -1, 0, or 1"));
        }
        let user = require_user(ctx).await?;
        let st = state(ctx);
        // Resolve the comment's author + thread (for the notification and self-check).
        let row: Option<(String, String, String)> =
            sqlx::query_as("SELECT user_id, target_type, target_id FROM comments WHERE id = ?")
                .bind(&comment_id.0)
                .fetch_optional(&st.pool)
                .await
                .map_err(gql_err)?;
        let Some((author_id, target_type, target_id)) = row else {
            return Err(Error::new("comment not found"));
        };
        // You can't vote on your own comment — that would let an author inflate their
        // own like tally (which feeds everyone's milestones and any like-ranking).
        if author_id == user.id {
            return Err(Error::new("you can't vote on your own comment"));
        }
        let prior_likes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM comment_votes WHERE comment_id = ? AND value = 1",
        )
        .bind(&comment_id.0)
        .fetch_one(&st.pool)
        .await
        .map_err(gql_err)?;

        if value == 0 {
            sqlx::query("DELETE FROM comment_votes WHERE comment_id = ? AND user_id = ?")
                .bind(&comment_id.0)
                .bind(&user.id)
                .execute(&st.pool)
                .await
                .map_err(gql_err)?;
        } else {
            sqlx::query(
                "INSERT INTO comment_votes (comment_id, user_id, value, created_at) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT(comment_id, user_id) DO UPDATE SET \
                   value = excluded.value, created_at = excluded.created_at",
            )
            .bind(&comment_id.0)
            .bind(&user.id)
            .bind(value)
            .bind(Utc::now().to_rfc3339())
            .execute(&st.pool)
            .await
            .map_err(gql_err)?;
        }

        let (likes, dislikes): (i64, i64) = sqlx::query_as(
            "SELECT \
               COALESCE(SUM(CASE WHEN value = 1 THEN 1 ELSE 0 END), 0), \
               COALESCE(SUM(CASE WHEN value = -1 THEN 1 ELSE 0 END), 0) \
             FROM comment_votes WHERE comment_id = ?",
        )
        .bind(&comment_id.0)
        .fetch_one(&st.pool)
        .await
        .map_err(gql_err)?;

        // Notify the author for EVERY milestone this like crossed (prior_likes, likes],
        // not just an exact landing — so a like that jumps 4→6 (concurrent likes) still
        // fires milestone 5. `record_like_milestone` is idempotent per (comment, count),
        // so re-crossing after an unlike, or a concurrent double-send, notifies once.
        // (Self-votes are already rejected above, so this is always someone else's like.)
        if value == 1 && likes > prior_likes {
            for &m in crate::notify::LIKE_MILESTONES {
                if m > prior_likes && m <= likes {
                    crate::notify::record_like_milestone(
                        &st.pool,
                        &author_id,
                        &comment_id.0,
                        &target_type,
                        &target_id,
                        m,
                    )
                    .await;
                }
            }
        }

        Ok(CommentVote {
            likes: likes as i32,
            dislikes: dislikes as i32,
            my_vote: value,
        })
    }

    /// Mark the viewer's notifications read. Pass a list of ids to mark just those, or
    /// omit/empty to mark ALL unread read (the "mark all read" action). Returns how many
    /// rows this transition changed. Scoped to the viewer — you can only read your own.
    async fn mark_notifications_read(
        &self,
        ctx: &Context<'_>,
        ids: Option<Vec<ID>>,
    ) -> Result<i32> {
        let user = require_user(ctx).await?;
        let st = state(ctx);
        let now = Utc::now().to_rfc3339();
        // Cap the explicit list: this issues one UPDATE per id inside a single
        // transaction, so an unbounded list holds the single SQLite writer for as long as
        // the caller cares to make it. "Mark all read" (omit `ids`) is the one-statement
        // path for bulk, so no legitimate client needs more than this.
        const MAX_NOTIFICATION_IDS: usize = 200;
        if ids.as_ref().is_some_and(|v| v.len() > MAX_NOTIFICATION_IDS) {
            return Err(Error::new(format!(
                "Too many notification ids (max {MAX_NOTIFICATION_IDS}) — omit `ids` to mark all read"
            )));
        }
        let affected = match ids {
            Some(ids) if !ids.is_empty() => {
                let mut n: u64 = 0;
                let mut tx = st.pool.begin().await.map_err(gql_err)?;
                for id in ids {
                    let r = sqlx::query(
                        "UPDATE notifications SET read_at = ? \
                         WHERE id = ? AND user_id = ? AND read_at IS NULL",
                    )
                    .bind(&now)
                    .bind(&id.0)
                    .bind(&user.id)
                    .execute(&mut *tx)
                    .await
                    .map_err(gql_err)?;
                    n += r.rows_affected();
                }
                tx.commit().await.map_err(gql_err)?;
                n
            }
            _ => sqlx::query(
                "UPDATE notifications SET read_at = ? WHERE user_id = ? AND read_at IS NULL",
            )
            .bind(&now)
            .bind(&user.id)
            .execute(&st.pool)
            .await
            .map_err(gql_err)?
            .rows_affected(),
        };
        Ok(affected as i32)
    }

    async fn login(
        &self,
        ctx: &Context<'_>,
        username: String,
        password: String,
    ) -> Result<Session> {
        let st = state(ctx);
        // Rate-limit by client IP (not username, which would let an attacker lock
        // out a victim). Check before verifying, but only *record* failed attempts
        // below — successful logins must not consume the budget.
        let key = format!("login:{}", client_ip(ctx));
        if let Some(retry) = st.auth_limiter.is_limited(&key) {
            return Err(Error::new(format!(
                "Too many login attempts — try again in {retry}s"
            )));
        }
        // Reject over-long passwords before any Argon2 work (A7). Such a password
        // can never be valid (registration caps at the same length), so this is
        // just the wrong-credentials path.
        if password.len() > MAX_PASSWORD_LEN {
            st.auth_limiter.record(&key);
            return Err(Error::new("Invalid username or password"));
        }
        let row = sqlx::query_as::<_, User>(
            "SELECT id, username, email, password_hash, avatar_url, is_admin, is_banned FROM users WHERE username = ?",
        )
        .bind(&username)
        .fetch_optional(&st.pool)
        .await
        .map_err(gql_err)?;
        // Always run an Argon2 verify — against the real hash if the user exists,
        // else a fixed dummy hash — so login time doesn't reveal whether the
        // username exists (A3). Argon2 is ~10-50ms of pure CPU; run it off the async
        // runtime (spawn_blocking) so a login flood can't stall other request tasks.
        let has_user = row.is_some();
        let phc = match &row {
            Some(u) => u.password_hash.clone(),
            None => DUMMY_PASSWORD_HASH.clone(),
        };
        let password_ok = {
            let password = password.clone();
            tokio::task::spawn_blocking(move || {
                // Always compute the verify (constant-time-ish across exists/not-exists);
                // only a real user with a matching hash counts as success.
                let verified = auth::verify_password(&password, &phc);
                has_user && verified
            })
            .await
            .map_err(gql_err)?
        };
        let user = match row {
            Some(u) if password_ok => u,
            _ => {
                st.auth_limiter.record(&key); // count only failed attempts
                return Err(Error::new("Invalid username or password"));
            }
        };
        // A suspended account can't sign in even with the right password. This
        // is not a failed credential attempt, so it doesn't consume the budget.
        if user.is_banned != 0 {
            return Err(Error::new("This account has been suspended."));
        }
        let tok = new_session(&st.pool, &user.id, st.session_ttl_secs).await?;
        let show_nsfw = user_show_nsfw(&st.pool, &user.id).await;
        Ok(Session {
            token: tok,
            user: build_session_user(&st.pool, &user, show_nsfw).await,
        })
    }

    async fn register(&self, ctx: &Context<'_>, input: RegisterInput) -> Result<Session> {
        let st = state(ctx);
        // Rate-limit by client IP so one source can't mass-create accounts (and
        // can't grief a specific username's registration).
        if let Err(retry) = st
            .auth_limiter
            .check(&format!("register:{}", client_ip(ctx)))
        {
            return Err(Error::new(format!(
                "Too many registration attempts — try again in {retry}s"
            )));
        }
        let username = input.username.trim();
        let email = input.email.trim();
        // Count glyphs, not bytes, so a multibyte username can't slip under the
        // minimum; also cap the maximum and restrict the charset (letters, digits,
        // and `_ - .`) to keep control chars / whitespace / homoglyph tricks out.
        let uname_len = username.chars().count();
        if uname_len < 3 {
            return Err(Error::new("username must be at least 3 characters"));
        }
        if uname_len > 32 {
            return Err(Error::new("username must be at most 32 characters"));
        }
        if !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            return Err(Error::new(
                "username may only contain letters, digits, and _ - .",
            ));
        }
        if email.chars().count() > 254 {
            return Err(Error::new("a valid email is required"));
        }
        if !email.contains('@') {
            return Err(Error::new("a valid email is required"));
        }
        if input.password.len() < 8 {
            return Err(Error::new("password must be at least 8 characters"));
        }
        if input.password.len() > MAX_PASSWORD_LEN {
            return Err(Error::new("password must be at most 1024 characters"));
        }
        // Admin usernames are reserved: open registration must never grant admin
        // (A5). Admin accounts are provisioned/promoted at startup from
        // KOMIKA_ADMIN_USERS + KOMIKA_ADMIN_PASSWORD (see provision_admins), so a
        // stranger cannot squat a configured admin name to self-elevate.
        if st
            .admin_users
            .iter()
            .any(|u| u.eq_ignore_ascii_case(username))
        {
            return Err(Error::new("This username is reserved."));
        }
        // Case-insensitive uniqueness pre-check (L): the DB `UNIQUE` is byte-exact,
        // so without this `mika` and `Mika` register as distinct accounts and one
        // can impersonate the other. (Residual race: two concurrent registrations of
        // case-variant names can still both pass this check; the byte-exact UNIQUE
        // only stops exact-duplicate inserts. A COLLATE NOCASE unique index would
        // close it fully but needs a migration owned elsewhere.)
        let taken: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM users WHERE username = ? COLLATE NOCASE")
                .bind(username)
                .fetch_optional(&st.pool)
                .await
                .map_err(gql_err)?;
        if taken.is_some() {
            return Err(Error::new("username or email already taken"));
        }
        // Argon2 hashing is CPU-bound (~10-50ms): keep it off the async runtime.
        let hash = {
            let pw = input.password.clone();
            tokio::task::spawn_blocking(move || auth::hash_password(&pw))
                .await
                .map_err(gql_err)?
                .map_err(gql_err)?
        };
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let is_admin = false;
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, avatar_url, is_admin, created_at) \
             VALUES (?, ?, ?, ?, NULL, ?, ?)",
        )
        .bind(&id)
        .bind(username)
        .bind(email)
        .bind(&hash)
        .bind(is_admin as i64)
        .bind(&now)
        .execute(&st.pool)
        .await
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                Error::new("username or email already taken")
            } else {
                gql_err(e)
            }
        })?;
        let tok = new_session(&st.pool, &id, st.session_ttl_secs).await?;
        Ok(Session {
            token: tok,
            user: SessionUser {
                id: ID(id),
                username: username.to_string(),
                display_name: None,
                bio: None,
                avatar_url: None,
                is_admin,
                show_nsfw: false, // fresh accounts default to hiding NSFW
                joined_at: now,
            },
        })
    }

    async fn logout(&self, ctx: &Context<'_>) -> Result<bool> {
        let st = state(ctx);
        if let Some(tok) = token(ctx) {
            // Stored column is sha256(token) — hash the presented token to match.
            sqlx::query("DELETE FROM sessions WHERE token = ?")
                .bind(auth::hash_token(&tok))
                .execute(&st.pool)
                .await
                .map_err(gql_err)?;
        }
        Ok(true)
    }

    /// Update the signed-in user's editable profile (display name + bio).
    /// A blank/`null` field clears that value (display falls back to username).
    /// Returns the refreshed `SessionUser`.
    async fn update_profile(
        &self,
        ctx: &Context<'_>,
        input: UpdateProfileInput,
    ) -> Result<SessionUser> {
        let user = require_user(ctx).await?;
        // Trim, then treat an empty string as "clear" (store NULL).
        let display_name = input
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let bio = input
            .bio
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(name) = &display_name {
            if name.chars().count() > 50 {
                return Err(Error::new("display name must be at most 50 characters"));
            }
        }
        if let Some(b) = &bio {
            if b.chars().count() > 500 {
                return Err(Error::new("bio must be at most 500 characters"));
            }
        }
        let st = state(ctx);
        sqlx::query("UPDATE users SET display_name = ?, bio = ? WHERE id = ?")
            .bind(&display_name)
            .bind(&bio)
            .bind(&user.id)
            .execute(&st.pool)
            .await
            .map_err(gql_err)?;
        let show_nsfw = user_show_nsfw(&st.pool, &user.id).await;
        Ok(build_session_user(&st.pool, &user, show_nsfw).await)
    }

    /// Set the signed-in user's NSFW visibility preference (CATALOGUE.md §2).
    /// Returns the new value.
    async fn set_show_nsfw(&self, ctx: &Context<'_>, value: bool) -> Result<bool> {
        let user = require_user(ctx).await?;
        sqlx::query("UPDATE users SET show_nsfw = ? WHERE id = ?")
            .bind(value as i64)
            .bind(&user.id)
            .execute(&state(ctx).pool)
            .await
            .map_err(gql_err)?;
        Ok(value)
    }

    /// Admin "manga DB" console: upsert the per-series overrides (whole-state;
    /// a null field clears that override) and return the recomputed series.
    async fn update_series_admin(
        &self,
        ctx: &Context<'_>,
        input: SeriesAdminInput,
    ) -> Result<Series> {
        require_admin(ctx).await?;
        // Reject absurd/invalid overrides up front (the scanner also clamps, but
        // give the admin a clean error instead of silently coercing).
        if let Some(hours) = input.override_interval_hours {
            if !hours.is_finite() || hours <= 0.0 || hours > 876_000.0 {
                return Err(Error::new(
                    "overrideIntervalHours must be between 0 and 876000 (100 years)",
                ));
            }
        }
        if let Some(poll) = input.poll_every_minutes {
            if poll <= 0 {
                return Err(Error::new("pollEveryMinutes must be a positive integer"));
            }
        }
        let st = state(ctx);
        // Resolve the series FIRST so a bogus id fails before any write — same order as
        // `set_series_paused`. Previously the upsert ran first, so a `w_`-prefixed id
        // persisted a junk `series_admin` row and THEN failed with a masked "Internal
        // error", leaving the admin believing nothing had been written.
        let n = input.series_id.0.parse::<i64>().map_err(|_| {
            Error::new("seriesId must be a numeric Suwayomi series id (not a canonical w_ id)")
        })?;
        let m = st.suwayomi.series(n).await.map_err(gql_err)?;

        let now = Utc::now().to_rfc3339();
        let status = input.status.map(status_word);
        let paused = input.paused.map(|p| p as i64);
        sqlx::query(
            "INSERT INTO series_admin \
               (series_id, override_interval_hours, poll_every_minutes, paused_override, status_override, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(series_id) DO UPDATE SET \
               override_interval_hours = excluded.override_interval_hours, \
               poll_every_minutes = excluded.poll_every_minutes, \
               paused_override = excluded.paused_override, \
               status_override = excluded.status_override, \
               updated_at = excluded.updated_at",
        )
        .bind(input.series_id.0.clone())
        .bind(input.override_interval_hours)
        .bind(input.poll_every_minutes)
        .bind(paused)
        .bind(status)
        .bind(&now)
        .execute(&st.pool)
        .await
        .map_err(gql_err)?;
        // Make the change take effect promptly WITHOUT stranding or thrashing the series.
        // Re-scan directly (like unpause / triggerScan) rather than nulling `next_scan_at`:
        // nulling forced the series due-now, which the next tick reads as "overdue with no
        // new chapter" and wrongly flips it into the accelerated awaiting poll — and NULLs
        // sort ahead of every genuinely-due row, so a burst of admin edits jumps the queue
        // (audit #7). A direct scan reschedules off fresh data and leaves `awaiting` alone
        // (the scan isn't "due"), and un-parks a previously-paused series. If the scan
        // hiccups, fall back to due-now so a parked series still can't be stranded by a
        // cleared/loosened override.
        if let Err(e) = scan_series(st, &m, Utc::now()).await {
            tracing::warn!(series_id = n, error = %e, "updateSeriesAdmin: re-scan failed; marking due-now");
            let _ =
                sqlx::query("UPDATE series_scan_state SET next_scan_at = ? WHERE series_id = ?")
                    .bind(Utc::now().to_rfc3339())
                    .bind(&input.series_id.0)
                    .execute(&st.pool)
                    .await;
        }
        Ok(map_series(st, m).await)
    }

    /// Admin: pause or unpause one series' scanning — the targeted toggle
    /// (`updateSeriesAdmin` is whole-state and would clobber the other overrides).
    /// Sets the forced `paused_override` (winning over the auto-by-status pause);
    /// unpausing also triggers an immediate re-scan so the chapter list and count
    /// refresh at once instead of waiting for the next cadence. Returns the
    /// recomputed series. To CLEAR the override (back to auto-by-status), use
    /// `updateSeriesAdmin` with `paused: null`.
    async fn set_series_paused(
        &self,
        ctx: &Context<'_>,
        series_id: ID,
        paused: bool,
    ) -> Result<Series> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let n = series_id.0.parse::<i64>().map_err(gql_err)?;
        // Resolve the series first so a bogus id fails before any write.
        let m = st.suwayomi.series(n).await.map_err(gql_err)?;
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO series_admin (series_id, paused_override, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(series_id) DO UPDATE SET \
               paused_override = excluded.paused_override, \
               updated_at = excluded.updated_at",
        )
        .bind(&series_id.0)
        .bind(paused as i64)
        .bind(now.to_rfc3339())
        .execute(&st.pool)
        .await
        .map_err(gql_err)?;
        if !paused {
            scan_series(st, &m, now).await.map_err(gql_err)?;
        }
        Ok(map_series(st, m).await)
    }

    /// Admin: force an immediate re-scan of one series, bypassing the adaptive
    /// overdue/pause gating. Returns the series with its refreshed scan state.
    async fn trigger_scan(&self, ctx: &Context<'_>, series_id: ID) -> Result<Series> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let n = series_id.0.parse::<i64>().map_err(gql_err)?;
        let m = st.suwayomi.series(n).await.map_err(gql_err)?;
        scan_series(st, &m, Utc::now()).await.map_err(gql_err)?;
        Ok(map_series(st, m).await)
    }

    /// Admin series-detail editor: edit a canonical work's user-facing metadata as an
    /// override layer — the source-derived fields stay immutable and these overrides
    /// win at read time (like `series_admin` for scan/status). Each field is
    /// three-valued: OMITTED => leave unchanged; null => clear the override; a value =>
    /// set it. `tags` is a whole-list replace of the curated set. Returns the
    /// recomputed series (in the same id shape the caller passed).
    async fn update_series_metadata(
        &self,
        ctx: &Context<'_>,
        input: SeriesMetadataInput,
    ) -> Result<Series> {
        use async_graphql::MaybeUndefined::{Null, Undefined, Value};
        require_admin(ctx).await?;
        let st = state(ctx);
        let work_id = resolve_work_id(&st.pool, &input.series_id.0)
            .await
            .ok_or_else(|| {
                Error::new(
                    "Series is not catalogued — add it to the catalogue before editing its metadata.",
                )
            })?;
        let now = Utc::now().to_rfc3339();

        // All override writes in ONE transaction so a partial failure can't leave a
        // half-applied edit. Each singular column follows the three-valued input:
        // Undefined => leave; Null => clear (NULL); Value => set.
        let mut tx = st.pool.begin().await.map_err(gql_err)?;
        match &input.title {
            Undefined => {}
            Null | Value(_) => {
                let v = match &input.title {
                    Value(v) => Some(v.as_str()),
                    _ => None,
                };
                sqlx::query("UPDATE work SET title_override = ?, updated_at = ? WHERE id = ?")
                    .bind(v)
                    .bind(&now)
                    .bind(&work_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(gql_err)?;
            }
        }
        match &input.description {
            Undefined => {}
            Null | Value(_) => {
                let v = match &input.description {
                    Value(v) => Some(v.as_str()),
                    _ => None,
                };
                sqlx::query(
                    "UPDATE work SET description_override = ?, updated_at = ? WHERE id = ?",
                )
                .bind(v)
                .bind(&now)
                .bind(&work_id)
                .execute(&mut *tx)
                .await
                .map_err(gql_err)?;
            }
        }
        match input.r#type {
            Undefined => {}
            Null | Value(_) => {
                let word = match input.r#type {
                    Value(t) => Some(content_type_word(t)),
                    _ => None,
                };
                sqlx::query(
                    "UPDATE work SET content_type_override = ?, updated_at = ? WHERE id = ?",
                )
                .bind(word)
                .bind(&now)
                .bind(&work_id)
                .execute(&mut *tx)
                .await
                .map_err(gql_err)?;
            }
        }
        match input.is_nsfw {
            Undefined => {}
            Null | Value(_) => {
                let val = match input.is_nsfw {
                    Value(b) => Some(b as i64),
                    _ => None,
                };
                sqlx::query("UPDATE work SET is_nsfw_override = ?, updated_at = ? WHERE id = ?")
                    .bind(val)
                    .bind(&now)
                    .bind(&work_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(gql_err)?;
            }
        }
        // Tags: `tags: []` and `tags: null` both leave zero curated rows → the work
        // reverts to source-derived genres (curated-empty is intentionally not
        // expressible; the console only sends `tags` when the admin edits them).
        match &input.tags {
            Undefined => {}
            Null | Value(_) => {
                sqlx::query("DELETE FROM work_tag WHERE work_id = ?")
                    .bind(&work_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(gql_err)?;
                if let Value(list) = &input.tags {
                    for (i, tag) in list.iter().enumerate() {
                        let t = tag.trim();
                        if t.is_empty() {
                            continue;
                        }
                        sqlx::query(
                            "INSERT OR IGNORE INTO work_tag (work_id, tag, ord) VALUES (?, ?, ?)",
                        )
                        .bind(&work_id)
                        .bind(t)
                        .bind(i as i64)
                        .execute(&mut *tx)
                        .await
                        .map_err(gql_err)?;
                    }
                }
            }
        }
        tx.commit().await.map_err(gql_err)?;
        // `is_nsfw` above may have just changed the effective flag; push it into the two
        // feed tables that store a COPY of it, or `updatesFeed` keeps serving this work to
        // opted-out viewers until the next feed rebuild. See `resync_feed_nsfw`.
        resync_feed_nsfw(&st.pool, std::slice::from_ref(&work_id)).await;
        // The home rails and Browse's genre chips memoize their payloads for
        // `FEED_TTL`, and this edit can change title, type, tags OR the NSFW gate that
        // keys them. Without this the admin's correction is invisible for up to a
        // minute, which reads as "the toggle didn't work" and invites a double-edit.
        clear_feed_caches();

        // Recompute the series in the caller's id shape so the console updates in place.
        if input.series_id.0.starts_with("w_") {
            let work = catalog::load_canonical_work(&st.pool, &work_id)
                .await
                .map_err(gql_err)?
                .ok_or_else(|| Error::new("No such work"))?;
            let chapters = catalog::load_canonical_chapters(&st.pool, &work_id)
                .await
                .map_err(gql_err)?;
            let user = current_user(ctx).await;
            Ok(map_canonical_series(
                &st.pool,
                user.as_ref().map(|u| u.id.as_str()),
                work,
                catalog::main_chapter_count_str(&chapters) as i32,
            )
            .await)
        } else {
            let n = input.series_id.0.parse::<i64>().map_err(gql_err)?;
            let m = resolve_series_cached(st, n).await.map_err(gql_err)?;
            Ok(map_series(st, m).await)
        }
    }

    /// Admin: add an alternative title to a work. The title is indexed into the alias
    /// set (so it drives dedup + search), and — per operator policy that identical
    /// titles are the same work — if it exactly matches ANY OTHER work, those works are
    /// auto-merged INTO this one. Returns the updated series in the caller's id shape.
    async fn add_series_alt_title(
        &self,
        ctx: &Context<'_>,
        id: ID,
        title: String,
    ) -> Result<Series> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let work_id = resolve_work_id(&st.pool, &id.0).await.ok_or_else(|| {
            Error::new("Series is not catalogued — add it to the catalogue before editing.")
        })?;
        let raw = title.trim();
        if raw.is_empty() {
            return Err(Error::new("Alternative title must not be empty."));
        }
        let norm = catalog::add_work_alias(&st.pool, &work_id, raw)
            .await
            .map_err(gql_err)?;
        // Auto-merge every work that shares this exact normalized alias into ONE
        // survivor. The survivor is the richest work (MangaDex-anchored → most sources →
        // lowest id, via pick_survivor), NOT necessarily the edited one, so a rich
        // canonical work is never folded into a bare one (which would drop its
        // description/cover). A too-large match set is almost always a generic/typo'd
        // title rather than a real identity — index it but skip the mass-merge, leaving
        // it to the review queue / consolidate, so one field edit can't destroy dozens
        // of works.
        const MAX_AUTO_MERGE: usize = 8;
        let mut survivor = work_id.clone();
        if !norm.is_empty() {
            let mut involved: Vec<String> = catalog::find_works_by_alias(&st.pool, &norm)
                .await
                .map_err(gql_err)?;
            if !involved.contains(&work_id) {
                involved.push(work_id.clone());
            }
            if involved.len() >= 2 && involved.len() <= MAX_AUTO_MERGE {
                survivor = catalog::pick_survivor(&st.pool, &involved)
                    .await
                    .map_err(gql_err)?;
                for other in involved.iter().filter(|w| *w != &survivor) {
                    // `_ex` + the covers pool: the loser's cached cover BLOB lives in a
                    // separate database with no FK to `work`, so the plain entry point
                    // orphans it forever (8,868 orphans / 1.53 GB measured in prod).
                    catalog::merge_works_ex(&st.pool, Some(&st.cover_pool), other, &survivor)
                        .await
                        .map_err(gql_err)?;
                }
            }
        }
        // Return the survivor — if a merge picked a different canonical work, the edited
        // id may no longer exist, so reload the survivor by its own `w_` shape.
        if survivor == work_id {
            reload_series_in_shape(st, ctx, &id.0, &work_id).await
        } else {
            reload_series_in_shape(st, ctx, &survivor, &survivor).await
        }
    }

    /// Admin: remove an alternative title from a work (matched by its normalized key or
    /// exact text). Does not un-merge anything — merges are one-way.
    async fn remove_series_alt_title(
        &self,
        ctx: &Context<'_>,
        id: ID,
        title: String,
    ) -> Result<Series> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let work_id = resolve_work_id(&st.pool, &id.0).await.ok_or_else(|| {
            Error::new("Series is not catalogued — add it to the catalogue before editing.")
        })?;
        catalog::remove_work_alias(&st.pool, &work_id, title.trim())
            .await
            .map_err(gql_err)?;
        reload_series_in_shape(st, ctx, &id.0, &work_id).await
    }

    /// Admin: consolidate the backlog of duplicate works — works that share an exact
    /// normalized alias but were minted separately (pre-policy or by concurrent ingest).
    ///
    /// A shared alias alone is NOT enough to merge: `merge_works` physically deletes the
    /// loser, and MangaDex alt-titles make unrelated series share aliases (every JoJo
    /// part carries `ジョジョの奇妙な冒険`). Only a 2-work cluster whose shared alias is
    /// the PRIMARY title of both sides, is long enough, and is corroborated by year /
    /// author / cover-pHash is folded — see `consolidate_gate`. Everything else is routed
    /// to the `merge_candidate` review queue.
    ///
    /// `limit` bounds the alias GROUPS examined per call, and every merged
    /// `(loser, survivor)` pair is logged for audit. Returns how many works were merged
    /// away. Single-flighted against `reconcileCatalogue` and the post-ingest sweep — it
    /// will not run concurrently with another merge loop.
    ///
    /// Successive calls RESUME where the last one stopped (`CONSOLIDATE_CURSOR`) and wrap
    /// to the start when the walk runs off the end. Restarting each time made the button
    /// go permanently dead after two clicks: a refused cluster stays in `work_alias`, so
    /// the head of the ordering silts up with refusals and the same `limit` groups get
    /// re-examined forever. A pass that merges nothing is still normal — it means those
    /// `limit` groups were all refused, and the next call moves past them.
    async fn consolidate_exact_duplicates(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 100)] limit: i32,
    ) -> Result<i32> {
        require_admin(ctx).await?;
        let st = state(ctx);
        if RECONCILE_RUNNING
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return Err(Error::new(
                "A catalogue reconcile is already running — try again when it finishes.",
            ));
        }
        let limit = limit.max(1) as i64;
        // RESUME, don't restart. See `CONSOLIDATE_CURSOR`.
        let start = {
            let g = CONSOLIDATE_CURSOR.lock().unwrap_or_else(|e| e.into_inner());
            g.clone()
        };
        let res =
            consolidate_exact_duplicates_from(&st.pool, Some(&st.cover_pool), limit, &start).await;
        if let Ok(out) = &res {
            // Fewer groups than asked for means the keyset walk ran off the end of
            // `work_alias`; wrap so the next call re-examines from the start (and picks
            // up duplicates minted since). Otherwise advance to where this pass stopped.
            let next = if out.groups_seen < limit {
                String::new()
            } else {
                out.cursor.clone()
            };
            *CONSOLIDATE_CURSOR.lock().unwrap_or_else(|e| e.into_inner()) = next;
        }
        RECONCILE_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
        let out = res.map_err(gql_err)?;
        for (loser, survivor) in &out.merged {
            tracing::info!(%loser, %survivor, "consolidateExactDuplicates: merged work");
        }
        tracing::info!(
            merged = out.merged.len(),
            queued = out.queued,
            unqueueable = out.unqueueable,
            stale = out.stale,
            groups_seen = out.groups_seen,
            "consolidateExactDuplicates done"
        );
        Ok(out.merged.len() as i32)
    }

    /// Admin: force `is_nsfw_override` on EVERY catalogued work that has a Suwayomi
    /// source under `sourceId` — mark (or, with `isNsfw: false`, un-mark) an entire
    /// source's series in one shot. Complete + repeatable with no per-series
    /// enumeration or live source browse: a single UPDATE over `source_series → work`.
    /// `isNsfw: true` pins NSFW; `isNsfw: false` CLEARS the override (reverts to
    /// source-derived), so it's a clean undo. Returns how many works were updated.
    /// Only marks series already INGESTED (they need a `work`); run `startSourceIngest`
    /// first to catch a source's not-yet-catalogued series, then re-run this.
    async fn mark_source_nsfw(
        &self,
        ctx: &Context<'_>,
        source_id: String,
        is_nsfw: bool,
    ) -> Result<i32> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let now = Utc::now().to_rfc3339();
        // true → pin override = 1; false → clear (NULL) so it reverts to derived.
        let val: Option<i64> = is_nsfw.then_some(1);
        let res = sqlx::query(
            "UPDATE work SET is_nsfw_override = ?, updated_at = ? \
             WHERE id IN (SELECT work_id FROM source_series \
                          WHERE source_type = 'suwayomi' AND source_id = ?)",
        )
        .bind(val)
        .bind(&now)
        .bind(&source_id)
        .execute(&st.pool)
        .await
        .map_err(gql_err)?;
        let n = res.rows_affected() as i32;
        // Push the new effective flag into the two feed tables that store a COPY of it.
        // Same id set as the UPDATE above, read back rather than threaded through: the
        // UPDATE reports a row COUNT, not which rows (see `resync_feed_nsfw`).
        let touched: Vec<String> = sqlx::query_scalar(
            "SELECT work_id FROM source_series \
             WHERE source_type = 'suwayomi' AND source_id = ?",
        )
        .bind(&source_id)
        .fetch_all(&st.pool)
        .await
        .unwrap_or_default();
        resync_feed_nsfw(&st.pool, &touched).await;
        clear_feed_caches();
        tracing::info!(source_id, is_nsfw, updated = n, "markSourceNsfw");
        Ok(n)
    }

    /// Admin: recompute the DERIVED `is_nsfw` for every ingested Suwayomi work under
    /// the current rule — adult genre tags OR an adult Suwayomi source
    /// (`source_extension.is_nsfw`) — and flip the ones currently mis-stored as SFW.
    /// Backfills the leak where whole adult sources were ingested as SFW (their tags
    /// weren't in the old keyword list and the source flag was ignored). Uses stored
    /// genres from the `series` cache, so no re-fetch. Conservative: only flips
    /// 0 → 1 (never un-flags), and leaves `is_nsfw_override` alone. Returns how many
    /// works flipped; a per-source breakdown is logged for review.
    async fn rederive_suwayomi_nsfw(&self, ctx: &Context<'_>) -> Result<i32> {
        require_admin(ctx).await?;
        let st = state(ctx);
        // The cast is on `ss.source_key`, never on `s.id`: casting the INDEXED side
        // (`CAST(s.id AS TEXT)`) hides the integer primary key from the planner, which
        // then re-scans all 13,802 `suwayomi_series` rows for each of the ~13.8k
        // source_series rows — measured at 10,264 ms for this one query. Casting the
        // other side turns `SCAN s LEFT-JOIN` into
        // `SEARCH s USING INTEGER PRIMARY KEY (rowid=?) LEFT-JOIN`.
        let rows = sqlx::query_as::<_, (String, String, Option<String>, i64, i64, Option<String>)>(
            "SELECT ss.work_id, ss.source_id, s.genre, w.is_nsfw, COALESCE(se.is_nsfw, 0), \
                    w.content_rating \
             FROM source_series ss \
             JOIN work w ON w.id = ss.work_id \
             LEFT JOIN suwayomi_series s ON s.id = CAST(ss.source_key AS INTEGER) \
             LEFT JOIN source_extension se ON se.source_id = ss.source_id \
             WHERE ss.source_type = 'suwayomi'",
        )
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;

        // A work is NSFW if ANY of its Suwayomi sources is adult (genre or source
        // flag) — UNLESS MangaDex authoritatively rated it safe/suggestive, which wins
        // over the unreliable source-level flag (a source flagged NSFW taints every
        // mainstream series it carries; same rule as catalog::mark_work_nsfw). Track
        // the triggering source for the per-source report.
        let mut should_nsfw: HashMap<String, bool> = HashMap::new();
        let mut currently: HashMap<String, i64> = HashMap::new();
        let mut trigger: HashMap<String, String> = HashMap::new();
        for (work_id, source_id, genre_json, w_nsfw, src_nsfw, content_rating) in rows {
            currently.insert(work_id.clone(), w_nsfw);
            let genres: Vec<String> = genre_json
                .as_deref()
                .and_then(|g| serde_json::from_str(g).ok())
                .unwrap_or_default();
            let this_nsfw = (src_nsfw != 0 || genre_is_nsfw(&genres))
                && !matches!(content_rating.as_deref(), Some("safe") | Some("suggestive"));
            let e = should_nsfw.entry(work_id.clone()).or_insert(false);
            if this_nsfw {
                *e = true;
                trigger.entry(work_id).or_insert(source_id);
            }
        }
        let to_flip: Vec<String> = should_nsfw
            .iter()
            .filter(|(wid, &should)| should && currently.get(*wid).copied().unwrap_or(0) == 0)
            .map(|(wid, _)| wid.clone())
            .collect();

        let mut per_source: std::collections::BTreeMap<String, i32> = Default::default();
        for wid in &to_flip {
            if let Some(src) = trigger.get(wid) {
                *per_source.entry(src.clone()).or_default() += 1;
            }
        }
        let now = Utc::now().to_rfc3339();
        for chunk in to_flip.chunks(500) {
            let sql = format!(
                "UPDATE work SET is_nsfw = 1, updated_at = ? WHERE id IN ({})",
                in_placeholders(chunk.len())
            );
            let mut q = sqlx::query(&sql).bind(&now);
            for wid in chunk {
                q = q.bind(wid);
            }
            q.execute(&st.pool).await.map_err(gql_err)?;
        }
        // Push the newly-derived flags into the two feed tables that store a COPY of them
        // (see `resync_feed_nsfw`). Only the flipped works can have changed.
        resync_feed_nsfw(&st.pool, &to_flip).await;
        clear_feed_caches();
        for (src, n) in &per_source {
            tracing::info!(source_id = %src, flipped = n, "rederiveSuwayomiNsfw: source flagged");
        }
        tracing::info!(
            total = to_flip.len(),
            sources = per_source.len(),
            "rederiveSuwayomiNsfw complete"
        );
        Ok(to_flip.len() as i32)
    }

    /// Admin: force an immediate re-scan of every installed Suwayomi source of a
    /// canonical work (each source's `source_key` is a Suwayomi manga id). Returns how
    /// many sources were successfully scanned; unresolvable sources are skipped.
    async fn rescan_work(&self, ctx: &Context<'_>, work_id: ID) -> Result<i32> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let keys: Vec<String> = sqlx::query_scalar(
            "SELECT source_key FROM source_series \
             WHERE work_id = ? AND source_type = 'suwayomi'",
        )
        .bind(&work_id.0)
        .fetch_all(&st.pool)
        .await
        .map_err(gql_err)?;
        let now = Utc::now();
        let mut scanned = 0;
        for key in keys {
            let Ok(n) = key.parse::<i64>() else { continue };
            match st.suwayomi.series(n).await {
                Ok(m) => {
                    if scan_series(st, &m, now).await.is_ok() {
                        scanned += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(source_key = key, error = %e, "rescanWork: skipping unresolvable source")
                }
            }
        }
        Ok(scanned)
    }

    /// Admin "Bugs" panel: re-attempt cover processing for ONE work (after fixing an
    /// upstream image, or to re-check now that the size cap / codecs were widened).
    /// Fetches the source cover, re-encodes, stores it, and clears the recorded
    /// issue on success. Returns true if a cover was stored; false if the source
    /// couldn't be fetched (transient — the issue is left in place). A deterministic
    /// re-failure surfaces as an error (and refreshes the recorded reason).
    async fn retry_cover(&self, ctx: &Context<'_>, work_id: ID) -> Result<bool> {
        require_admin(ctx).await?;
        let st = state(ctx);
        crate::cover::retry_one_cover(
            &st.pool,
            &st.cover_pool,
            &st.mangadex,
            &st.suwayomi,
            &work_id.0,
        )
        .await
        .map_err(gql_err)
    }

    /// Admin series-detail editor: soft-hide (reversible) or rename one chapter of a
    /// work by aggregate number. Non-destructive — the cached chapter rows are
    /// untouched, so a re-scan can't resurrect a hidden chapter. Clearing both fields
    /// removes the override row entirely.
    async fn set_chapter_override(
        &self,
        ctx: &Context<'_>,
        input: ChapterOverrideInput,
    ) -> Result<bool> {
        use async_graphql::MaybeUndefined::{Null, Undefined, Value};
        require_admin(ctx).await?;
        let st = state(ctx);
        let exists = sqlx::query_scalar::<_, i64>("SELECT 1 FROM work WHERE id = ?")
            .bind(&input.work_id.0)
            .fetch_optional(&st.pool)
            .await
            .map_err(gql_err)?;
        if exists.is_none() {
            return Err(Error::new("No such work"));
        }
        // Read-modify-write under ONE transaction so two concurrent partial edits (one
        // toggling `hidden`, one renaming) can't clobber each other's field.
        let mut tx = st.pool.begin().await.map_err(gql_err)?;
        let existing = sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT hidden, title_override FROM chapter_override WHERE work_id = ? AND chapter_key = ?",
        )
        .bind(&input.work_id.0)
        .bind(&input.chapter_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(gql_err)?;
        let (mut hidden, mut title) = existing.map(|(h, t)| (h != 0, t)).unwrap_or((false, None));
        match input.hidden {
            Undefined => {}
            Null => hidden = false,
            Value(b) => hidden = b,
        }
        match input.title {
            Undefined => {}
            Null => title = None,
            Value(v) => {
                let v = v.trim().to_string();
                title = if v.is_empty() { None } else { Some(v) };
            }
        }
        // A no-op override (visible + no rename) is stored as the absence of a row.
        if !hidden && title.is_none() {
            sqlx::query("DELETE FROM chapter_override WHERE work_id = ? AND chapter_key = ?")
                .bind(&input.work_id.0)
                .bind(&input.chapter_key)
                .execute(&mut *tx)
                .await
                .map_err(gql_err)?;
            tx.commit().await.map_err(gql_err)?;
            return Ok(true);
        }
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO chapter_override (work_id, chapter_key, hidden, title_override, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(work_id, chapter_key) DO UPDATE SET \
               hidden = excluded.hidden, \
               title_override = excluded.title_override, \
               updated_at = excluded.updated_at",
        )
        .bind(&input.work_id.0)
        .bind(&input.chapter_key)
        .bind(hidden as i64)
        .bind(&title)
        .bind(&now)
        .execute(&mut *tx)
        .await
        .map_err(gql_err)?;
        tx.commit().await.map_err(gql_err)?;
        Ok(true)
    }

    /// Admin moderation: suspend or restore a user account. A banned user can't
    /// sign in and their active sessions are revoked immediately. Admins can't
    /// ban themselves or another admin.
    async fn ban_user(&self, ctx: &Context<'_>, user_id: ID, banned: bool) -> Result<AdminUser> {
        let admin = require_admin(ctx).await?;
        let st = state(ctx);
        if user_id.0 == admin.id {
            return Err(Error::new("You cannot ban your own account."));
        }
        let target: Option<(String, String, String, Option<String>, i64, String)> = sqlx::query_as(
            "SELECT id, username, email, avatar_url, is_admin, created_at FROM users WHERE id = ?",
        )
        .bind(&user_id.0)
        .fetch_optional(&st.pool)
        .await
        .map_err(gql_err)?;
        let Some((id, username, email, avatar_url, is_admin, created_at)) = target else {
            return Err(Error::new("No such user."));
        };
        if is_admin != 0 {
            return Err(Error::new("You cannot ban an admin account."));
        }
        sqlx::query("UPDATE users SET is_banned = ? WHERE id = ?")
            .bind(banned as i64)
            .bind(&id)
            .execute(&st.pool)
            .await
            .map_err(gql_err)?;
        if banned {
            // Revoke active sessions so the ban takes effect at once.
            sqlx::query("DELETE FROM sessions WHERE user_id = ?")
                .bind(&id)
                .execute(&st.pool)
                .await
                .map_err(gql_err)?;
        }
        Ok(AdminUser {
            id: ID(id),
            username,
            email,
            avatar_url,
            is_admin: is_admin != 0,
            is_banned: banned,
            created_at,
        })
    }

    /// Admin moderation: delete a comment and its entire reply subtree. Returns
    /// false if it was already gone. (Authors don't self-delete here — this is the
    /// mod action.) The subtree and its attached images are removed explicitly
    /// (via a recursive CTE) rather than relying on `ON DELETE CASCADE`, so it
    /// behaves the same on connections without the foreign_keys pragma.
    async fn delete_comment(&self, ctx: &Context<'_>, comment_id: ID) -> Result<bool> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let mut tx = st.pool.begin().await.map_err(gql_err)?;
        // Drop the attached images of the whole subtree first (FK-safe ordering).
        sqlx::query(
            "WITH RECURSIVE subtree(id) AS ( \
                 SELECT id FROM comments WHERE id = ? \
                 UNION ALL \
                 SELECT c.id FROM comments c JOIN subtree s ON c.parent_id = s.id \
             ) \
             DELETE FROM comment_media WHERE comment_id IN (SELECT id FROM subtree)",
        )
        .bind(&comment_id.0)
        .execute(&mut *tx)
        .await
        .map_err(gql_err)?;
        let res = sqlx::query(
            "WITH RECURSIVE subtree(id) AS ( \
                 SELECT id FROM comments WHERE id = ? \
                 UNION ALL \
                 SELECT c.id FROM comments c JOIN subtree s ON c.parent_id = s.id \
             ) \
             DELETE FROM comments WHERE id IN (SELECT id FROM subtree)",
        )
        .bind(&comment_id.0)
        .execute(&mut *tx)
        .await
        .map_err(gql_err)?;
        tx.commit().await.map_err(gql_err)?;
        Ok(res.rows_affected() > 0)
    }

    /// Admin user-management: grant or revoke a user's admin flag. An admin can't
    /// revoke their own access (that could lock every admin out of the console).
    async fn set_user_admin(
        &self,
        ctx: &Context<'_>,
        user_id: ID,
        is_admin: bool,
    ) -> Result<AdminUser> {
        let admin = require_admin(ctx).await?;
        let st = state(ctx);
        if user_id.0 == admin.id && !is_admin {
            return Err(Error::new("You cannot remove your own admin access."));
        }
        let res = sqlx::query("UPDATE users SET is_admin = ? WHERE id = ?")
            .bind(is_admin as i64)
            .bind(&user_id.0)
            .execute(&st.pool)
            .await
            .map_err(gql_err)?;
        if res.rows_affected() == 0 {
            return Err(Error::new("No such user."));
        }
        let row: AdminUserRow = sqlx::query_as(
            "SELECT id, username, email, avatar_url, is_admin, is_banned, created_at \
             FROM users WHERE id = ?",
        )
        .bind(&user_id.0)
        .fetch_one(&st.pool)
        .await
        .map_err(gql_err)?;
        Ok(row.into())
    }

    /// Tier-2 add flow (CATALOGUE.md §6): pull a source series' detail from Suwayomi,
    /// run the dedup matcher, and wire it into the canonical model. `auto_merge` links
    /// straight to the matched work; `review` creates a provisional work AND enqueues a
    /// `merge_candidate` for manual confirmation; `new` creates a first-class work.
    async fn add_source_series(
        &self,
        ctx: &Context<'_>,
        suwayomi_manga_id: ID,
    ) -> Result<MatchResult> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let mid: i64 = suwayomi_manga_id
            .0
            .parse()
            .map_err(|_| Error::new("suwayomiMangaId must be an integer id"))?;
        let m = st.suwayomi.series(mid).await.map_err(gql_err)?;

        // DD1: compute the cover pHash server-side (strongest cheap dedup signal).
        // Best-effort — `None` on any fetch/decode failure just drops the signal.
        // Done here (the only async/network step) so the core is unit-testable
        // without a live Suwayomi.
        //
        // This WAITS on the shared bounded cover-fetch pool and eats into the reader's
        // on-demand headroom — one permit per in-flight call, and one call per request
        // here. See the full accounting on `ingest_source_series` before making any enrol
        // path concurrent.
        let cover_phash = match st.suwayomi.cover_bytes(m.thumbnail_url.as_deref()).await {
            // dhash decodes + grayscales + resizes the cover — CPU-bound; keep it off
            // the async runtime. Best-effort: a task panic just drops the signal.
            Some(bytes) => tokio::task::spawn_blocking(move || crate::phash::dhash(&bytes))
                .await
                .unwrap_or(None),
            None => None,
        };
        let result = add_source_series_core(&st.pool, &m, cover_phash)
            .await
            .map_err(gql_err)?;
        // Register a "due now" scan-state row FIRST, independent of the scan-on-enrol
        // below. The DB-driven scanner selects work only from `series_scan_state`, so
        // without a row a single-add whose enrol-time scan hiccups (network / FlareSolverr
        // stall) would be invisible to the scheduler until the daily reconcile backfill —
        // the exact "single-added series never updated" bug this feature exists to fix.
        // `ensure_pending` is an idempotent `ON CONFLICT DO NOTHING`, so a successful scan
        // right after cleanly overwrites the schedule (mirrors `federated_ingest`).
        if let Err(e) = crate::scanner::ensure_pending(&st.pool, &mid.to_string()).await {
            tracing::warn!(
                series_id = m.id,
                error = %e,
                "ensure_pending after enrol failed; reconcile backfill will retry"
            );
        }
        // Enrol the manga in the Suwayomi library so it stays in-library upstream (the
        // reconcile pass re-asserts this). NOTE: scan eligibility is driven by the
        // `series_scan_state` row above, not library membership — the DB-driven scanner
        // no longer iterates `suwayomi.library()`. Best-effort: a hiccup here must not fail
        // an otherwise-successful enrol.
        if let Err(e) = st.suwayomi.set_in_library(mid, true).await {
            tracing::warn!(
                series_id = m.id,
                error = %e,
                "set_in_library after enrol failed; reconcile will retry"
            );
        }
        // Populate this series' chapters + scan state NOW, so its chapters (and the
        // chapter count / updates feed derived from scan state) surface immediately
        // instead of waiting for the next adaptive scan tick (SCAN_TICK_SECONDS).
        // Best-effort: a scan hiccup must never fail an otherwise-successful enrol — the
        // scheduler retries on its next pass via the `ensure_pending` row above.
        // Idempotent (record_scan is a read-modify-write keyed on the series) and
        // rate-limit-safe (a single chapters fetch for this one series).
        if let Err(e) = scan_series(st, &m, Utc::now()).await {
            tracing::warn!(
                series_id = m.id,
                error = %e,
                "immediate scan after enrol failed; will retry on next tick"
            );
        }
        Ok(result)
    }

    /// Admin (D1): fold one canonical work into another — for cleaning up two
    /// already-created duplicates (the `merge_candidate` review flow only handles a
    /// source-series-vs-provisional at add time, not two existing works). All of
    /// the source work's source-series mappings + user data (library, progress,
    /// reviews, comments) re-point to the target; the target keeps its identity and
    /// gains the source's aliases/external-ids (and cover pHash if it had none); the
    /// empty source work is deleted. Pick `target` as the canonical/enriched one
    /// (e.g. the MangaDex-anchored work).
    async fn merge_works(
        &self,
        ctx: &Context<'_>,
        source_work_id: ID,
        target_work_id: ID,
    ) -> Result<MergeWorksResult> {
        require_admin(ctx).await?;
        let st = state(ctx);
        // `_ex` so the losing work's cached cover blob is reclaimed from the separate
        // covers DB rather than orphaned (see catalog::merge_works_ex).
        let outcome = catalog::merge_works_ex(
            &st.pool,
            Some(&st.cover_pool),
            &source_work_id.0,
            &target_work_id.0,
        )
        .await
        .map_err(gql_err)?;
        Ok(MergeWorksResult {
            target_work_id,
            moved_source_series: outcome.moved_source_series as i32,
        })
    }

    /// Resolve a pending dedup review. `accept` folds the source series' whole work
    /// into the candidate work (via merge_works — aliases, sources, external ids, and
    /// user data all move, then the emptied work is dropped), fully consolidating a
    /// duplicate that may span several sources; rejecting keeps the source work as a
    /// distinct first-class entry. Either way the row is closed.
    async fn resolve_merge_candidate(
        &self,
        ctx: &Context<'_>,
        id: ID,
        accept: bool,
    ) -> Result<bool> {
        require_admin(ctx).await?;
        let st = state(ctx);

        #[derive(sqlx::FromRow)]
        struct Row {
            source_series_id: String,
            candidate_work_id: String,
            status: String,
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT source_series_id, candidate_work_id, status FROM merge_candidate WHERE id = ?",
        )
        .bind(&id.0)
        .fetch_optional(&st.pool)
        .await
        .map_err(gql_err)?;
        let Some(row) = row else {
            return Err(Error::new("No such merge candidate."));
        };
        // Fast-path only — the correctness guard is the atomic claim below.
        if row.status != "pending" {
            return Err(Error::new("This merge candidate is already resolved."));
        }

        let mut tx = st.pool.begin().await.map_err(gql_err)?;

        // Atomically claim the candidate: only the admin whose UPDATE flips a
        // still-`pending` row proceeds. A concurrent resolver that already claimed
        // it leaves `rows_affected() == 0` here, so the loser never repoints.
        let now = Utc::now().to_rfc3339();
        let claim = sqlx::query(
            "UPDATE merge_candidate SET status = ?, resolved_at = ? \
             WHERE id = ? AND status = 'pending'",
        )
        .bind(if accept { "confirmed" } else { "rejected" })
        .bind(&now)
        .bind(&id.0)
        .execute(&mut *tx)
        .await
        .map_err(gql_err)?;
        if claim.rows_affected() == 0 {
            tx.rollback().await.map_err(gql_err)?;
            return Err(Error::new("This merge candidate is already resolved."));
        }

        // Claimed the row. Commit the status flip first, then (on accept) fold the
        // whole source work into the candidate. merge_works runs its own transaction,
        // so the claim must be committed before it — otherwise the two contend for the
        // single SQLite writer and deadlock.
        tx.commit().await.map_err(gql_err)?;

        if accept {
            let old_work: Option<String> =
                sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = ?")
                    .bind(&row.source_series_id)
                    .fetch_optional(&st.pool)
                    .await
                    .map_err(gql_err)?;
            // Fold the ENTIRE source work into the candidate work, not just this one
            // source series. A reconcile-originated candidate represents a duplicate
            // WORK that may carry several sources (the 3-works-across-5-sources case);
            // moving one source would leave the duplicate behind. merge_works folds
            // aliases/external-ids/sources/user-data and drops the emptied work — the
            // same consolidation reconcile's AutoMerge path uses. Skip if the source
            // already belongs to the candidate (self-referential; nothing to do).
            if let Some(old) = old_work {
                if old != row.candidate_work_id {
                    // The status flip is already committed. If the fold fails, revert the
                    // candidate to `pending` — otherwise it's stuck `confirmed` with the
                    // duplicate un-merged and the admin can't retry it via the console.
                    if let Err(e) = catalog::merge_works_ex(
                        &st.pool,
                        Some(&st.cover_pool),
                        &old,
                        &row.candidate_work_id,
                    )
                    .await
                    {
                        let _ = sqlx::query(
                            "UPDATE merge_candidate SET status = 'pending', resolved_at = NULL \
                             WHERE id = ?",
                        )
                        .bind(&id.0)
                        .execute(&st.pool)
                        .await;
                        return Err(gql_err(e));
                    }
                }
            }
        }

        Ok(true)
    }

    // ---- Sources & Extensions admin surface (EXT-1) --------------------------

    /// Admin: register an extension repo (store) on the Suwayomi engine by its
    /// index URL and refresh the available-extension list from every store.
    /// Idempotent upstream (re-adding an existing store is a no-op). Returns how
    /// many extensions are now known.
    async fn add_extension_repo(&self, ctx: &Context<'_>, index_url: String) -> Result<i32> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let url = index_url.trim();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(Error::new("indexUrl must be an http(s) URL"));
        }
        st.suwayomi
            .add_extension_store(url)
            .await
            .map_err(gql_err)?;
        let count = st.suwayomi.refresh_extensions().await.map_err(gql_err)?;
        Ok(count as i32)
    }

    /// Admin: install a store extension onto the Suwayomi engine. An NSFW
    /// extension is refused unless the admin opted in (show_nsfw posture — the
    /// listing hides it, and it can't be installed by pkgName either).
    async fn install_extension(
        &self,
        ctx: &Context<'_>,
        pkg_name: String,
    ) -> Result<ExtensionInfo> {
        let user = require_admin(ctx).await?;
        let st = state(ctx);
        if !user_show_nsfw(&st.pool, &user.id).await {
            let ext = st
                .suwayomi
                .get_extension(&pkg_name)
                .await
                .map_err(gql_err)?;
            if ext.is_nsfw {
                return Err(Error::new(
                    "This extension is NSFW — enable NSFW in your settings to install it",
                ));
            }
        }
        let e = st
            .suwayomi
            .install_extension(&pkg_name)
            .await
            .map_err(gql_err)?;
        Ok(map_extension_info(st, e))
    }

    /// Admin: uninstall an extension from the Suwayomi engine. No NSFW gate —
    /// removing content is always allowed.
    async fn uninstall_extension(
        &self,
        ctx: &Context<'_>,
        pkg_name: String,
    ) -> Result<ExtensionInfo> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let e = st
            .suwayomi
            .uninstall_extension(&pkg_name)
            .await
            .map_err(gql_err)?;
        // Clear any sync subscription for the now-removed extension so it doesn't linger as
        // invisible state (the Sync toggle only renders for installed extensions) and
        // silently resume syncing on a later reinstall (audit LOW). Best-effort.
        if let Err(err) = catalog::set_extension_subscription(&st.pool, &pkg_name, false).await {
            tracing::warn!(pkg = %pkg_name, error = %err, "failed to clear subscription on uninstall");
        }
        Ok(map_extension_info(st, e))
    }

    /// Admin: update an installed extension to the store's latest version. Gated
    /// like `installExtension` (an update installs a new APK).
    async fn update_extension(&self, ctx: &Context<'_>, pkg_name: String) -> Result<ExtensionInfo> {
        let user = require_admin(ctx).await?;
        let st = state(ctx);
        if !user_show_nsfw(&st.pool, &user.id).await {
            let ext = st
                .suwayomi
                .get_extension(&pkg_name)
                .await
                .map_err(gql_err)?;
            if ext.is_nsfw {
                return Err(Error::new(
                    "This extension is NSFW — enable NSFW in your settings to update it",
                ));
            }
        }
        let e = st
            .suwayomi
            .update_extension(&pkg_name)
            .await
            .map_err(gql_err)?;
        Ok(map_extension_info(st, e))
    }

    /// Admin (S2/H1/F2): backfill MangaDex enrichment — all-language alt titles,
    /// localized descriptions, full author/artist credits, AND the full per-volume
    /// cover set (F2) — for works anchored to MangaDex not yet enriched. Selects on
    /// `metadata_synced_at IS NULL OR covers_synced_at IS NULL`, and marks both, so
    /// it advances past works legitimately lacking descriptions/covers instead of
    /// re-selecting them forever (drains, doesn't thrash). Metadata is fetched
    /// batched (100/req); covers are one `/cover` request per work, so the default
    /// `limit` is modest — call repeatedly until it returns 0.
    async fn backfill_mangadex_metadata(
        &self,
        ctx: &Context<'_>,
        #[graphql(default = 200)] limit: i32,
    ) -> Result<i32> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let limit = limit.clamp(1, 500) as i64;
        let ids = works_needing_enrichment(&st.pool, limit).await?;
        enrich_works(st, &ids).await
    }

    /// Admin (S1): "save everything to DB" — materialize the whole Suwayomi library
    /// into the DB cache so reader loads serve from SQLite. Series METADATA (title,
    /// cover reference, description, author/artist, genres, status, chapter count)
    /// is written synchronously from one `library()` call and returned immediately;
    /// the per-series CHAPTER LISTS are filled by a spawned background task (each is
    /// one source fetch, so a large library would otherwise block the request past
    /// client/proxy timeouts). PRODUCTION maintenance action gated ONLY by
    /// `require_admin` (admin identity is the gate — no separate feature flag).
    /// Returns how many series were persisted.
    async fn persist_catalogue(&self, ctx: &Context<'_>) -> Result<i32> {
        require_admin(ctx).await?;
        let st_arc = ctx.data_unchecked::<std::sync::Arc<AppState>>().clone();
        let library = st_arc.suwayomi.library().await.map_err(gql_err)?;
        let mut ids = Vec::with_capacity(library.len());
        let mut persisted = 0i32;
        let mut refused = 0i32;
        for mut m in library {
            m.in_library = true;
            // `put_series` REFUSES a non-English series (its English-only backstop —
            // caching one is what let purged rows resurrect). It reports that refusal as
            // `Ok(false)`, so a refusal is neither counted as a persist nor pushed into
            // the background chapter fill below, which would spend one Suwayomi source
            // fetch per refused series on chapters `put_chapters` then discards (it skips
            // a series with no cached row). Enforcement lives in `put_series` alone —
            // this used to mirror the predicate here, which risked the two drifting.
            match crate::series_cache::put_series(&st_arc.pool, &m).await {
                Ok(true) => {
                    ids.push(m.id);
                    persisted += 1;
                }
                Ok(false) => refused += 1,
                Err(e) => {
                    tracing::warn!(series_id = m.id, error = %e, "persistCatalogue: series write failed")
                }
            }
        }
        // Fill chapter lists in the background (best-effort, sequential + polite) so
        // the request returns immediately even for a large library.
        let st_bg = st_arc.clone();
        tokio::spawn(async move {
            for id in ids {
                match st_bg.suwayomi.chapters(id).await {
                    Ok(chapters) => {
                        let _ = crate::series_cache::put_chapters(&st_bg.pool, id, &chapters).await;
                    }
                    Err(e) => {
                        tracing::warn!(series_id = id, error = %e, "persistCatalogue(bg): chapter fetch failed")
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            tracing::info!("persistCatalogue: background chapter fill complete");
        });
        tracing::info!(
            persisted,
            refused,
            "persistCatalogue: metadata materialized; chapters filling in background"
        );
        Ok(persisted)
    }

    /// Admin: materialize the whole catalogue's cover images into the DB
    /// (`work_cover_blob`) so the WEB reader serves covers from our own origin
    /// (`/covers/{id}.webp`) instead of routing every cover through the Cloudflare
    /// image Worker. Fetches each still-uncached work's MangaDex cover, re-encodes
    /// to a bounded WebP, and stores the bytes — a polite background crawl bounded
    /// by the MangaDex rate limiter (a full catalogue takes a while). Returns how
    /// many works are QUEUED (still missing a cover) at kick-off; progress lands in
    /// the server logs. Idempotent + resumable (only `cover_cached_version IS NULL`
    /// works are processed), and single-flighted so overlapping runs can't hammer
    /// MangaDex. Gated by `require_admin`.
    async fn materialize_catalogue_covers(&self, ctx: &Context<'_>) -> Result<i32> {
        require_admin(ctx).await?;
        let st = ctx.data_unchecked::<std::sync::Arc<AppState>>().clone();
        // Single-flight: refuse a second concurrent crawl (compare-and-set false→true).
        if st
            .cover_crawl_running
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return Err(gql_err(
                "a cover materialization run is already in progress",
            ));
        }
        let queued = crate::cover::pending_cover_count(&st.pool)
            .await
            .map_err(gql_err)? as i32;
        tokio::spawn(async move {
            crate::cover::crawl_uncached_covers(
                &st.pool,
                &st.cover_pool,
                &st.mangadex,
                &st.suwayomi,
                None,
            )
            .await;
            st.cover_crawl_running
                .store(false, std::sync::atomic::Ordering::SeqCst);
        });
        tracing::info!(queued, "materializeCatalogueCovers: crawl started");
        Ok(queued)
    }

    /// Admin: reconcile the existing (provisional, Suwayomi-only) catalogue against
    /// the MangaDex spine. For each work that has a Suwayomi source but no MangaDex
    /// source, re-run the dedup matcher against the catalogue: an exact-title (or
    /// external-id) match folds the provisional work INTO the matched work; a
    /// mid-confidence match queues a `merge_candidate` for human review; no match
    /// leaves it as its own work. Runs in the background (batched + paced so it
    /// coexists with the live catalogue sync); returns how many provisional works are
    /// pending reconciliation at kickoff. Idempotent + re-runnable (merged works
    /// vanish, queued works are skipped via their pending candidate). Single-flighted.
    async fn reconcile_catalogue(&self, ctx: &Context<'_>) -> Result<i32> {
        require_admin(ctx).await?;
        let st = ctx.data_unchecked::<std::sync::Arc<AppState>>().clone();
        if RECONCILE_RUNNING
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_err()
        {
            return Err(gql_err("a catalogue reconcile is already in progress"));
        }
        let pending = pending_reconcile_count(&st.pool).await.map_err(gql_err)? as i32;
        tokio::spawn(async move {
            match reconcile_provisional_works(&st.pool, Some(&st.cover_pool)).await {
                Ok((merged, queued, skipped)) => {
                    tracing::info!(merged, queued, skipped, "reconcile: complete")
                }
                Err(e) => tracing::error!(error = %e, "reconcile: failed"),
            }
            RECONCILE_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
        });
        tracing::info!(pending, "reconcile: started");
        Ok(pending)
    }

    /// Admin: force a full catalogue re-seed. Clears the `catalogue` + `chapters`
    /// sync cursors (so `seed_done` resets and the next cycle walks the whole
    /// `createdAt` history from scratch) and kicks one sync cycle in the background.
    /// Needed because a truncated seed latches `seed_done = true` and the recurring
    /// loop then only does incremental refreshes — and the flag can't be reset via
    /// raw SQL (container-owned DB, no sqlite3 in the image). Single-flighted: refused
    /// while any sync cycle (recurring or manual) is already running. Watch progress
    /// in the logs (`catalogue page offset=` climbing, then `catalogue cycle done`).
    async fn resync_catalogue(&self, ctx: &Context<'_>) -> Result<bool> {
        require_admin(ctx).await?;
        let st = ctx.data_unchecked::<std::sync::Arc<AppState>>().clone();
        if !crate::mangadex::spawn_resync(
            st.pool.clone(),
            st.mangadex.clone(),
            st.catalogue_cover_phash,
        ) {
            return Err(gql_err(
                "a catalogue sync cycle is already running; retry in a moment",
            ));
        }
        tracing::info!("resyncCatalogue: fresh full seed kicked");
        Ok(true)
    }

    /// Admin: start an "add all from this source" background ingest job (S1).
    /// Walks the source's POPULAR listing page by page and runs every entry
    /// through the Tier-2 dedup add flow; progress is persisted on the job row
    /// (poll `sourceIngestJobs`). Refused while a job is already running for the
    /// source, and NSFW sources are refused unless the admin opted in.
    async fn start_source_ingest(
        &self,
        ctx: &Context<'_>,
        source_id: ID,
    ) -> Result<SourceIngestJob> {
        let user = require_admin(ctx).await?;
        let st_arc = ctx.data_unchecked::<std::sync::Arc<AppState>>().clone();
        // Validates the source exists AND carries the NSFW posture gate + the
        // English-only ingest policy (single-source sibling of `start_extension_ingest`,
        // which filters languages at the extension level).
        let (_, source_nsfw, source_lang) = st_arc
            .suwayomi
            .source_meta(&source_id.0)
            .await
            .map_err(gql_err)?;
        if source_nsfw && !user_show_nsfw(&st_arc.pool, &user.id).await {
            return Err(Error::new(
                "This source is NSFW — enable NSFW in your settings to ingest it",
            ));
        }
        // English-only: ingesting a non-English source (e.g. a per-language MangaDex
        // source) would enrol native-language series that the reader can't serve and
        // that the reconcile purge would immediately delete. Refuse it up front.
        if source_lang != "en" {
            return Err(Error::new(
                "Komika serves English only — this source is not an English source",
            ));
        }
        let Some(job) = crate::ingest::try_start_job(&st_arc.pool, &source_id.0)
            .await
            .map_err(gql_err)?
        else {
            return Err(Error::new(
                "An ingest job is already running for this source",
            ));
        };
        crate::ingest::spawn_runner(st_arc, job.id.clone(), source_id.0.clone());
        Ok(job.into())
    }

    /// Admin: request cancellation of a running ingest job. The runner observes
    /// it between items and stops; progress up to that point is preserved.
    /// Cancelling an already-finished job is a no-op returning the row as-is.
    async fn cancel_source_ingest(&self, ctx: &Context<'_>, job_id: ID) -> Result<SourceIngestJob> {
        require_admin(ctx).await?;
        let st = state(ctx);
        crate::ingest::cancel_job(&st.pool, &job_id.0)
            .await
            .map_err(gql_err)?
            .map(Into::into)
            .ok_or_else(|| Error::new("No such ingest job"))
    }

    /// Admin (F1): "add all from the whole EXTENSION" — start an ingest job for
    /// EVERY installed Suwayomi source belonging to `pkgName` in one action (an
    /// extension like MangaDex exposes ~60 per-language sources). NSFW sources are
    /// skipped for an opted-out admin; a source that already has a running job is
    /// skipped (its existing job is still returned so the UI can track them
    /// together), never erroring the whole call. Errors only when no source
    /// matches the package. Returns every started + already-running job.
    async fn start_extension_ingest(
        &self,
        ctx: &Context<'_>,
        pkg_name: ID,
    ) -> Result<Vec<SourceIngestJob>> {
        let user = require_admin(ctx).await?;
        let st_arc = ctx.data_unchecked::<std::sync::Arc<AppState>>().clone();
        let show_nsfw = user_show_nsfw(&st_arc.pool, &user.id).await;
        let sources = st_arc.suwayomi.list_sources().await.map_err(gql_err)?;
        let matching: Vec<_> = sources
            .into_iter()
            .filter(|s| s.pkg_name.as_deref() == Some(pkg_name.0.as_str()))
            .filter(|s| show_nsfw || !s.is_nsfw)
            // English-only, as in the background source-sync (`sync::sync_extension`):
            // a multi-language extension like `all.mangadex` fans out into ~70
            // per-language sources; ingesting all of them enrolled non-English series
            // that leaked into Browse. Komika serves English only.
            .filter(|s| s.lang == "en")
            .collect();
        if matching.is_empty() {
            return Err(Error::new(
                "No installed, English, visible sources for this extension",
            ));
        }
        let mut jobs = Vec::new();
        for src in matching {
            match crate::ingest::try_start_job(&st_arc.pool, &src.id)
                .await
                .map_err(gql_err)?
            {
                Some(job) => {
                    crate::ingest::spawn_runner(st_arc.clone(), job.id.clone(), src.id.clone());
                    jobs.push(job.into());
                }
                // Already running for this source — surface its existing job.
                None => {
                    if let Some(existing) = crate::ingest::load_running_job(&st_arc.pool, &src.id)
                        .await
                        .map_err(gql_err)?
                    {
                        jobs.push(existing.into());
                    }
                }
            }
        }
        Ok(jobs)
    }

    /// Admin (F1): cancel every running ingest job for an extension's sources.
    /// Returns the cancelled job rows (empty if none were running).
    async fn cancel_extension_ingest(
        &self,
        ctx: &Context<'_>,
        pkg_name: ID,
    ) -> Result<Vec<SourceIngestJob>> {
        require_admin(ctx).await?;
        let st = state(ctx);
        let source_ids: Vec<String> = st
            .suwayomi
            .list_sources()
            .await
            .map_err(gql_err)?
            .into_iter()
            .filter(|s| s.pkg_name.as_deref() == Some(pkg_name.0.as_str()))
            .map(|s| s.id)
            .collect();
        let cancelled = crate::ingest::cancel_running_for_sources(&st.pool, &source_ids)
            .await
            .map_err(gql_err)?;
        Ok(cancelled.into_iter().map(Into::into).collect())
    }

    /// Admin: subscribe/unsubscribe an extension for background source-sync. While
    /// subscribed, the sync job periodically re-walks the extension's sources (LATEST)
    /// to auto-enrol newly-appeared series and reconcile library membership, so new
    /// series show up (and keep updating) without a manual add. Enabling kicks an
    /// immediate sync pass in the background so the admin doesn't wait for the interval;
    /// this does NOT backfill the whole catalogue — use `startExtensionIngest` for that.
    async fn set_extension_subscription(
        &self,
        ctx: &Context<'_>,
        pkg_name: ID,
        subscribed: bool,
    ) -> Result<bool> {
        require_admin(ctx).await?;
        let st_arc = ctx.data_unchecked::<std::sync::Arc<AppState>>().clone();
        catalog::set_extension_subscription(&st_arc.pool, &pkg_name.0, subscribed)
            .await
            .map_err(gql_err)?;
        if subscribed {
            // Re-subscribing is the admin's "I've fixed it, try again" signal, so it
            // clears any breaker trip and its failure count. Without this a subscription
            // auto-disabled after SUBSCRIPTION_FAILURE_LIMIT consecutive failures could
            // never be revived — `subscribed_extensions` skips disabled rows, so the
            // sync loop would keep ignoring it however many times it was toggled.
            catalog::reset_subscription_breaker(&st_arc.pool, &pkg_name.0)
                .await
                .map_err(gql_err)?;
            crate::sync::spawn_extension_sync(st_arc, pkg_name.0.clone());
        }
        Ok(subscribed)
    }

    /// Admin bulk catalogue ingest (EXT-1): for each Suwayomi manga id, ensure
    /// the manga is in the Suwayomi library (so the adaptive scanner tracks it)
    /// and run the Tier-2 dedup add flow (`add_source_series` semantics). A
    /// failing id is recorded in its entry — it never aborts the batch.
    async fn bulk_add_source_series(
        &self,
        ctx: &Context<'_>,
        suwayomi_manga_ids: Vec<ID>,
    ) -> Result<BulkAddResult> {
        require_admin(ctx).await?;
        if suwayomi_manga_ids.is_empty() {
            return Err(Error::new("suwayomiMangaIds must not be empty"));
        }
        if suwayomi_manga_ids.len() > 100 {
            return Err(Error::new("At most 100 ids per bulkAddSourceSeries call"));
        }
        let st = state(ctx);
        let mut entries = Vec::with_capacity(suwayomi_manga_ids.len());
        for id in suwayomi_manga_ids {
            let entry = match ingest_source_series(st, &id.0).await {
                Ok(r) => BulkAddEntry {
                    suwayomi_manga_id: id,
                    result: Some(r),
                    error: None,
                },
                Err(e) => BulkAddEntry {
                    suwayomi_manga_id: id,
                    result: None,
                    error: Some(e.to_string()),
                },
            };
            entries.push(entry);
        }
        Ok(summarize_bulk(entries))
    }
}

/// One id of the bulk-ingest flow: resolve the Suwayomi manga, put it in the
/// Suwayomi library, compute the cover pHash (best-effort, DD1) and run the
/// shared Tier-2 dedup core. Mirrors `add_source_series` plus the library step.
pub(crate) async fn ingest_source_series(
    st: &AppState,
    raw_id: &str,
) -> anyhow::Result<MatchResult> {
    let mid: i64 = raw_id
        .parse()
        .map_err(|_| anyhow::anyhow!("suwayomiMangaId must be an integer id"))?;
    // OPT-6: idempotency pre-check BEFORE any upstream fetch. If this Suwayomi manga is
    // already linked to a work we're done — a re-enrol was previously fetched, put in
    // library, and scanned, so re-running all of that (series fetch, set_in_library,
    // cover download, dhash, immediate scan) is pure waste. The manga id is a global
    // key, so linkage resolves without first fetching the manga to learn its source_id.
    // (The full-fidelity idempotency + concurrent-claim handling still lives in
    // add_source_series_core_ex for the non-short-circuited path.)
    if let Some((ssid, work_id)) =
        crate::catalog::find_source_series_by_key(&st.pool, "suwayomi", &mid.to_string()).await?
    {
        return Ok(MatchResult {
            decision: "existing".into(),
            work_id,
            matched_work_id: None,
            score: None,
            method: None,
            source_series_id: ssid,
        });
    }
    let mut m = st.suwayomi.series(mid).await?;
    st.suwayomi.set_in_library(mid, true).await?;
    m.in_library = true;
    // COVER-POOL COUPLING — read this before parallelising the enrol loop.
    //
    // `cover_bytes` takes a permit from the SHARED bounded cover-fetch pool
    // (`suwayomi::COVER_FETCH_CONCURRENCY`, 12) and WAITS for it, unlike the on-demand
    // reader path which `try_acquire`s and fails fast. This call site is the consumer
    // that module's headroom table does not count: the documented floor of 5 concurrent
    // on-demand reader fetches is `12 - (WARM 3 + BG 4)`, and every permit held here
    // comes out of that floor.
    //
    // It is safe TODAY only because the three enrol paths are strictly sequential — this
    // one is awaited in a `for` loop by `bulk_add_source_series`, and the other two
    // (`add_source_series`, `federated_ingest`) are one call per request — so a request
    // holds at most one permit at a time. Nothing structurally enforces that. Fanning
    // this loop out with `join_all`/`buffer_unordered` would let one admin bulk-add of
    // up to 100 ids take every permit and starve reader cover requests.
    //
    // If it ever goes concurrent, bound it — but NOT by moving to
    // `try_background_slot()`: that pool is 4 permits sized for DETACHED materializations,
    // and putting a foreground admin request behind it would be slower, not faster.
    // Neither should this become a `try_acquire` fail-fast: dropping the pHash under
    // reader load silently degrades the dedup signal, trading a correctness property for
    // latency on an admin action nobody is watching in real time. A separate small
    // semaphore around the fan-out is the shape that works.
    let cover_phash = match st.suwayomi.cover_bytes(m.thumbnail_url.as_deref()).await {
        // dhash is CPU-bound (decode + grayscale + resize); keep it off the async
        // runtime. Best-effort: a task panic just drops the signal.
        Some(bytes) => tokio::task::spawn_blocking(move || crate::phash::dhash(&bytes))
            .await
            .unwrap_or(None),
        None => None,
    };
    let result = add_source_series_core(&st.pool, &m, cover_phash).await?;
    // Populate this series' chapters + chapter count + scan state NOW, so it shows a
    // real chapter count in Browse (and enters the updates feed) immediately instead
    // of reading 0 chapters until the next adaptive scan tick. This makes the bulk
    // "add all from source" ingest scan-on-enrol like the single add
    // (`addSourceSeries`) — one extra chapters fetch per item. Best-effort: a scan
    // hiccup only logs and leaves the scheduler to retry; it never fails the enrol.
    if let Err(e) = scan_series(st, &m, Utc::now()).await {
        // `scan_series` only caches series metadata AFTER its (fallible) chapter fetch,
        // so on a scan hiccup the `suwayomi_series` row (thumbnail_url etc.) never got
        // written — leaving the cover crawl with no thumbnail to materialize until the
        // next scheduler tick. Persist the metadata here as a best-effort fallback so
        // that window is closed (no double on the success path, which OPT-6 dropped).
        let _ = crate::series_cache::put_series(&st.pool, &m).await;
        tracing::warn!(
            series_id = m.id,
            error = %e,
            "immediate scan after bulk enrol failed; cached metadata, will retry scan on next tick"
        );
    }
    Ok(result)
}

/// Minimum normalized-title length for a federated silent consolidation (C2).
/// Below this, a common short title (e.g. "hero", "love") could collide across
/// unrelated series, so even a corroborated match falls back to cautious review.
const FEDERATED_MIN_TITLE_CHARS: usize = 5;

/// C2 guard: a federated direct-link (silent, un-mergeable consolidation) is only
/// safe when a mid-confidence (`Decision::Review`) match is CORROBORATED beyond the
/// bare title. A title-only exact-alias hit scores exactly `dedup::MID` (0.6 =
/// 0.6·1.0 title + 0 corroboration); any cover-pHash / description / author / year
/// signal pushes it strictly above. We additionally require a non-trivial title
/// length so a short common title can't merge two different series. Matches that
/// fail this fall back to the cautious provisional + `merge_candidate` path — never
/// an irreversible merge.
fn federated_consolidate_ok(score: f64, title: &str) -> bool {
    score > crate::dedup::MID + 1e-9
        && crate::catalog::normalize::normalize_title(title)
            .chars()
            .count()
            >= FEDERATED_MIN_TITLE_CHARS
}

/// Extract a MangaDex manga UUID from a Suwayomi `MangaType.url` (e.g.
/// `/manga/<uuid>`, or a `realUrl` like `.../title/<uuid>/slug`). Returns the
/// canonical lowercase UUID when a path segment is a well-formed 8-4-4-4-12 hex
/// UUID, else None. Feeds the exact-identity consolidation in the add flow.
fn mangadex_uuid(url: &str) -> Option<String> {
    url.split(['/', '?', '#'])
        .find(|seg| is_uuid(seg.trim()))
        .map(|seg| seg.trim().to_ascii_lowercase())
}

/// True when `s` is a hyphenated 8-4-4-4-12 hex UUID (case-insensitive).
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    b.iter().enumerate().all(|(i, &ch)| match i {
        8 | 13 | 18 | 23 => ch == b'-',
        _ => ch.is_ascii_hexdigit(),
    })
}

/// Derive a source-level NSFW signal from a Suwayomi manga's genres (CATALOGUE.md
/// §2). Shared by the Tier-2 add flow and the federated persist gate (M1).
fn genre_is_nsfw(genre: &[String]) -> bool {
    genre.iter().any(|g| {
        let g = g.to_ascii_lowercase();
        // Explicit adult genre tags. `mature`/`18+`/`r18` catch adult scanlation
        // sources (e.g. omegascans) that tag content "Mature"/"18+" rather than
        // "Adult"/"Erotica" — the tags the original list missed, which leaked to the
        // home page. Suggestive/ecchi are deliberately NOT here (kept SFW-visible,
        // matching MangaDex "suggestive"); the source-level flag is the backstop.
        [
            "hentai",
            "erotica",
            "smut",
            "pornographic",
            "adult",
            "mature",
            "18+",
            "nsfw",
            "r18",
            "r-18",
        ]
        .iter()
        .any(|k| g.contains(k))
    })
}

/// Select up to `batch` MangaDex-anchored works still needing enrichment
/// (missing metadata OR cover set), oldest first. Shared by the backfill mutation
/// and the X1 scheduler.
///
/// PLAN. `source_series` is the driver and the ORDER BY is on ITS `created_at`, so the
/// whole statement is a bounded index walk that `LIMIT` short-circuits — but only once
/// `idx_source_series_type_created (source_type, created_at)` exists (migration 0058).
/// Before it, the only usable index was `(source_type, source_key)`, whose second column
/// is not `created_at`, so the planner materialized all ~109k matching rows through
/// `USE TEMP B-TREE FOR ORDER BY` to hand back 25. Measured on a copy of production:
/// 738.7 ms -> 0.2 ms, plan `SEARCH ss USING INDEX idx_source_series_type_key` +
/// temp B-tree -> `SEARCH ss USING INDEX idx_source_series_type_created` with no sort.
///
/// The freshness test is an `EXISTS` rather than a `JOIN` purely to PIN that plan: it
/// makes `source_series` structurally the outer loop, so no future stats refresh can
/// reorder the join to drive from `work` and reintroduce the sort. Row-for-row
/// identical to the old JOIN (`source_series.work_id` selects at most one `work`, and
/// `EXISTS` keeps the JOIN's implicit "skip orphan mappings" behaviour); verified equal
/// over the first 500 rows against production data.
///
/// NOTE (selection semantics, unchanged here on purpose): in production
/// `metadata_synced_at` is non-NULL for all 109,241 mangadex-anchored works while
/// `covers_synced_at` is NULL for ALL of them — migration 0021 added that column with no
/// backfill and only the enrichment path (off: `METADATA_BACKFILL` unset) ever writes it.
/// So this predicate currently matches 100% of the catalogue and the drain would
/// re-enrich every already-enriched work. Narrowing it is a behaviour change, not a perf
/// fix, so it is deliberately NOT done here.
async fn works_needing_enrichment(pool: &SqlitePool, batch: i64) -> Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT ss.source_key FROM source_series ss \
         WHERE ss.source_type = 'mangadex' \
           AND EXISTS (SELECT 1 FROM work w WHERE w.id = ss.work_id \
                       AND (w.metadata_synced_at IS NULL OR w.covers_synced_at IS NULL)) \
         ORDER BY ss.created_at ASC LIMIT ?",
    )
    .bind(batch)
    .fetch_all(pool)
    .await
    .map_err(gql_err)
}

/// X1: recurring auto-enrichment. Every `interval_secs` it drains a small batch of
/// un-enriched MangaDex-anchored works (S2 metadata + F2 covers) so newly-ingested
/// works self-enrich without an operator. Shares the MangaDex rate limiter (via
/// `enrich_works`), logs what it did, and does nothing — no thrash — when the
/// backlog is empty. Mirrors `scanner::spawn`. Off unless `METADATA_BACKFILL=on`.
pub fn spawn_metadata_backfill(
    state: std::sync::Arc<AppState>,
    interval_secs: u64,
    batch: i64,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Panic-supervised (crate::task::supervise): a panic in `enrich_works` used to end
    // enrichment silently. The factory re-clones the Arc state + shutdown handle.
    tokio::spawn(crate::task::supervise(
        "metadata-enrichment",
        Duration::from_secs(30),
        shutdown.clone(),
        move || metadata_backfill_loop(state.clone(), interval_secs, batch, shutdown.clone()),
    ));
}

async fn metadata_backfill_loop(
    state: std::sync::Arc<AppState>,
    interval_secs: u64,
    batch: i64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    // Offset the first tick so this loop (when enabled) doesn't fire in the same
    // instant as the others — see scanner::run_loop.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(120),
        Duration::from_secs(interval_secs),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(interval_secs, batch, "metadata auto-enrichment started");
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let ids = match works_needing_enrichment(&state.pool, batch).await {
                    Ok(ids) => ids,
                    Err(e) => { tracing::warn!(error = %e.message, "enrich tick: selection failed"); continue; }
                };
                if ids.is_empty() {
                    tracing::debug!("enrich tick: nothing to enrich");
                    continue;
                }
                match enrich_works(&state, &ids).await {
                    Ok(n) => tracing::info!(selected = ids.len(), refreshed = n, "enrich tick: done"),
                    Err(e) => tracing::warn!(error = %e.message, "enrich tick: enrich_works failed"),
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("metadata auto-enrichment stopping");
                    break;
                }
            }
        }
    }
}

/// Enrich a set of MangaDex-anchored works (S2 metadata + F2 full cover set) and
/// mark them so the backfill cursor advances. Shared by the interactive backfill
/// mutation and the recurring auto-enrichment scheduler (X1). Metadata is fetched
/// batched (100/req); covers are one `/cover` request per work. Every requested id
/// is marked (even if MangaDex returns nothing) so the drain terminates. Returns
/// how many works were upserted.
pub(crate) async fn enrich_works(st: &AppState, ids: &[String]) -> Result<i32> {
    use futures::StreamExt as _;
    // Overlap the per-work /cover fetches: metadata is already batched 100/req, but each
    // work's cover set is a separate round-trip. The shared TokenBucket still caps the
    // MangaDex request rate (5/s), so buffer_unordered only hides RTT — it never raises the
    // upstream load. Kept modest to stay well under the burst budget.
    const COVER_FETCH_CONCURRENCY: usize = 6;
    let mut refreshed = 0i32;
    for chunk in ids.chunks(100) {
        let mangas = st.mangadex.get_manga_by_ids(chunk).await.map_err(gql_err)?;
        // Map to owned upsert inputs synchronously (no network) so the cover-fetch stream
        // owns its items — streaming over `mangas.iter()` borrows would trip an HRTB
        // lifetime error on the async closure.
        let base: Vec<(String, catalog::WorkInput, Option<String>)> = mangas
            .iter()
            .map(|m| {
                let (id, input) = crate::mangadex::to_work_input(m);
                (id, input, crate::mangadex::cover_file_name(m))
            })
            .collect();
        // Fetch every work's full cover set concurrently, yielding upsert-ready inputs.
        let prepared: Vec<(String, catalog::WorkInput)> = futures::stream::iter(base)
            .map(|(id, mut input, primary)| async move {
                // F2: fetch the full per-volume cover set and mark the primary (the one
                // the sweep mirrors on work.cover_file_name). Best-effort — a /cover
                // failure just leaves the sweep's primary cover.
                match st.mangadex.list_covers(&id, 100).await {
                    Ok(fetched) if !fetched.is_empty() => {
                        input.covers =
                            crate::mangadex::covers_from_fetch(fetched, primary.as_deref());
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(manga = %id, error = %e, "enrich: /cover fetch failed")
                    }
                }
                (id, input)
            })
            .buffer_unordered(COVER_FETCH_CONCURRENCY)
            .collect()
            .await;
        // Upserts stay serial — SQLite has a single writer, so concurrency there buys
        // nothing and only contends on the busy-timeout.
        for (id, input) in &prepared {
            match catalog::upsert_work_from_mangadex(&st.pool, id, input).await {
                Ok(_) => refreshed += 1,
                Err(e) => tracing::warn!(manga = %id, error = %e, "enrich: upsert failed"),
            }
        }
        // Advance the cursor past every requested id — including ones MangaDex
        // didn't return — so the drain can't loop (H1/F2).
        //
        // The cost, stated plainly because it is NOT self-healing: an id MangaDex did
        // return but whose record we failed to deserialize (`get_manga_by_ids` drops it
        // loudly — see `mangadex::log_manga_drops`) is marked synced here just the same,
        // so `works_needing_enrichment` never re-offers it. The work row survives; it
        // just never receives S2 metadata or F2 covers. That is deliberately the lesser
        // evil — the alternative re-fetches a permanently-unparseable id on every sweep
        // forever — but it means a parse regression silently costs enrichment coverage
        // rather than announcing itself. Making this self-healing needs a bounded retry
        // (an attempt counter, hence a migration), not simply narrowing this call to the
        // ids actually received.
        catalog::mark_metadata_synced(&st.pool, chunk)
            .await
            .map_err(gql_err)?;
        catalog::mark_covers_synced(&st.pool, chunk)
            .await
            .map_err(gql_err)?;
    }
    Ok(refreshed)
}

/// Federated-search persist (S3): like `ingest_source_series` but consolidating —
/// a mid-confidence title match links straight to the existing work so the same
/// series across extensions resolves to ONE canonical entry.
async fn federated_ingest(st: &AppState, raw_id: &str) -> anyhow::Result<MatchResult> {
    let mid: i64 = raw_id
        .parse()
        .map_err(|_| anyhow::anyhow!("suwayomiMangaId must be an integer id"))?;
    let mut m = st.suwayomi.series(mid).await?;
    st.suwayomi.set_in_library(mid, true).await?;
    m.in_library = true;
    // S1: cache series METADATA (chapters fill on scan / first read — not per item).
    let _ = crate::series_cache::put_series(&st.pool, &m).await;
    // This path deliberately does NOT scan-on-enrol (chapters fill lazily), so register
    // a "due now" scan-state row explicitly — otherwise the DB-driven scanner, which
    // selects work from `series_scan_state`, would never pick this series up.
    let _ = crate::scanner::ensure_pending(&st.pool, &mid.to_string()).await;
    // WAITS on the shared bounded cover-fetch pool, eating into the reader's on-demand
    // headroom. Called once per id from a SEQUENTIAL loop in `federated_search`, so one
    // permit per in-flight federated search — and that loop is the one most exposed to
    // ordinary (rate-limited, opt-in) user traffic rather than admin traffic. See the full
    // accounting on `ingest_source_series` before making it concurrent.
    let cover_phash = match st.suwayomi.cover_bytes(m.thumbnail_url.as_deref()).await {
        // dhash is CPU-bound (decode + grayscale + resize); keep it off the async
        // runtime. Best-effort: a task panic just drops the signal.
        Some(bytes) => tokio::task::spawn_blocking(move || crate::phash::dhash(&bytes))
            .await
            .unwrap_or(None),
        None => None,
    };
    add_source_series_core_ex(&st.pool, &m, cover_phash, true).await
}

/// Create a session row and return its opaque token.
async fn new_session(pool: &SqlitePool, user_id: &str, ttl_secs: i64) -> Result<String> {
    let tok = auth::generate_token();
    // Store only sha256(token) at rest (defense-in-depth): a leaked DB snapshot
    // never yields a replayable bearer token. The plaintext `tok` is returned to
    // the caller unchanged and is what the client presents on later requests
    // (`user_for_token` re-hashes before lookup).
    let token_hash = auth::hash_token(&tok);
    let now = Utc::now();
    let created = now.to_rfc3339();
    let expires = auth::format_ts(now + chrono::Duration::seconds(ttl_secs));
    sqlx::query(
        "INSERT INTO sessions (token, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&token_hash)
    .bind(user_id)
    .bind(&created)
    .bind(&expires)
    .execute(pool)
    .await
    .map_err(gql_err)?;
    // Opportunistic GC: drop rows that have already expired so the table (and its
    // index) don't accumulate dead sessions between event-driven deletes.
    let _ = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(auth::format_ts(now))
        .execute(pool)
        .await;
    Ok(tok)
}

/// Single-flight guard for the catalogue reconcile (process-global; single replica).
static RECONCILE_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Where the NEXT `consolidateExactDuplicates` call resumes its keyset walk of
/// `work_alias` (exclusive `normalized_title` lower bound; `""` = from the start).
///
/// WHY THIS EXISTS. The sweep REFUSES far more clusters than it merges, and a refusal
/// leaves the alias group exactly where it was — still in `work_alias`, still matching
/// `HAVING COUNT(DISTINCT work_id) > 1`. A mutation that always restarted at `""`
/// therefore re-walked the same refusals on every call. Measured against production
/// (9,464 alias groups, 1,154 of them mergeable under `consolidate_gate`): the first
/// 100 groups in `normalized_title ASC` order contain 12 mergeable clusters and 88
/// refusals, so the default-`limit` call merges 12, and every subsequent call re-walks
/// those 88 plus ~12 fresh ones — the merge rate collapses within two or three clicks
/// and the mutation is a permanent no-op long before it reaches the other ~1,140.
///
/// Process-global (not an `AppState` field) for the same reason as `VIEW_LIMITER`: the
/// construction of `AppState` lives in `main.rs`. Losing the cursor on restart is
/// harmless — it only replays work that is already idempotent. `RECONCILE_RUNNING`
/// single-flights the sweep, so two callers can never interleave their cursor updates.
///
/// The post-ingest sweep (`run_post_ingest_dedup_ex`) deliberately does NOT share this:
/// it runs right after a bulk ingest to clean up what THAT ingest just minted, which
/// can be anywhere in the alias space, so it must start from the beginning.
static CONSOLIDATE_CURSOR: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// SQL predicate for a work that still needs reconciling: it has a Suwayomi source,
/// no MangaDex source, and no pending merge_candidate yet.
const RECONCILE_PENDING_WHERE: &str = "EXISTS (SELECT 1 FROM source_series ss \
        WHERE ss.work_id = w.id AND ss.source_type = 'suwayomi') \
     AND NOT EXISTS (SELECT 1 FROM source_series ss \
        WHERE ss.work_id = w.id AND ss.source_type = 'mangadex') \
     AND NOT EXISTS (SELECT 1 FROM merge_candidate mc \
        JOIN source_series ss ON ss.id = mc.source_series_id \
        WHERE ss.work_id = w.id AND mc.status = 'pending')";

/// How many provisional (Suwayomi-only) works still need reconciling against the spine.
async fn pending_reconcile_count(pool: &SqlitePool) -> anyhow::Result<i64> {
    let n: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM work w WHERE {RECONCILE_PENDING_WHERE}"
    ))
    .fetch_one(pool)
    .await?;
    Ok(n)
}

enum ReconcileAction {
    Merged,
    Queued,
    Skipped,
}

/// Reconcile every provisional (Suwayomi-only) work against the spine. Keyset-
/// paginated by work id so one pass terminates even though "no match" works stay
/// selectable (they're simply left past the cursor for this run). Returns
/// `(merged, queued, skipped)`.
pub(crate) async fn reconcile_provisional_works(
    pool: &SqlitePool,
    covers: Option<&SqlitePool>,
) -> anyhow::Result<(i64, i64, i64)> {
    const BATCH: i64 = 200;
    let mut cursor = String::new();
    let (mut merged, mut queued, mut skipped) = (0i64, 0i64, 0i64);
    loop {
        let batch: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT w.id FROM work w WHERE {RECONCILE_PENDING_WHERE} AND w.id > ? \
             ORDER BY w.id ASC LIMIT ?"
        ))
        .bind(&cursor)
        .bind(BATCH)
        .fetch_all(pool)
        .await?;
        if batch.is_empty() {
            break;
        }
        for work_id in &batch {
            cursor.clone_from(work_id);
            match reconcile_one(pool, covers, work_id).await {
                Ok(ReconcileAction::Merged) => merged += 1,
                Ok(ReconcileAction::Queued) => queued += 1,
                Ok(ReconcileAction::Skipped) => skipped += 1,
                Err(e) => {
                    skipped += 1;
                    tracing::warn!(work_id = %work_id, error = %e, "reconcile: work failed");
                }
            }
        }
        // Yield between batches so the concurrent MangaDex sync + cover drainer keep
        // getting the single SQLite writer.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    Ok((merged, queued, skipped))
}

/// Minimum cover-pHash similarity that counts as corroboration for an alias-cluster
/// consolidation. Deliberately the same 0.90 as `dedup::PHASH_CORROBORATION` (which is
/// private to that module) — at most ~6 differing bits on the 64-bit dHash. Cover
/// hashes are OPTIONAL corroboration only: `COVER_PHASH` is off in production and only
/// ~10% of works carry a hash, so this can never be the sole gate.
const CONSOLIDATE_PHASH_CORROBORATION: f64 = 0.90;

/// The OTHER half of `dedup`'s cover rule, which the threshold constant alone does not
/// carry: a near-uniform dHash (almost-all-0 / almost-all-1 bits) has no signal in it.
/// Flat, letterboxed, all-dark and placeholder covers all collapse to such a hash, so
/// two entirely UNRELATED works score 1.00 similarity against each other.
///
/// `dedup::is_discriminative_phash` (private there) guards its 0.90 comparison with
/// exactly this test; copying only the threshold reproduced the number but not the
/// rule, which is the drift this restores. Without it, `year`/`author` both being NULL
/// — the common shape for a Suwayomi-only provisional work — leaves a blank cover as
/// the SOLE corroboration for an irreversible `merge_works`.
///
/// Kept byte-identical to the original: 8 bytes (64-bit dHash), popcount in `12..=52`.
fn consolidate_phash_is_discriminative(hex: &str) -> bool {
    let Ok(bytes) = hex::decode(hex) else {
        return false;
    };
    if bytes.len() != 8 {
        return false;
    }
    let ones: u32 = bytes.iter().map(|b| b.count_ones()).sum();
    (12..=52).contains(&ones)
}

/// Hard ceiling on how many works may share one normalized alias and still be treated
/// as duplicates. An alias shared by three or more works is a GENERIC-TITLE signal, not
/// a duplicate signal: MangaDex alt-titles give every JoJo part the alias
/// `ジョジョの奇妙な冒険`, and "first love" is the alias of 18 unrelated series.
const CONSOLIDATE_MAX_CLUSTER: usize = 2;

/// One work's identity metadata inside an exact-alias cluster — everything the
/// ambiguity gate needs, fetched in the same query as the cluster membership.
#[derive(Clone, sqlx::FromRow)]
struct ConsolidateWork {
    id: String,
    primary_title: Option<String>,
    year: Option<i64>,
    author: Option<String>,
    cover_phash: Option<String>,
}

impl ConsolidateWork {
    /// The snapshot the gate was evaluated against, in the shape `merge_works_checked`
    /// re-validates inside its transaction. Every column the gate reads is here — adding
    /// an input to `consolidate_gate` without adding it to `catalog::WorkIdentity` would
    /// silently re-open the TOCTOU window for that column.
    fn identity(&self) -> catalog::WorkIdentity {
        catalog::WorkIdentity {
            primary_title: self.primary_title.clone(),
            year: self.year,
            author: self.author.clone(),
            cover_phash: self.cover_phash.clone(),
        }
    }
}

/// Did this error come from `merge_works_checked`'s optimistic-concurrency precondition?
/// A stale gate is an ordinary, expected outcome (skip the pair, retry next pass), not a
/// sweep-aborting failure — every other error still propagates.
fn is_precondition_failure(e: &anyhow::Error) -> bool {
    e.to_string().starts_with("merge precondition failed")
}

/// Order-insensitive author-name key (mirrors `dedup::author_key`, private there):
/// lowercased word tokens, sorted, so "Masashi Kishimoto" and "Kishimoto Masashi"
/// fold together.
fn consolidate_author_key(name: &str) -> Vec<String> {
    let mut toks: Vec<String> = name
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    toks.sort();
    toks
}

/// The ambiguity gate for an exact-alias consolidation. `Ok(())` means the pair is
/// safe to fold irreversibly; `Err(reason)` is a short machine-friendly label used in
/// the log line and to route the pair to the review queue instead.
///
/// A shared normalized ALIAS is far too weak to justify `merge_works` (which physically
/// DELETEs the loser): aliases include every MangaDex `altTitle`, so distinct series in
/// one franchise routinely share one. Every condition below must hold:
///
///  1. the shared alias is the PRIMARY title of both works (not merely an alt-title);
///  2. the normalized title is at least `FEDERATED_MIN_TITLE_CHARS` long (same reason
///     `federated_consolidate_ok` requires it — short common titles collide);
///  3. corroboration beyond the title: equal-or-adjacent `year`, OR an equal author
///     key, OR cover-pHash similarity >= `CONSOLIDATE_PHASH_CORROBORATION` between two
///     DISCRIMINATIVE hashes (`consolidate_phash_is_discriminative` — the guard
///     `dedup` applies alongside the same threshold).
///
/// The cluster-size limit (`CONSOLIDATE_MAX_CLUSTER`) is enforced by the caller, which
/// sees the whole cluster.
fn consolidate_gate(
    norm: &str,
    a: &ConsolidateWork,
    b: &ConsolidateWork,
) -> std::result::Result<(), &'static str> {
    if norm.chars().count() < FEDERATED_MIN_TITLE_CHARS {
        return Err("short_title");
    }
    let is_primary = |w: &ConsolidateWork| {
        w.primary_title
            .as_deref()
            .map(|t| crate::catalog::normalize::normalize_title(t) == norm)
            .unwrap_or(false)
    };
    if !is_primary(a) || !is_primary(b) {
        return Err("alt_title_only");
    }
    let year_ok = matches!((a.year, b.year), (Some(x), Some(y)) if (x - y).abs() <= 1);
    let author_ok = match (a.author.as_deref(), b.author.as_deref()) {
        (Some(x), Some(y)) => {
            let kx = consolidate_author_key(x);
            !kx.is_empty() && kx == consolidate_author_key(y)
        }
        _ => false,
    };
    // BOTH hashes must carry signal before their similarity means anything — see
    // `consolidate_phash_is_discriminative`. Checked before the comparison so a pair of
    // blank covers is not even scored.
    let phash_discriminative = a
        .cover_phash
        .as_deref()
        .is_some_and(consolidate_phash_is_discriminative)
        && b.cover_phash
            .as_deref()
            .is_some_and(consolidate_phash_is_discriminative);
    let phash_ok = phash_discriminative
        && crate::catalog::similarity::phash_similarity(
            a.cover_phash.as_deref(),
            b.cover_phash.as_deref(),
        )
        .map(|p| p >= CONSOLIDATE_PHASH_CORROBORATION)
        .unwrap_or(false);
    if year_ok || author_ok || phash_ok {
        Ok(())
    } else {
        Err("uncorroborated")
    }
}

/// What one consolidation pass did. `merged` carries the audit trail — every
/// `(loser_id, survivor_id)` pair that was irreversibly folded.
#[derive(Default)]
struct ConsolidateOutcome {
    merged: Vec<(String, String)>,
    /// Pairs routed to `merge_candidate` for human review instead of merging.
    queued: i64,
    /// Pairs that failed the gate AND could not be queued (the loser has no
    /// `source_series` row to hang a candidate off). Never merged, never dropped
    /// silently — counted and logged.
    unqueueable: i64,
    /// Pairs the gate approved but whose identity changed before the merge could take
    /// its write lock (`merge_works_checked` precondition). Not merged, not queued —
    /// simply re-examined on a later pass against the new values.
    stale: i64,
    /// Alias groups examined in this pass; 0 means the cursor reached the end.
    groups_seen: i64,
    /// Keyset cursor (last `normalized_title` examined) for the next pass.
    cursor: String,
}

/// Consolidate up to `limit` clusters of works that share an exact normalized alias
/// but were minted separately (pre-policy, concurrent ingest, or a UUID-keyed backfill
/// that bypassed the dedup matcher).
///
/// SAFETY: a shared alias alone is NOT evidence of a duplicate — see `consolidate_gate`.
/// Only a 2-work cluster that clears the gate is folded (MangaDex-anchored survivor
/// first, then most sources, then lowest id). Everything else is routed to the
/// `merge_candidate` review queue for a human, never merged and never dropped.
///
/// `limit` bounds the alias groups EXAMINED, so a pass that merges nothing is normal and
/// does not mean the sweep is finished. Keyset-paginated from `cursor` (an exclusive
/// `normalized_title` lower bound): a pass that REFUSES every cluster still advances,
/// instead of re-examining the same first `limit` groups forever.
async fn consolidate_exact_duplicates_from(
    pool: &SqlitePool,
    covers: Option<&SqlitePool>,
    limit: i64,
    cursor: &str,
) -> anyhow::Result<ConsolidateOutcome> {
    let groups: Vec<String> = sqlx::query_scalar(
        "SELECT normalized_title FROM work_alias WHERE normalized_title > ? \
         GROUP BY normalized_title HAVING COUNT(DISTINCT work_id) > 1 \
         ORDER BY normalized_title ASC LIMIT ?",
    )
    .bind(cursor)
    .bind(limit.max(1))
    .fetch_all(pool)
    .await?;
    let mut out = ConsolidateOutcome {
        cursor: cursor.to_string(),
        ..Default::default()
    };
    for norm in groups {
        out.groups_seen += 1;
        out.cursor.clone_from(&norm);
        // Re-query the group's works fresh (a prior group may already have folded
        // some), best survivor first.
        let works: Vec<ConsolidateWork> = sqlx::query_as(
            "SELECT w.id, w.primary_title, w.year, w.author, w.cover_phash FROM work w \
             JOIN work_alias a ON a.work_id = w.id AND a.normalized_title = ? \
             GROUP BY w.id \
             ORDER BY (SELECT COUNT(*) FROM source_series ss \
                       WHERE ss.work_id = w.id AND ss.source_type = 'mangadex') > 0 DESC, \
                      (SELECT COUNT(*) FROM source_series ss WHERE ss.work_id = w.id) DESC, \
                      w.id ASC",
        )
        .bind(&norm)
        .fetch_all(pool)
        .await?;
        if works.len() < 2 {
            continue;
        }
        let survivor = works[0].clone();
        // A cluster wider than a pair is a generic-title signal: refuse the WHOLE
        // cluster (never "merge the first two"), and queue each member for review.
        let oversized = works.len() > CONSOLIDATE_MAX_CLUSTER;
        for other in &works[1..] {
            let verdict = if oversized {
                Err("cluster_too_large")
            } else {
                consolidate_gate(&norm, &survivor, other)
            };
            match verdict {
                Ok(()) => {
                    // The gate judged a snapshot read OUTSIDE any transaction, and
                    // `RECONCILE_RUNNING` single-flights this sweep only against itself
                    // — never against `updateSeriesMetadata`. Hand the exact values the
                    // gate approved to the merge as a precondition it re-checks under its
                    // own write lock, so an admin retitling a work (or clearing the
                    // `year` that was the sole corroboration) in that window aborts the
                    // merge instead of silently destroying a work on stale grounds. The
                    // pair is simply re-examined next pass, against the new values.
                    let expect_loser = other.identity();
                    let expect_survivor = survivor.identity();
                    match catalog::merge_works_checked(
                        pool,
                        covers,
                        &other.id,
                        &survivor.id,
                        Some((&expect_loser, &expect_survivor)),
                    )
                    .await
                    {
                        Ok(_) => {
                            tracing::info!(
                                loser = %other.id, survivor = %survivor.id, alias = %norm,
                                "consolidate: merged"
                            );
                            out.merged.push((other.id.clone(), survivor.id.clone()));
                        }
                        Err(e) if is_precondition_failure(&e) => {
                            out.stale += 1;
                            tracing::info!(
                                loser = %other.id, survivor = %survivor.id, alias = %norm,
                                "consolidate: skipped — identity changed under the gate"
                            );
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(reason) => {
                    match queue_consolidate_review(pool, &other.id, &survivor.id).await? {
                        true => out.queued += 1,
                        false => out.unqueueable += 1,
                    }
                    tracing::debug!(
                        loser = %other.id, survivor = %survivor.id, alias = %norm, reason,
                        "consolidate: refused (routed to review)"
                    );
                }
            }
        }
    }
    Ok(out)
}

/// Enqueue a refused alias pair for human review. Returns `Ok(true)` when a candidate
/// row now exists (freshly inserted or already there), `Ok(false)` when `loser` has no
/// `source_series` row to hang a candidate off.
///
/// Idempotence and the never-resurrect-a-decision rule are NOT implemented here: they
/// live in `catalog::insert_merge_candidate`, which suppresses any pair that already has
/// a candidate row in any status, atomically. This function used to carry its own
/// existence probe, which protected only the consolidation path while the other two
/// writers (`reconcile_one`, `add_source_series_core_ex`) still appended blindly — and
/// it is `reconcile_one` that produced all five duplicate pairs found in production.
/// One guard in the shared writer, not one guard per caller.
async fn queue_consolidate_review(
    pool: &SqlitePool,
    loser: &str,
    survivor: &str,
) -> anyhow::Result<bool> {
    let ssid: Option<String> = sqlx::query_scalar(
        "SELECT id FROM source_series WHERE work_id = ? ORDER BY created_at ASC, id ASC LIMIT 1",
    )
    .bind(loser)
    .fetch_optional(pool)
    .await?;
    let Some(ssid) = ssid else {
        return Ok(false);
    };
    // A bare exact-alias hit scores exactly `dedup::MID` (title only, no corroboration)
    // — the same score the reconcile path records for this band. A `None` return means
    // the pair is already in the queue or already resolved; either way a row exists.
    catalog::insert_merge_candidate(pool, &ssid, survivor, crate::dedup::MID, "title_exact")
        .await?;
    Ok(true)
}

/// Post-ingest dedup sweep: fold Suwayomi-only provisionals into the MangaDex spine
/// (exact + fuzzy, over aliases — which include every MangaDex `altTitle`), then
/// consolidate any remaining exact-alias duplicate clusters until none are left.
/// Meant to run right after a bulk ingest that keys only on the provider UUID (the
/// one-time catalogue backfill), which can otherwise mint a second canonical work for
/// a series the catalogue already had. Single-flighted against the admin
/// `reconcileCatalogue` mutation: if a reconcile is already in flight this returns
/// cleanly without doing anything.
///
/// COVER BLOBS: pass `Some(covers)` so the merges reclaim the losing works' cached
/// cover blobs (see `catalog::merge_works_ex`). Passing `None` leaks them, and is only
/// correct where no covers pool exists.
pub(crate) async fn run_post_ingest_dedup_ex(pool: &SqlitePool, covers: Option<&SqlitePool>) {
    if RECONCILE_RUNNING
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        tracing::info!("post-ingest dedup: a reconcile is already running; skipping");
        return;
    }
    tracing::info!("post-ingest dedup: starting");
    match reconcile_provisional_works(pool, covers).await {
        Ok((merged, queued, skipped)) => {
            tracing::info!(merged, queued, skipped, "post-ingest dedup: reconcile done")
        }
        Err(e) => tracing::error!(error = %e, "post-ingest dedup: reconcile failed"),
    }
    // BOUNDED. This used to be `loop { … }` until nothing merged, which — with the old
    // merge-on-any-shared-alias rule — would have folded 11,580 works across 9,464
    // clusters into each other on a single restart, irreversibly. The gate in
    // `consolidate_gate` is the real fix; this cap is the belt-and-braces one: even a
    // future gate regression can only touch CONSOLIDATE_MAX_BATCHES × 200 alias groups
    // per run, and stopping early is logged loudly.
    const CONSOLIDATE_MAX_BATCHES: usize = 10;
    const CONSOLIDATE_BATCH: i64 = 200;
    let mut consolidated = 0i64;
    let (mut queued, mut unqueueable, mut stale) = (0i64, 0i64, 0i64);
    let mut cursor = String::new();
    let mut exhausted = false;
    for _ in 0..CONSOLIDATE_MAX_BATCHES {
        match consolidate_exact_duplicates_from(pool, covers, CONSOLIDATE_BATCH, &cursor).await {
            Ok(out) => {
                consolidated += out.merged.len() as i64;
                queued += out.queued;
                unqueueable += out.unqueueable;
                stale += out.stale;
                for (loser, survivor) in &out.merged {
                    tracing::info!(%loser, %survivor, "post-ingest dedup: consolidated work");
                }
                if out.groups_seen == 0 {
                    exhausted = true;
                    break;
                }
                cursor = out.cursor;
                // Yield between batches so the live sync + cover drainer keep getting
                // the single SQLite writer.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
            Err(e) => {
                tracing::error!(error = %e, "post-ingest dedup: consolidate failed");
                break;
            }
        }
    }
    if !exhausted {
        tracing::warn!(
            cursor = %cursor,
            max_batches = CONSOLIDATE_MAX_BATCHES,
            "post-ingest dedup: consolidate stopped at the batch cap; alias groups remain \
             unexamined (they will be picked up by a later run or the admin mutation)"
        );
    }
    tracing::info!(
        consolidated,
        queued,
        unqueueable,
        stale,
        "post-ingest dedup: complete"
    );
    RECONCILE_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// Reconcile one provisional work: build a candidate from its own metadata, match it
/// against the spine (excluding itself), then merge / queue / skip.
async fn reconcile_one(
    pool: &SqlitePool,
    covers: Option<&SqlitePool>,
    work_id: &str,
) -> anyhow::Result<ReconcileAction> {
    let Some(md) = catalog::load_match_data(pool, work_id).await? else {
        return Ok(ReconcileAction::Skipped); // vanished (e.g. merged by a prior item)
    };
    let title = md
        .primary_title
        .clone()
        .or_else(|| md.aliases_norm.first().cloned())
        .unwrap_or_default();
    if title.is_empty() && md.aliases_norm.is_empty() {
        return Ok(ReconcileAction::Skipped);
    }
    let cand = crate::dedup::Candidate {
        title,
        alt_titles: md.aliases_norm.clone(),
        description: md.description.clone(),
        author: md.author.clone(),
        year: md.year,
        cover_phash: md.cover_phash.clone(),
        external_ids: Vec::new(),
    };
    match crate::dedup::resolve_ex(pool, &cand, Some(work_id)).await? {
        crate::dedup::Decision::AutoMerge {
            work_id: target, ..
        } => {
            catalog::merge_works_ex(pool, covers, work_id, &target).await?;
            Ok(ReconcileAction::Merged)
        }
        crate::dedup::Decision::Review {
            work_id: target,
            score,
            method,
        } => {
            let ssid: Option<String> = sqlx::query_scalar(
                "SELECT id FROM source_series WHERE work_id = ? AND source_type = 'suwayomi' \
                 ORDER BY created_at ASC, id ASC LIMIT 1",
            )
            .bind(work_id)
            .fetch_optional(pool)
            .await?;
            match ssid {
                // A `None` from the insert is a pair an admin already ruled on (or one
                // already queued): counting it as `Queued` would report review work that
                // does not exist. `Skipped` is the honest bucket.
                Some(ssid) => {
                    match catalog::insert_merge_candidate(pool, &ssid, &target, score, &method)
                        .await?
                    {
                        Some(_) => Ok(ReconcileAction::Queued),
                        None => {
                            tracing::debug!(
                                work_id, %target,
                                "reconcile: pair already queued or already resolved — not re-proposed"
                            );
                            Ok(ReconcileAction::Skipped)
                        }
                    }
                }
                None => Ok(ReconcileAction::Skipped),
            }
        }
        crate::dedup::Decision::New => Ok(ReconcileAction::Skipped),
    }
}

/// Core of the Tier-2 "add source series" flow (DD2/DD1/N1). Separated from the
/// resolver's Suwayomi fetch so it's unit-testable without a live Suwayomi:
/// idempotency pre-check, dedup, provisional work creation, source-series link,
/// and review-queue insert. `cover_phash` is the already-computed cover hash
/// (None when unavailable).
async fn add_source_series_core(
    pool: &SqlitePool,
    m: &crate::suwayomi::SuwayomiManga,
    cover_phash: Option<String>,
) -> anyhow::Result<MatchResult> {
    add_source_series_core_ex(pool, m, cover_phash, false).await
}

/// As `add_source_series_core`, but `consolidate` controls the mid-confidence
/// (Review-band) branch. When false (the Tier-2 admin add flow): a Review match
/// mints a PROVISIONAL work + a `merge_candidate` for a human to confirm — cautious
/// by design. When true (federated search, S3): a Review match links DIRECTLY to
/// the matched work so the same series across extensions consolidates under ONE
/// canonical entry in search results, as the reader UX requires. Federated
/// consolidation is optimistic and does NOT enqueue a review row (that would
/// re-split the very entry the feature is meant to unify).
async fn add_source_series_core_ex(
    pool: &SqlitePool,
    m: &crate::suwayomi::SuwayomiManga,
    cover_phash: Option<String>,
    consolidate: bool,
) -> anyhow::Result<MatchResult> {
    let source_key = m.id.to_string();

    // DD2 idempotency: if this source series is already linked to a work, return
    // that linkage untouched instead of re-running the matcher (which would mint an
    // orphan work and, for a review, a duplicate merge_candidate row).
    if let Some((ssid, existing_work_id)) =
        crate::catalog::find_source_series(pool, "suwayomi", &m.source_id, &source_key).await?
    {
        return Ok(MatchResult {
            decision: "existing".into(),
            work_id: existing_work_id,
            matched_work_id: None,
            score: None,
            method: None,
            source_series_id: ssid,
        });
    }

    // N1/N5: NSFW if the series' own genres look adult OR its Suwayomi source is an
    // adult source. The source flag (source_extension.is_nsfw, cached by the scan
    // tick — a cheap PK lookup, no network) is the backstop for adult sources whose
    // per-series tags don't include an explicit adult genre: that gap ingested whole
    // adult sources (e.g. omegascans) as SFW and leaked them to the home page.
    // CATALOGUE.md §2.
    let source_ext_nsfw =
        sqlx::query_scalar::<_, i64>("SELECT is_nsfw FROM source_extension WHERE source_id = ?")
            .bind(&m.source_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or(0)
            != 0;
    let source_nsfw = genre_is_nsfw(&m.genre) || source_ext_nsfw;

    // Exact MangaDex-identity consolidation. The MangaDex Suwayomi extension carries
    // the canonical MangaDex UUID in `MangaType.url` (e.g. `/manga/<uuid>`). When that
    // UUID resolves to an existing `mangadex`-anchored catalogue work, this mirror IS
    // that same work — link it DIRECTLY by exact id instead of leaning on fuzzy
    // title/cover dedup, which was leaving ~1 in 5 MangaDex-extension series as
    // un-catalogued standalone works (present in Browse but invisible to search / home
    // / canonical surfaces, which are built only from `mangadex`-anchored works). The
    // existence of a canonical work for that exact UUID IS the gate — no dependency on
    // `source_extension` being populated yet (it isn't at first boot), and no risk of a
    // false link: a non-MangaDex source's url practically never contains a UUID, and a
    // UUID that matches one of our ~109k MangaDex ids can only be that MangaDex series.
    // A UUID hit is authoritative, so no merge_candidate review is enqueued.
    if let Some(uuid) = m.url.as_deref().and_then(mangadex_uuid) {
        if let Some((_, work_id)) =
            crate::catalog::find_source_series(pool, "mangadex", "mangadex", &uuid).await?
        {
            let ssid = crate::catalog::upsert_source_series(
                pool,
                &work_id,
                "suwayomi",
                &m.source_id,
                &source_key,
                None,
                source_nsfw,
            )
            .await?;
            if source_nsfw {
                crate::catalog::mark_work_nsfw(pool, &work_id).await?;
                // Push the escalation into the materialized feed copies immediately.
                // Without this the flag reaches `work` but not `browse_catalogue`, and
                // Browse gates on the copy — so an opted-out viewer keeps being served
                // this work until the next rebuild (one catalogue-sync interval, 6h by
                // default; boot-only if the sync is disabled). The admin mark paths have
                // always done this; the ingest/link path did not.
                resync_feed_nsfw(pool, std::slice::from_ref(&work_id)).await;
            }
            // Adopt the AUTHORITATIVE stored linkage (H6): if a concurrent add via the
            // fuzzy path won the natural-key claim and linked this series to a different
            // work, `upsert_source_series`'s `ON CONFLICT` kept that work_id — return it
            // rather than our own, so the reported linkage never disagrees with the DB.
            let linked =
                crate::catalog::find_source_series(pool, "suwayomi", &m.source_id, &source_key)
                    .await?
                    .map(|(_, w)| w)
                    .unwrap_or(work_id);
            return Ok(MatchResult {
                decision: "mangadex_id".into(),
                work_id: linked,
                matched_work_id: None,
                score: None,
                method: Some("mangadex_id".into()),
                source_series_id: ssid,
            });
        }
    }

    // Suwayomi carries no external tracker IDs (no AniList/MAL on MangaType), so
    // `external_ids` stays empty — the external-ID dedup rung is a no-op here.
    let cand = crate::dedup::Candidate {
        title: m.title.clone(),
        alt_titles: Vec::new(),
        description: m.description.clone(),
        author: m.author.clone(),
        year: None,
        cover_phash: cover_phash.clone(),
        external_ids: Vec::new(),
    };
    let decision = crate::dedup::resolve(pool, &cand).await?;

    // The work this source series is (provisionally) linked to. For `new` and
    // `review` we mint a first-class work from the Suwayomi metadata.
    let make_work = || crate::catalog::WorkInput {
        primary_title: Some(m.title.clone()),
        description: m.description.clone(),
        author: m.author.clone(),
        artist: m.artist.clone(),
        status: Some(m.status.clone()),
        is_nsfw: source_nsfw,
        cover_phash: cover_phash.clone(),
        aliases: vec![crate::catalog::Alias {
            raw: m.title.clone(),
            lang: None,
        }],
        ..Default::default()
    };

    use crate::dedup::Decision;
    // `minted_work` is the work we FRESHLY created in this call (New / provisional
    // Review). It's the orphan-cleanup target if we lose the concurrent claim below
    // — AutoMerge / review_consolidated reuse an existing work and must never be
    // deleted here.
    let mut minted_work: Option<String> = None;
    let (mut decision_str, matched_work_id, score, method, mut work_id) = match &decision {
        Decision::AutoMerge {
            work_id,
            score,
            method,
        } => (
            "auto_merge",
            Some(work_id.clone()),
            Some(*score),
            Some(method.clone()),
            work_id.clone(),
        ),
        // Federated (S3/C2): consolidate onto the matched work ONLY when the match
        // is corroborated beyond the bare title and the title is non-trivial —
        // otherwise this falls through to the cautious provisional arm below.
        Decision::Review {
            work_id,
            score,
            method,
        } if consolidate && federated_consolidate_ok(*score, &m.title) => (
            "review_consolidated",
            Some(work_id.clone()),
            Some(*score),
            Some(method.clone()),
            work_id.clone(),
        ),
        Decision::Review {
            work_id,
            score,
            method,
        } => {
            // Cautious path: a provisional work + (below) a merge_candidate for a
            // human. Reached by the admin add flow AND by federated matches that
            // failed the C2 corroboration/length guard — never a silent merge.
            let provisional = crate::catalog::create_work(pool, &make_work()).await?;
            minted_work = Some(provisional.clone());
            (
                "review",
                Some(work_id.clone()),
                Some(*score),
                Some(method.clone()),
                provisional,
            )
        }
        Decision::New => {
            let created = crate::catalog::create_work(pool, &make_work()).await?;
            minted_work = Some(created.clone());
            ("new", None, None, None, created)
        }
    };

    let ssid = crate::catalog::upsert_source_series(
        pool,
        &work_id,
        "suwayomi",
        &m.source_id,
        &source_key,
        None,
        source_nsfw,
    )
    .await?;

    // H6 — post-claim re-check. The natural-key `ON CONFLICT` in
    // `upsert_source_series` is the atomic claim: exactly one work_id ends up
    // stored for this (source_type, source_id, source_key). If a concurrent add of
    // the SAME series won the claim, the stored work_id differs from the one we
    // just linked. Adopt the authoritative stored linkage and, if WE minted a fresh
    // work, delete it so it isn't orphaned. This closes the false-split / orphan
    // race without a cross-helper transaction refactor.
    // Residual risk: two concurrent *New* decisions for what is genuinely the same
    // series (both saw no existing work) still mint two works; the claim serializes
    // them so only one is linked and the loser's work is reclaimed here — but a
    // concurrent add matching a DIFFERENT existing work than the winner would keep
    // its own (non-minted) work. That window is narrow and human-reviewable.
    if let Some((_, stored_work_id)) =
        crate::catalog::find_source_series(pool, "suwayomi", &m.source_id, &source_key).await?
    {
        if stored_work_id != work_id {
            if minted_work.as_deref() == Some(work_id.as_str()) {
                crate::catalog::delete_work_cascade(pool, &work_id).await?;
            }
            // The concurrent winner already established (and, if applicable, queued
            // a review for) the canonical linkage — treat this add as idempotent.
            work_id = stored_work_id;
            decision_str = "existing";
        }
    }

    // N4: the source-level NSFW signal must reach `work.is_nsfw` — the only column the
    // gating reads consult. `new`/`review` already mint the work with it via make_work;
    // an `auto_merge` reuses an existing (possibly SFW) work, so OR the flag in there.
    if source_nsfw {
        crate::catalog::mark_work_nsfw(pool, &work_id).await?;
        // Same reason as the other `mark_work_nsfw` site: `work` is the authoritative
        // column, but Browse and the feeds gate on a materialized copy, so the escalation
        // has to be pushed there in the same breath or it is invisible to the gate for up
        // to a full sync interval.
        resync_feed_nsfw(pool, std::slice::from_ref(&work_id)).await;
    }

    // Enqueue a review row whenever we took the PROVISIONAL review path (decision
    // "review") — the admin add flow, and the federated fallback for an
    // uncorroborated match. A directly-consolidated federated match
    // ("review_consolidated") already linked to the matched work, so a candidate
    // would ask a human to re-split the very entry we consolidated.
    if decision_str == "review" {
        if let Decision::Review {
            work_id: cand_work,
            score,
            method,
        } = &decision
        {
            crate::catalog::insert_merge_candidate(pool, &ssid, cand_work, *score, method).await?;
        }
    }

    Ok(MatchResult {
        decision: decision_str.to_string(),
        work_id,
        matched_work_id,
        score,
        method,
        source_series_id: ssid,
    })
}

/// Ensure every configured admin username exists and is an admin. An existing
/// account is promoted (never re-passworded); a missing one is CREATED from
/// `admin_password` (with `is_admin = 1`). When no password is configured, a
/// missing admin can only be logged — it cannot self-register, since admin names
/// are reserved from open registration (A5). This is the sole path to admin
/// status: `register` never grants it.
pub async fn provision_admins(
    pool: &SqlitePool,
    admin_users: &[String],
    admin_password: Option<&str>,
    admin_email: Option<&str>,
) -> anyhow::Result<()> {
    for (idx, username) in admin_users.iter().enumerate() {
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT id FROM users WHERE username = ? COLLATE NOCASE")
                .bind(username)
                .fetch_optional(pool)
                .await?;
        if let Some((id,)) = existing {
            sqlx::query("UPDATE users SET is_admin = 1 WHERE id = ?")
                .bind(&id)
                .execute(pool)
                .await?;
            tracing::info!(username, "ensured admin (promoted existing account)");
            continue;
        }
        let Some(pw) = admin_password else {
            tracing::warn!(
                username,
                "configured admin user is missing and KOMIKA_ADMIN_PASSWORD is unset — \
                 cannot provision it, and the reserved name cannot self-register"
            );
            continue;
        };
        let hash = auth::hash_password(pw)?;
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        // The primary admin gets the configured email; any others get a
        // collision-free synthetic address (email is UNIQUE).
        let email = match (idx, admin_email) {
            (0, Some(e)) => e.to_string(),
            _ => format!("{username}@admin.local"),
        };
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, avatar_url, is_admin, created_at) \
             VALUES (?, ?, ?, ?, NULL, 1, ?)",
        )
        .bind(&id)
        .bind(username)
        .bind(&email)
        .bind(&hash)
        .bind(&now)
        .execute(pool)
        .await?;
        tracing::info!(
            username,
            "provisioned admin account from KOMIKA_ADMIN_PASSWORD"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    // ---- MangaDex UUID extraction (exact-identity consolidation) ----

    #[test]
    fn mangadex_uuid_extracts_from_source_urls() {
        // Suwayomi `MangaType.url` form.
        assert_eq!(
            mangadex_uuid("/manga/a77742b1-befd-49a4-bff5-1ad4e6b0ef7b").as_deref(),
            Some("a77742b1-befd-49a4-bff5-1ad4e6b0ef7b")
        );
        // `realUrl` form with a trailing slug — the UUID segment is still found.
        assert_eq!(
            mangadex_uuid(
                "https://mangadex.org/title/A77742B1-BEFD-49A4-BFF5-1AD4E6B0EF7B/chainsaw-man"
            )
            .as_deref(),
            Some("a77742b1-befd-49a4-bff5-1ad4e6b0ef7b"),
            "uppercase UUID is canonicalized to lowercase"
        );
        // No UUID → None (a slug-only or numeric source url must not false-match).
        assert!(mangadex_uuid("/series/chainsaw-man").is_none());
        assert!(mangadex_uuid("/manga/12345").is_none());
        // A 36-char non-hex string of the right shape is rejected.
        assert!(mangadex_uuid("/manga/zzzzzzzz-befd-49a4-bff5-1ad4e6b0ef7b").is_none());
    }

    // RateLimiter unit tests moved to crate::rate_limit alongside the sharded impl.

    // ---- viewer-invariant feed memo ----

    /// The feed memos are keyed on the NSFW posture ALONE. This pins the three
    /// properties the NSFW gate depends on: a stored entry comes back only within its
    /// TTL, only under the key it was stored under (an opted-out viewer must never be
    /// served the opted-in payload or vice versa), and a fresh store replaces an
    /// expired one. Uses a LOCAL map — the process-wide statics stay untouched, so
    /// this can't leak into the end-to-end resolver tests regardless of scheduling.
    #[test]
    fn feed_memo_is_key_isolated_and_ttl_bounded() {
        let cache: FeedCache<Vec<u8>> = std::sync::Mutex::new(std::collections::HashMap::new());
        let long = Duration::from_secs(60);
        let zero = Duration::from_secs(0);

        store_feed(&cache, false, vec![1u8]);
        // hit under its own key within TTL…
        assert_eq!(
            cached_feed(&cache, false, long),
            Some(std::sync::Arc::new(vec![1u8])),
            "a fresh entry is served"
        );
        // …and NEVER under the other posture — the property that keeps an
        // opted-out viewer from receiving an opted-in computation.
        assert!(
            cached_feed(&cache, true, long).is_none(),
            "the other NSFW posture must miss"
        );

        // expired → recomputed; the replacement lands under the SAME single key.
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(cached_feed(&cache, false, zero).is_none(), "TTL expired");
        store_feed(&cache, true, vec![2u8]);
        assert_eq!(
            cached_feed(&cache, true, long),
            Some(std::sync::Arc::new(vec![2u8]))
        );
        // Overwriting one posture leaves the sibling payload untouched.
        store_feed(&cache, false, vec![9u8]);
        assert_eq!(
            cached_feed(&cache, false, long),
            Some(std::sync::Arc::new(vec![9u8])),
            "a fresh store replaces the entry"
        );
        assert_eq!(
            cached_feed(&cache, true, long),
            Some(std::sync::Arc::new(vec![2u8])),
            "the other NSFW posture's entry is unaffected"
        );
    }

    /// The admin invalidation really drops a live memo entry, for BOTH posture keys.
    /// Nothing end-to-end can catch a regression here: [`FEED_TTL`] is zero under
    /// `cfg(test)`, so no resolver ever reads the memo — the only way to observe a live
    /// hit at all is to pass an explicit non-zero TTL, which is what this does.
    ///
    /// Unlike [`feed_memo_is_key_isolated_and_ttl_bounded`] this MUST use the real
    /// statics, because that is what `clear_feed_caches` reaches for. Two consequences,
    /// both deliberate: nothing this test writes can be served to another test (every
    /// resolver reads with the zero `FEED_TTL` and therefore always misses), and nothing
    /// another test writes can fail this one — `DISCOVERY_CACHE` has no other writer in
    /// the suite, and `GENRE_FACETS_CACHE` does (the `genreFacets` resolver test), so
    /// that half asserts the SENTINEL is gone rather than that the map is empty.
    #[test]
    fn clear_feed_caches_drops_a_live_memo_entry() {
        let long = Duration::from_secs(60);
        let sentinel = vec![("__clear_feed_caches_probe__".to_string(), 1i64)];

        for key in [feed_cache_key(false), feed_cache_key(true)] {
            store_feed(&DISCOVERY_CACHE, key, Vec::<DiscoveryFeed>::new());
            store_feed(&GENRE_FACETS_CACHE, key, sentinel.clone());
            assert!(
                cached_feed(&DISCOVERY_CACHE, key, long).is_some(),
                "the memo holds a live entry before the bust (key {key})"
            );
            assert!(
                cached_feed(&GENRE_FACETS_CACHE, key, long).is_some(),
                "the memo holds a live entry before the bust (key {key})"
            );
        }

        clear_feed_caches();

        for key in [feed_cache_key(false), feed_cache_key(true)] {
            assert!(
                cached_feed(&DISCOVERY_CACHE, key, long).is_none(),
                "the home rails recompute after an admin edit (key {key})"
            );
            assert_ne!(
                cached_feed(&GENRE_FACETS_CACHE, key, long).as_deref(),
                Some(&sentinel),
                "the genre chips recompute after an admin edit (key {key})"
            );
        }
    }

    // ---- Tier-2 add-source-series core (DD2/DD1/N1) -----------------------

    fn suwayomi_manga(
        id: i64,
        title: &str,
        genre: &[&str],
        source_id: &str,
    ) -> crate::suwayomi::SuwayomiManga {
        crate::suwayomi::SuwayomiManga {
            id,
            title: title.into(),
            url: None,
            thumbnail_url: None,
            author: None,
            artist: None,
            description: None,
            genre: genre.iter().map(|g| g.to_string()).collect(),
            status: "ONGOING".into(),
            in_library: false,
            in_library_at: None,
            last_fetched_at: None,
            latest_chapter_at: None,
            source_id: source_id.into(),
            source: None,
            chapters: None,
        }
    }

    async fn migrated_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn add_source_series_new_sets_nsfw_and_is_idempotent() {
        let pool = migrated_pool().await;
        // A unique-title, NSFW-genre series → decision "new", work.is_nsfw = 1 (N1).
        let m = suwayomi_manga(42, "A Very Spicy One-Off", &["Action", "Hentai"], "src1");
        let r1 = add_source_series_core(&pool, &m, None).await.unwrap();
        assert_eq!(r1.decision, "new");
        let nsfw: i64 = sqlx::query_scalar("SELECT is_nsfw FROM work WHERE id = ?")
            .bind(&r1.work_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(nsfw, 1, "N1: an NSFW genre sets work.is_nsfw");

        let works_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        // DD2: re-adding the same source id returns the existing linkage and mints
        // nothing new.
        let r2 = add_source_series_core(&pool, &m, None).await.unwrap();
        assert_eq!(r2.decision, "existing");
        assert_eq!(r2.work_id, r1.work_id);
        assert_eq!(r2.source_series_id, r1.source_series_id);
        let works_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(works_after, works_before, "DD2: no orphan work on re-add");
    }

    #[tokio::test]
    async fn opt6_relinked_series_short_circuits_before_upstream() {
        // OPT-6: ingest_source_series pre-checks linkage by manga id ALONE (no
        // source_id) and returns early before any upstream fetch. SuwayomiClient is a
        // concrete HTTP client (not unit-mockable), so we assert the predicate that
        // gates that early return: once a series is linked, find_source_series_by_key
        // resolves its (ssid, work_id) by key only — which is exactly what lets the
        // enrol path answer "existing" without fetching the manga to learn its
        // source_id. An unlinked id resolves to None, so a genuinely-new enrol still
        // proceeds to the full fetch+dedup path.
        let pool = migrated_pool().await;
        let m = suwayomi_manga(4242, "Only Enrolled Once", &["Action"], "srcX");

        // Not linked yet → None → enrol would proceed upstream.
        assert!(
            crate::catalog::find_source_series_by_key(&pool, "suwayomi", "4242")
                .await
                .unwrap()
                .is_none()
        );

        let r1 = add_source_series_core(&pool, &m, None).await.unwrap();
        assert_eq!(r1.decision, "new");

        // Now linked → resolvable by key alone (no source_id), matching the linkage the
        // add produced → the enrol short-circuit fires with decision "existing".
        let hit = crate::catalog::find_source_series_by_key(&pool, "suwayomi", "4242")
            .await
            .unwrap()
            .expect("linked series resolves by key alone");
        assert_eq!(hit.0, r1.source_series_id);
        assert_eq!(hit.1, r1.work_id);

        // A different (unlinked) manga id stays None — no false short-circuit.
        assert!(
            crate::catalog::find_source_series_by_key(&pool, "suwayomi", "9999")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn add_source_series_review_does_not_duplicate_on_re_add() {
        let pool = migrated_pool().await;
        // Pre-seed a work with a title so a same-titled add auto-merges into it under
        // the exact-title policy (exact normalized-title hit).
        let existing = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Twin Star Exorcists".into()),
                aliases: vec![crate::catalog::Alias {
                    raw: "Twin Star Exorcists".into(),
                    lang: None,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let m = suwayomi_manga(7, "Twin Star Exorcists", &["Action"], "src1");
        let r1 = add_source_series_core(&pool, &m, None).await.unwrap();
        assert_eq!(r1.decision, "auto_merge", "exact title auto-merges");
        assert_eq!(r1.matched_work_id.as_deref(), Some(existing.as_str()));

        let works_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        let mc_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mc_before, 0, "exact-title match auto-merged; no review row");

        // DD2: re-add → existing; neither an orphan work nor a merge_candidate is
        // created.
        let r2 = add_source_series_core(&pool, &m, None).await.unwrap();
        assert_eq!(r2.decision, "existing");
        let works_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        let mc_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(works_after, works_before, "DD2: no orphan work on re-add");
        assert_eq!(mc_after, mc_before, "DD2: no duplicate merge_candidate");
    }

    #[test]
    fn federated_consolidate_ok_requires_corroboration_and_title_length() {
        use crate::dedup::MID;
        // A bare title-only exact match scores exactly MID → never consolidates.
        assert!(!federated_consolidate_ok(MID, "Twin Star Exorcists"));
        // Any corroboration pushes score above MID → consolidates (long title).
        assert!(federated_consolidate_ok(MID + 0.05, "Twin Star Exorcists"));
        // Even corroborated, a too-short/common title is refused.
        assert!(!federated_consolidate_ok(0.9, "ao"));
        assert!(!federated_consolidate_ok(0.9, "hero")); // 4 chars < 5
        assert!(federated_consolidate_ok(0.9, "naruto"));
    }

    #[tokio::test]
    async fn federated_corroborated_match_consolidates_to_existing_work() {
        // C2: a mid-confidence match CORROBORATED beyond the title (here a shared
        // description) links DIRECTLY to the existing work — one work, no review row.
        let pool = migrated_pool().await;
        let blurb = "Twin exorcists destined to marry and birth the Miko.";
        let existing = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Twin Star Exorcists".into()),
                description: Some(blurb.into()),
                aliases: vec![crate::catalog::Alias {
                    raw: "Twin Star Exorcists".into(),
                    lang: None,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let mut m = suwayomi_manga(7, "Twin Star Exorcists", &["Action"], "src-ext2");
        m.description = Some(blurb.into()); // corroborating description
        let r = add_source_series_core_ex(&pool, &m, None, true)
            .await
            .unwrap();
        // Exact normalized-title hit → auto-merges outright (the exact-title policy
        // supersedes the review-consolidate path for identical titles).
        assert_eq!(r.decision, "auto_merge");
        assert_eq!(r.work_id, existing, "links to the existing work");
        let works: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(works, 1, "no provisional work created");
        let mc: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mc, 0, "corroborated consolidation enqueues no review row");
    }

    /// Build a `ConsolidateWork` for the gate unit tests.
    fn cw(id: &str, title: &str, year: Option<i64>, author: Option<&str>) -> ConsolidateWork {
        ConsolidateWork {
            id: id.into(),
            primary_title: Some(title.into()),
            year,
            author: author.map(str::to_string),
            cover_phash: None,
        }
    }

    /// P0-1: the ambiguity gate. A shared normalized ALIAS is not evidence of a
    /// duplicate — `merge_works` deletes the loser, so every condition must hold.
    #[test]
    fn consolidate_gate_requires_primary_title_length_and_corroboration() {
        let n = "naruto";
        // Corroborated by year: merges.
        assert!(consolidate_gate(
            n,
            &cw("a", "Naruto", Some(1999), None),
            &cw("b", "Naruto", Some(2000), None)
        )
        .is_ok());
        // Corroborated by author (order-insensitive key): merges.
        assert_eq!(
            consolidate_gate(
                n,
                &cw("a", "Naruto", None, Some("Masashi Kishimoto")),
                &cw("b", "Naruto", None, Some("Kishimoto, Masashi")),
            ),
            Ok(())
        );
        // Title only, nothing else: refused.
        assert_eq!(
            consolidate_gate(
                n,
                &cw("a", "Naruto", None, None),
                &cw("b", "Naruto", None, None)
            ),
            Err("uncorroborated")
        );
        // Different authors + far-apart years: refused.
        assert_eq!(
            consolidate_gate(
                n,
                &cw("a", "Naruto", Some(1999), Some("A B")),
                &cw("b", "Naruto", Some(2015), Some("C D")),
            ),
            Err("uncorroborated")
        );
        // The alias is only an ALT title on one side (its primary is something else):
        // this is the JoJo/`ジョジョの奇妙な冒険` shape — refused.
        assert_eq!(
            consolidate_gate(
                n,
                &cw("a", "Naruto", Some(1999), None),
                &cw("b", "Boruto", Some(1999), None),
            ),
            Err("alt_title_only")
        );
        // Too short to be discriminating.
        assert_eq!(
            consolidate_gate(
                "ao",
                &cw("a", "Ao", Some(1999), None),
                &cw("b", "Ao", Some(1999), None)
            ),
            Err("short_title")
        );
    }

    /// P0-1 end-to-end: three works sharing one alias is a GENERIC-TITLE cluster, not a
    /// duplicate cluster. Nothing may be merged; every pair goes to the review queue.
    #[tokio::test]
    async fn consolidate_refuses_oversized_alias_clusters() {
        let pool = migrated_pool().await;
        let mut ids = Vec::new();
        for (i, title) in ["First Love", "Hatsukoi", "Первая любовь"]
            .iter()
            .enumerate()
        {
            let id = catalog::create_work(
                &pool,
                &catalog::WorkInput {
                    primary_title: Some((*title).into()),
                    year: Some(2015),
                    author: Some("Same Author".into()),
                    // Every work carries the SHARED alias plus its own primary title.
                    aliases: vec![
                        catalog::Alias {
                            raw: (*title).into(),
                            lang: None,
                        },
                        catalog::Alias {
                            raw: "First Love".into(),
                            lang: None,
                        },
                    ],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            // Give each a source_series so a refusal is queueable rather than dropped.
            sqlx::query(
                "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
                 VALUES (?, ?, 'suwayomi', 'src', ?, '2020-01-01T00:00:00Z')",
            )
            .bind(format!("ss{i}"))
            .bind(&id)
            .bind(format!("k{i}"))
            .execute(&pool)
            .await
            .unwrap();
            ids.push(id);
        }

        let out = consolidate_exact_duplicates_from(&pool, None, 100, "")
            .await
            .unwrap();
        assert!(
            out.merged.is_empty(),
            "a 3-way alias cluster must never merge: {:?}",
            out.merged
        );
        assert!(
            out.queued > 0,
            "refused pairs must land in the review queue"
        );
        let works: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(works, 3, "no work may be deleted");

        // Re-running must not grow the queue (idempotent across ANY status).
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap();
        let _ = consolidate_exact_duplicates_from(&pool, None, 100, "")
            .await
            .unwrap();
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before, after, "re-running must not duplicate review rows");
    }

    /// P0-1: a corroborated 2-work cluster IS the case consolidation exists for.
    #[tokio::test]
    async fn consolidate_merges_a_corroborated_pair() {
        let pool = migrated_pool().await;
        for _ in 0..2 {
            catalog::create_work(
                &pool,
                &catalog::WorkInput {
                    primary_title: Some("Twin Star Exorcists".into()),
                    year: Some(2013),
                    aliases: vec![catalog::Alias {
                        raw: "Twin Star Exorcists".into(),
                        lang: None,
                    }],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let out = consolidate_exact_duplicates_from(&pool, None, 100, "")
            .await
            .unwrap();
        assert_eq!(out.merged.len(), 1, "the corroborated pair folds");
        let works: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(works, 1);
    }

    /// The review-queue side of a refusal, asserted directly because the sweep re-runs
    /// on every boot: an admin's decision must survive it, the queue must not grow, and
    /// a loser with nowhere to hang a candidate must be COUNTED rather than merged.
    #[tokio::test]
    async fn queue_consolidate_review_is_idempotent_and_never_resurrects_a_decision() {
        let pool = migrated_pool().await;
        let mk = |title: &'static str| {
            let pool = pool.clone();
            async move {
                catalog::create_work(
                    &pool,
                    &catalog::WorkInput {
                        primary_title: Some(title.into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
            }
        };
        let survivor = mk("Survivor Work").await;
        let loser = mk("Loser Work").await;

        // No `source_series` for the loser → unqueueable, and NOTHING is written.
        assert!(
            !queue_consolidate_review(&pool, &loser, &survivor)
                .await
                .unwrap(),
            "a loser with no source_series is reported unqueueable, not queued"
        );
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        // …and it is certainly not merged away.
        let alive: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(alive, 2, "an unqueueable refusal must never delete a work");

        // Give it a source series → now it queues exactly one row.
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
             VALUES ('ss1', ?, 'suwayomi', 'src', 'k1', '2020-01-01T00:00:00Z')",
        )
        .bind(&loser)
        .execute(&pool)
        .await
        .unwrap();
        assert!(queue_consolidate_review(&pool, &loser, &survivor)
            .await
            .unwrap());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);

        // The admin REJECTS it. A later sweep must not re-open the same pair.
        sqlx::query("UPDATE merge_candidate SET status = 'rejected', resolved_at = '2026-01-01'")
            .execute(&pool)
            .await
            .unwrap();
        for _ in 0..3 {
            assert!(queue_consolidate_review(&pool, &loser, &survivor)
                .await
                .unwrap());
        }
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT COUNT(*), MAX(status) FROM merge_candidate")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows[0],
            (1, "rejected".to_string()),
            "re-running must neither duplicate the row nor revert it to pending"
        );
    }

    /// REGRESSION: the cover-pHash rung copied `dedup`'s 0.90 threshold but not its
    /// `is_discriminative_phash` guard, so a pair of blank/flat covers (dHash
    /// `0000000000000000`) scored 1.00 against each other and became the SOLE
    /// corroboration for an irreversible `merge_works` between two unrelated works.
    #[test]
    fn consolidate_gate_ignores_a_non_discriminative_cover_phash() {
        let with_hash = |id: &str, hash: &str| ConsolidateWork {
            id: id.into(),
            primary_title: Some("Blank Cover Series".into()),
            year: None,
            author: None,
            cover_phash: Some(hash.into()),
        };
        let n = "blank cover series";

        // An all-zero (and an all-one) 64-bit dHash carries no signal: two of them are
        // a perfect 1.00 "match" and must NOT corroborate anything.
        assert_eq!(
            consolidate_gate(
                n,
                &with_hash("a", "0000000000000000"),
                &with_hash("b", "0000000000000000"),
            ),
            Err("uncorroborated"),
            "a flat cover hash must not corroborate a merge"
        );
        assert_eq!(
            consolidate_gate(
                n,
                &with_hash("a", "ffffffffffffffff"),
                &with_hash("b", "ffffffffffffffff"),
            ),
            Err("uncorroborated")
        );
        // One discriminative side is not enough — dedup requires signal in the hash it
        // is corroborating WITH, so both are tested.
        assert_eq!(
            consolidate_gate(
                n,
                &with_hash("a", "ff00ff00ff00ff00"),
                &with_hash("b", "0000000000000000"),
            ),
            Err("uncorroborated")
        );
        // Two genuinely discriminative, near-identical hashes still corroborate.
        assert_eq!(
            consolidate_gate(
                n,
                &with_hash("a", "ff00ff00ff00ff00"),
                &with_hash("b", "ff00ff00ff00ff01"),
            ),
            Ok(()),
            "a real cover match must still corroborate"
        );
        // The popcount window is dedup's 12..=52 — one bit set is below it.
        assert!(!consolidate_phash_is_discriminative("0000000000000001"));
        assert!(consolidate_phash_is_discriminative("ff00ff00ff00ff00"));
        // Wrong length / non-hex are rejected rather than panicking.
        assert!(!consolidate_phash_is_discriminative("ff00"));
        assert!(!consolidate_phash_is_discriminative("zzzzzzzzzzzzzzzz"));
    }

    /// REGRESSION: `consolidateExactDuplicates` used to restart its keyset walk at the
    /// beginning on every call. A REFUSED cluster stays in `work_alias`, so the head of
    /// the ordering silts up with refusals and the mutation goes permanently no-op —
    /// measured on production, the first 100 alias groups hold 12 mergeable clusters and
    /// 88 refusals, out of 1,154 mergeable clusters overall. Successive calls must
    /// RESUME past what they already refused.
    #[tokio::test]
    async fn consolidate_mutation_resumes_past_refused_alias_groups() {
        let (s, pool) = setup_full(100).await;
        // Process-global cursor: start from a known point (other tests never touch it,
        // but do not rely on that).
        *CONSOLIDATE_CURSOR.lock().unwrap_or_else(|e| e.into_inner()) = String::new();

        // Group 1 (sorts first): uncorroborated — no year, no author → always refused.
        for _ in 0..2 {
            catalog::create_work(
                &pool,
                &catalog::WorkInput {
                    primary_title: Some("Alpha Refused Title".into()),
                    aliases: vec![catalog::Alias {
                        raw: "Alpha Refused Title".into(),
                        lang: None,
                    }],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        // Group 2 (sorts second): corroborated by year → mergeable.
        for _ in 0..2 {
            catalog::create_work(
                &pool,
                &catalog::WorkInput {
                    primary_title: Some("Beta Mergeable Title".into()),
                    year: Some(2013),
                    aliases: vec![catalog::Alias {
                        raw: "Beta Mergeable Title".into(),
                        lang: None,
                    }],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        let call = r#"mutation { consolidateExactDuplicates(limit: 1) }"#;
        let r = exec(&s, call, Some("admintok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        assert_eq!(
            r.data.into_json().unwrap()["consolidateExactDuplicates"],
            serde_json::json!(0),
            "the first group is refused, so nothing merges yet"
        );

        // Second call: must move ON to the next alias group instead of re-walking the
        // refusal. Before the fix this returned 0 forever.
        let r = exec(&s, call, Some("admintok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        assert_eq!(
            r.data.into_json().unwrap()["consolidateExactDuplicates"],
            serde_json::json!(1),
            "the second call must reach the group beyond the refused one"
        );
        let alpha: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work WHERE primary_title = 'Alpha Refused Title'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let beta: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM work WHERE primary_title = 'Beta Mergeable Title'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(alpha, 2, "the refused pair is never merged");
        assert_eq!(beta, 1, "the corroborated pair folds");

        // Running off the end wraps back to the start, so newly-minted duplicates are
        // still reachable without a restart.
        for _ in 0..3 {
            let _ = exec(&s, call, Some("admintok"), "1.1.1.1").await;
        }
        assert_eq!(
            *CONSOLIDATE_CURSOR.lock().unwrap_or_else(|e| e.into_inner()),
            "",
            "the cursor wraps once the walk runs off the end"
        );
    }

    #[tokio::test]
    async fn federated_exact_title_collision_auto_merges() {
        // Exact-title policy (explicit operator decision): a bare exact-title match —
        // zero corroboration, exactly MID — now AUTO-MERGES into the existing work
        // rather than minting a provisional + a merge_candidate. Treating identical
        // normalized titles as the same work is what folds the Suwayomi catalogue into
        // the MangaDex spine; the accepted trade-off is that two genuinely distinct
        // same-titled series will merge. (FUZZY title-only collisions still take the
        // cautious provisional + review path — see `federated_consolidate_ok`.)
        let pool = migrated_pool().await;
        let existing = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Twin Star Exorcists".into()),
                aliases: vec![crate::catalog::Alias {
                    raw: "Twin Star Exorcists".into(),
                    lang: None,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // Same title, NO description/cover → exact title-only hit.
        let m = suwayomi_manga(7, "Twin Star Exorcists", &["Action"], "src-ext2");
        let r = add_source_series_core_ex(&pool, &m, None, true)
            .await
            .unwrap();
        assert_eq!(r.decision, "auto_merge", "exact title auto-merges");
        assert_eq!(r.work_id, existing, "merged into the existing work");
        let works: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            works, 1,
            "no provisional minted — merged into the existing work"
        );
        let mc: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mc, 0, "no review row — the exact-title match auto-merged");
        // The new source mapping points at the existing (merged-into) work.
        let linked: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_id = 'src-ext2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(linked, r.work_id);
        assert_eq!(
            linked, existing,
            "the mapping resolves to the existing work"
        );
    }

    #[tokio::test]
    async fn reconcile_merges_provisional_into_spine_on_exact_title() {
        let pool = migrated_pool().await;
        // 1. A Suwayomi series enrolled BEFORE any MangaDex spine → its own provisional
        //    work (nothing to match against yet).
        let m = suwayomi_manga(42, "Reconcile Target", &["Action"], "src-suw");
        let r = add_source_series_core(&pool, &m, None).await.unwrap();
        assert_eq!(r.decision, "new", "no spine yet → provisional work");
        let provisional = r.work_id.clone();

        // 2. The MangaDex spine later gains the same title as a separate canonical work
        //    (as CATALOGUE_SYNC upserts it — no dedup at upsert time).
        let spine = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Reconcile Target".into()),
                aliases: vec![crate::catalog::Alias {
                    raw: "Reconcile Target".into(),
                    lang: None,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // A real spine work carries a MangaDex source_series (upsert_work_from_mangadex
        // links one) — so it's not itself "provisional".
        crate::catalog::upsert_source_series(
            &pool, &spine, "mangadex", "md-src", "md-key", None, false,
        )
        .await
        .unwrap();

        // 3. Reconcile: the provisional folds into the spine work (exact title).
        let (merged, queued, skipped) = reconcile_provisional_works(&pool, None).await.unwrap();
        assert_eq!((merged, queued, skipped), (1, 0, 0));

        // The provisional work is gone; the Suwayomi mapping now points at the spine.
        let gone: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work WHERE id = ?")
            .bind(&provisional)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(gone, 0, "provisional work merged away");
        let linked: String = sqlx::query_scalar(
            "SELECT work_id FROM source_series WHERE source_type = 'suwayomi' AND source_key = '42'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(linked, spine, "Suwayomi source repointed to the spine work");

        // Re-running is a no-op — nothing provisional remains.
        let again = reconcile_provisional_works(&pool, None).await.unwrap();
        assert_eq!(again, (0, 0, 0), "reconcile is idempotent");
    }

    /// REGRESSION (BUG 1), end to end through the scanner that actually caused it.
    ///
    /// `RECONCILE_PENDING_WHERE` only skips a work while its candidate is PENDING, so the
    /// moment an admin rejects one the work becomes selectable again and `reconcile_one`
    /// recomputes the identical fuzzy match. With the old plain `INSERT` that wrote a
    /// fresh `pending` row every sweep — 4 of production's 5 duplicate pairs were exactly
    /// this, an admin's "no" silently reappearing as an open question. A short exact title
    /// ("love") is the cheapest way to make `dedup` return `Review` rather than AutoMerge.
    #[tokio::test]
    async fn reconcile_never_re_proposes_a_pair_the_admin_rejected() {
        let pool = migrated_pool().await;
        // Provisional Suwayomi-only work, minted before any spine exists.
        let m = suwayomi_manga(77, "Love", &["Romance"], "src-suw");
        let r = add_source_series_core(&pool, &m, None).await.unwrap();
        assert_eq!(r.decision, "new");

        // The spine later gains a work with the same (short, generic) title.
        let spine = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Love".into()),
                aliases: vec![crate::catalog::Alias {
                    raw: "Love".into(),
                    lang: None,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        crate::catalog::upsert_source_series(
            &pool, &spine, "mangadex", "md-src", "md-key", None, false,
        )
        .await
        .unwrap();

        // Pass 1: mid-confidence → queued for a human, nothing merged.
        let (merged, queued, _) = reconcile_provisional_works(&pool, None).await.unwrap();
        assert_eq!(
            (merged, queued),
            (0, 1),
            "a short generic title must be reviewed, not merged"
        );
        let mc_id: String = sqlx::query_scalar("SELECT id FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap();

        // The admin says NO.
        sqlx::query(
            "UPDATE merge_candidate SET status='rejected', resolved_at='2026-01-01' WHERE id=?",
        )
        .bind(&mc_id)
        .execute(&pool)
        .await
        .unwrap();

        // Passes 2..4: the work IS re-selected (the pending-only exclusion no longer
        // covers it) and the same match IS recomputed — the guard is in the writer.
        for _ in 0..3 {
            let (merged, queued, _) = reconcile_provisional_works(&pool, None).await.unwrap();
            assert_eq!(
                (merged, queued),
                (0, 0),
                "a rejected pair is neither merged nor re-queued"
            );
        }
        let rows: (i64, String) =
            sqlx::query_as("SELECT COUNT(*), MAX(status) FROM merge_candidate")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            (1, "rejected".to_string()),
            "the admin's decision stands: no duplicate row, no reversion to pending"
        );
    }

    /// A stale-gate abort is an ordinary outcome the sweep absorbs; every other merge
    /// failure must still propagate and stop the pass.
    #[test]
    fn only_the_precondition_error_is_treated_as_a_stale_gate() {
        assert!(is_precondition_failure(&anyhow::anyhow!(
            "merge precondition failed: work w_1 changed under the gate"
        )));
        assert!(!is_precondition_failure(&anyhow::anyhow!(
            "no such work: w_1"
        )));
        assert!(!is_precondition_failure(&anyhow::anyhow!(
            "database is locked"
        )));
    }

    #[tokio::test]
    async fn add_source_series_safe_genre_is_not_nsfw() {
        let pool = migrated_pool().await;
        let m = suwayomi_manga(
            99,
            "Wholesome Slice of Life",
            &["Comedy", "Slice of Life"],
            "src1",
        );
        let r = add_source_series_core(&pool, &m, None).await.unwrap();
        let nsfw: i64 = sqlx::query_scalar("SELECT is_nsfw FROM work WHERE id = ?")
            .bind(&r.work_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(nsfw, 0, "safe genres → work.is_nsfw stays 0");
    }

    // ---- GraphQL security integration tests (no Suwayomi needed) ----

    async fn seed_user(pool: &SqlitePool, id: &str, username: &str, is_admin: i64, is_banned: i64) {
        let hash = auth::hash_password("password123").unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, avatar_url, is_admin, is_banned, created_at) \
             VALUES (?, ?, ?, ?, NULL, ?, ?, '2020-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(username)
        .bind(format!("{username}@example.com"))
        .bind(&hash)
        .bind(is_admin)
        .bind(is_banned)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn setup() -> ApiSchema {
        setup_with_limit(100).await
    }

    async fn setup_with_limit(max: u32) -> ApiSchema {
        setup_full(max).await.0
    }

    /// Like `setup_with_limit`, but also hands back the pool so tests can seed
    /// canonical-catalogue rows directly.
    /// Seed a catalogued series reachable under the reader-facing key `key`: a `work`
    /// plus the Suwayomi `source_series` mapping `resolve_work_id` follows. Enough to
    /// satisfy the `known_series_id` existence check on the social mutations.
    async fn seed_target_series(pool: &SqlitePool, key: &str) {
        let work_id = format!("w_{key}");
        sqlx::query(
            "INSERT INTO work (id, primary_title, is_nsfw, created_at, updated_at) \
             VALUES (?, ?, 0, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
        )
        .bind(&work_id)
        .bind(format!("Fixture {key}"))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series \
               (id, work_id, source_type, source_id, source_key, created_at, last_seen) \
             VALUES (?, ?, 'suwayomi', 'src', ?, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
        )
        .bind(format!("ss_{key}"))
        .bind(&work_id)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn setup_full(max: u32) -> (ApiSchema, SqlitePool) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        seed_user(&pool, "admin-id", "admin", 1, 0).await;
        seed_user(&pool, "bob-id", "bob", 0, 0).await;
        seed_user(&pool, "banned-id", "carol", 0, 1).await;
        // Sessions store sha256(token); clients still present the raw token
        // ("admintok"/"bobtok") in the Authorization header, which `user_for_token`
        // re-hashes before lookup — so seed the hashed value here.
        for (tok, uid) in [("admintok", "admin-id"), ("bobtok", "bob-id")] {
            sqlx::query(
                "INSERT INTO sessions (token, user_id, created_at, expires_at) \
                 VALUES (?, ?, '2020-01-01T00:00:00Z', '2999-01-01T00:00:00Z')",
            )
            .bind(auth::hash_token(tok))
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
        }
        // An already-expired session — its token must not resolve (A1).
        sqlx::query(
            "INSERT INTO sessions (token, user_id, created_at, expires_at) \
             VALUES (?, 'bob-id', '2020-01-01T00:00:00Z', '2020-02-01T00:00:00Z')",
        )
        .bind(auth::hash_token("expiredtok"))
        .execute(&pool)
        .await
        .unwrap();
        // Comment/review targets used across the social tests. `postComment`/`postReview`
        // now REJECT ids that resolve to nothing (they write into FK-less TEXT columns),
        // so the threads these tests open must hang off real rows.
        seed_target_series(&pool, "s1").await;
        seed_target_series(&pool, "s2").await;
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, status, source_id, chapter_count, updated_at) \
             VALUES (42, 'Fixture 42', 'ONGOING', 'src', 0, '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let state = std::sync::Arc::new(AppState {
            pool: pool.clone(),
            cover_pool: pool.clone(),
            suwayomi: crate::suwayomi::SuwayomiClient::new("http://127.0.0.1:1".into(), None, None),
            mangadex: std::sync::Arc::new(crate::mangadex::MangaDexClient::new(
                "test-ua", 5.0, 40.0,
            )),
            admin_users: vec![],
            scan_health: Mutex::new(ScanHealth::default()),
            auth_limiter: RateLimiter::new(max, 60),
            federated_limiter: RateLimiter::new(100, 60),
            session_ttl_secs: 30 * 24 * 60 * 60,
            series_inflight: KeyedLocks::default(),
            chapters_inflight: KeyedLocks::default(),
            cover_crawl_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            catalogue_cover_phash: false,
            ext_icons_dir: std::path::PathBuf::new(),
        });
        (build_schema(state, false), pool)
    }

    async fn exec(
        schema: &ApiSchema,
        query: &str,
        token: Option<&str>,
        ip: &str,
    ) -> async_graphql::Response {
        let req = async_graphql::Request::new(query)
            .data(RequestAuth(token.map(|t| t.to_string())))
            .data(ClientIp(Some(ip.to_string())))
            .data(RequestUserCache::default())
            .data(RequestLibraryCache::default());
        schema.execute(req).await
    }

    /// `ensure_utc_offset` must be a NO-OP on anything already carrying an offset — and
    /// in particular must not be fooled by the `-` separators in the DATE part into
    /// thinking a naive value already has a negative UTC offset. Every dated
    /// `series_scan_state` row in production is the `+00:00` case, so the pass-through
    /// arm is the one that actually runs; the appending arm guards future writers.
    #[test]
    fn ensure_utc_offset_appends_only_when_missing() {
        for already in [
            "2026-07-26T13:48:54.108583013+00:00",
            "2026-07-26T13:48:54Z",
            "2026-07-26T08:48:54-05:00",
        ] {
            assert_eq!(ensure_utc_offset(already), already, "must pass through");
        }
        // Naive: the date's own hyphens must not be read as an offset.
        assert_eq!(
            ensure_utc_offset("2026-07-26T13:58:51.705034"),
            "2026-07-26T13:58:51.705034Z"
        );
        assert_eq!(
            ensure_utc_offset("2026-07-26T13:58:51"),
            "2026-07-26T13:58:51Z"
        );
        // Surrounding whitespace is trimmed rather than baked into the output.
        assert_eq!(
            ensure_utc_offset("  2026-07-26T13:58:51  "),
            "2026-07-26T13:58:51Z"
        );
        // Parseable by the same reader-side path that was reading these as local time.
        assert!(
            chrono::DateTime::parse_from_rfc3339(&ensure_utc_offset("2026-07-26T13:58:51")).is_ok(),
            "output must be valid RFC 3339"
        );
    }

    fn first_error(resp: &async_graphql::Response) -> String {
        resp.errors
            .first()
            .map(|e| e.message.clone())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn updates_feed_is_newest_first_and_reports_new_chapter_time() {
        // "Latest Updates" ORDERS by the REAL upstream release time of the newest
        // chapter (`latestChapterAt`, from `suwayomi_series.latest_chapter_at`,
        // migration 0050) — which is the timestamp the reader prints on every card.
        // The scanner's DETECTION time decides membership and is exposed separately as
        // `detectedAt`; it must not be written over `updatedAt`/`latestChapterAt`
        // (doing so made the feed claim a chapter uploaded days ago had landed an hour
        // ago) and it must NOT drive the order either.
        //
        // The two clocks are deliberately INVERTED in this fixture: the row detected
        // FIRST has the NEWER release time. Ordering by detection and ordering by
        // release therefore give opposite answers, so the assertions below can tell
        // the two apart. (They previously agreed, which made this test pass either way.)
        let (s, pool) = setup_full(100).await;
        for (id, title, new_at, latest_ms) in [
            (
                10_i64,
                // detected EARLIER, released LATER -> must sort FIRST.
                "Older Detection Newer Release",
                "2026-07-01T00:00:00+00:00",
                "1751328000000",
            ),
            (
                20,
                // detected LATER, released EARLIER -> must sort SECOND.
                "Newer Detection Older Release",
                "2026-07-10T00:00:00+00:00",
                "1748736000000",
            ),
        ] {
            sqlx::query(
                "INSERT INTO suwayomi_series \
                   (id, title, status, source_id, chapter_count, in_library, latest_chapter_at, updated_at) \
                 VALUES (?, ?, 'ONGOING', 'src', 5, 1, ?, '2026-07-15T00:00:00+00:00')",
            )
            .bind(id)
            .bind(title)
            .bind(latest_ms)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO series_scan_state \
                   (series_id, avg_interval_hours, known_chapter_count, last_new_chapter_at, updated_at) \
                 VALUES (?, 0, 5, ?, '2026-07-15T00:00:00+00:00')",
            )
            .bind(id.to_string())
            .bind(new_at)
            .execute(&pool)
            .await
            .unwrap();
        }

        let r = exec(
            &s,
            r#"{ updates { items { id title updatedAt detectedAt latestChapterAt } total } }"#,
            None,
            "1.2.3.4",
        )
        .await;
        assert!(r.errors.is_empty(), "updates failed: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        let items = data["updates"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        // Newest RELEASE time first. The earlier-detected row wins because its chapter
        // is the newer one — the whole point of the ordering.
        assert_eq!(
            items[0]["title"],
            serde_json::json!("Older Detection Newer Release")
        );
        assert_eq!(
            items[1]["title"],
            serde_json::json!("Newer Detection Older Release")
        );
        // `detectedAt` still carries the detection time, and is now plainly NOT the
        // sort key: it ascends down the page while the feed descends by release time.
        assert_eq!(
            items[0]["detectedAt"],
            serde_json::json!("2026-07-01T00:00:00+00:00")
        );
        assert_eq!(
            items[1]["detectedAt"],
            serde_json::json!("2026-07-10T00:00:00+00:00")
        );
        // ...and `updatedAt` stays HONEST (the source's own last-touch stamp), no
        // longer overwritten with our detection time.
        assert_ne!(
            items[0]["updatedAt"],
            serde_json::json!("2026-07-01T00:00:00+00:00"),
            "updatedAt must not be overwritten with the detection time"
        );
        // `latestChapterAt` is the stored upstream newest-chapter time, not the poll —
        // and it descends, because it IS the sort key.
        assert_eq!(
            items[0]["latestChapterAt"],
            serde_json::json!(to_iso(Some("1751328000000")).unwrap())
        );
        assert_eq!(
            items[1]["latestChapterAt"],
            serde_json::json!(to_iso(Some("1748736000000")).unwrap())
        );
    }

    /// Seed one Updates-feed member: the library `suwayomi_series` row that carries the
    /// release-time sort key, plus the `series_scan_state` row that grants membership.
    ///
    /// `latest_ms` is epoch-millis TEXT (as migration 0050 stores it) or `None` for a
    /// series with no datable chapter; `detected_at` is the ISO-8601 detection stamp.
    /// `in_library` is a parameter because the difference between 0 and 1 is exactly what
    /// `updates_total_matches_paged_row_count` is about.
    async fn seed_feed_member(
        pool: &SqlitePool,
        id: i64,
        title: &str,
        latest_ms: Option<&str>,
        detected_at: &str,
        in_library: i64,
    ) {
        sqlx::query(
            "INSERT INTO suwayomi_series \
               (id, title, status, source_id, chapter_count, in_library, latest_chapter_at, updated_at) \
             VALUES (?, ?, 'ONGOING', 'src', 5, ?, ?, '2026-07-15T00:00:00+00:00')",
        )
        .bind(id)
        .bind(title)
        .bind(in_library)
        .bind(latest_ms)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO series_scan_state \
               (series_id, avg_interval_hours, known_chapter_count, last_new_chapter_at, updated_at) \
             VALUES (?, 0, 5, ?, '2026-07-15T00:00:00+00:00')",
        )
        .bind(id.to_string())
        .bind(detected_at)
        .execute(pool)
        .await
        .unwrap();
    }

    /// REGRESSION GUARD for the Updates feed's sort key.
    ///
    /// The feed ordered by `series_scan_state.last_new_chapter_at` — the moment OUR
    /// scanner noticed the chapter — while the reader labelled every card with
    /// `latestChapterAt`, the real upstream release time. Those clocks are uncorrelated:
    /// measured against production, the top card of the feed read "36d" and position 10
    /// read "74d". A chapter released six months ago that we happened to poll a minute
    /// ago outranked one released this morning.
    ///
    /// The assertion is deliberately made on `latestChapterAt` — the field the reader
    /// RENDERS — and not on an id order, so it keeps holding if the fixture or the
    /// tiebreaker changes. Detection order here is the exact REVERSE of release order,
    /// so the old behaviour cannot pass.
    #[tokio::test]
    async fn updates_orders_by_release_time_not_detection() {
        let (s, pool) = setup_full(100).await;
        // (id, title, latest_chapter_at millis, detected_at)
        // Release order:   B (Jul 20) > A (Jul 10) > C (Jul 01)
        // Detection order: C (13:00)  > A (12:00)  > B (11:00)   <- exactly reversed
        for (id, title, latest_ms, detected) in [
            (
                1_i64,
                "Middle Release",
                "1752105600000", // 2025-07-10
                "2026-07-26T12:00:00+00:00",
            ),
            (
                2,
                "Newest Release",
                "1752969600000", // 2025-07-20
                "2026-07-26T11:00:00+00:00",
            ),
            (
                3,
                "Oldest Release",
                "1751328000000", // 2025-07-01
                "2026-07-26T13:00:00+00:00",
            ),
        ] {
            seed_feed_member(&pool, id, title, Some(latest_ms), detected, 1).await;
        }

        let r = exec(
            &s,
            r#"{ updates { items { title detectedAt latestChapterAt } total } }"#,
            None,
            "1.2.3.4",
        )
        .await;
        assert!(r.errors.is_empty(), "updates failed: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        let items = data["updates"]["items"].as_array().unwrap();
        assert_eq!(
            items.len(),
            3,
            "all three are library members with detections"
        );
        assert_eq!(data["updates"]["total"], serde_json::json!(3));

        // The visible label must be monotonically NON-INCREASING down the page.
        let released: Vec<&str> = items
            .iter()
            .map(|i| i["latestChapterAt"].as_str().expect("latestChapterAt set"))
            .collect();
        let mut sorted = released.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(
            released, sorted,
            "the feed must descend by the release time it displays, got {released:?}"
        );
        // And the fixture really did disagree: ordering by detection would have put
        // "Oldest Release" first. (If this ever stops holding the test has gone blind.)
        assert_eq!(items[0]["title"], serde_json::json!("Newest Release"));
        assert_eq!(items[2]["title"], serde_json::json!("Oldest Release"));
        let detected: Vec<&str> = items
            .iter()
            .map(|i| i["detectedAt"].as_str().expect("detectedAt set"))
            .collect();
        assert_ne!(
            detected,
            {
                let mut d = detected.clone();
                d.sort_unstable_by(|a, b| b.cmp(a));
                d
            },
            "fixture is not exercising the bug: detection order already matches release order"
        );
    }

    /// A series with NO release time sorts LAST, and is neither dropped nor promoted.
    ///
    /// This is the row a `COALESCE(latest_chapter_at, last_new_chapter_at)` would have
    /// put FIRST: the two columns are stored in different encodings (13-digit epoch
    /// millis TEXT vs ISO-8601 TEXT), so under BINARY collation every '2...' ISO
    /// fallback sorts above every '1...' epoch value. Its detection time here is the
    /// most recent in the fixture, so a fallback to detection time would also float it
    /// to the top.
    ///
    /// It must still be PRESENT (it is a genuine library member with a genuine
    /// detection) and still counted in `total` — the ordering says "we don't know when
    /// this was released", not "this doesn't exist".
    #[tokio::test]
    async fn updates_sorts_null_latest_chapter_at_last() {
        let (s, pool) = setup_full(100).await;
        seed_feed_member(
            &pool,
            1,
            "Has Release Time",
            Some("1751328000000"), // 2025-07-01
            "2026-07-01T00:00:00+00:00",
            1,
        )
        .await;
        seed_feed_member(
            &pool,
            2,
            "No Release Time",
            None,
            // The most recent detection in the fixture — the COALESCE trap.
            "2026-07-26T23:59:59+00:00",
            1,
        )
        .await;

        let r = exec(
            &s,
            r#"{ updates { items { title detectedAt latestChapterAt } total hasNextPage } }"#,
            None,
            "1.2.3.4",
        )
        .await;
        assert!(r.errors.is_empty(), "updates failed: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        let items = data["updates"]["items"].as_array().unwrap();
        assert_eq!(
            items.len(),
            2,
            "the undated row is not dropped from the page"
        );
        assert_eq!(
            data["updates"]["total"],
            serde_json::json!(2),
            "the undated row is still counted"
        );
        assert_eq!(items[0]["title"], serde_json::json!("Has Release Time"));
        assert_eq!(
            items[1]["title"],
            serde_json::json!("No Release Time"),
            "a row with no release time belongs at the BOTTOM, not the top"
        );
        // `latestChapterAt` is `String!` in the schema, so "no dated chapter" is the
        // EMPTY STRING, not null. Either way the point is that no time is invented for
        // it — in particular the detection time is not substituted in.
        assert_eq!(
            items[1]["latestChapterAt"],
            serde_json::json!(""),
            "no release time must be reported as empty, not invented"
        );
        // Membership data survives: this is a sort change, not a data removal.
        assert_eq!(
            items[1]["detectedAt"],
            serde_json::json!("2026-07-26T23:59:59+00:00")
        );
    }

    /// The `id DESC` tiebreaker gives the feed a TOTAL order, so paging cannot repeat or
    /// skip rows.
    ///
    /// `latest_chapter_at` ties are common, not theoretical: production has 34 groups of
    /// rows sharing one timestamp, covering 143 of the 1,316 feed members (whole batches
    /// of series get the same coarse upstream date). With no tiebreaker, SQLite is free
    /// to return tied rows in any order, and it need not be the SAME order for the
    /// OFFSET-0 and OFFSET-20 executions — so a row could appear on both pages while
    /// another appeared on neither. This test seeds `PAGE_SIZE + 5` rows sharing ONE
    /// timestamp, which is nothing BUT a tie, and would flap without the tiebreaker.
    #[tokio::test]
    async fn updates_tiebreaks_on_id_without_duplicates_across_pages() {
        let (s, pool) = setup_full(100).await;
        let n = PAGE_SIZE + 5;
        for id in 1..=n {
            seed_feed_member(
                &pool,
                id,
                &format!("Tied Series {id:02}"),
                Some("1751328000000"), // every row: the SAME release time
                "2026-07-20T00:00:00+00:00",
                1,
            )
            .await;
        }

        let ids_on = |page: i32| {
            let s = &s;
            async move {
                let r = exec(
                    s,
                    &format!("{{ updates(page: {page}) {{ items {{ id }} total hasNextPage }} }}"),
                    None,
                    "1.2.3.4",
                )
                .await;
                assert!(r.errors.is_empty(), "updates failed: {:?}", r.errors);
                let data = r.data.into_json().unwrap();
                let ids: Vec<i64> = data["updates"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["id"].as_str().unwrap().parse().unwrap())
                    .collect();
                (ids, data["updates"]["total"].as_i64().unwrap())
            }
        };
        let (p1, total) = ids_on(1).await;
        let (p2, _) = ids_on(2).await;
        assert_eq!(total, n, "total counts every seeded member");
        assert_eq!(p1.len() as i64, PAGE_SIZE, "page 1 is full");
        assert_eq!(p2.len() as i64, n - PAGE_SIZE, "page 2 holds the remainder");

        // Disjoint: no id may be served twice.
        let set1: std::collections::HashSet<i64> = p1.iter().copied().collect();
        let set2: std::collections::HashSet<i64> = p2.iter().copied().collect();
        assert!(
            set1.is_disjoint(&set2),
            "pages overlap: {:?}",
            set1.intersection(&set2).collect::<Vec<_>>()
        );
        // And gapless: together they are exactly the seeded set.
        let mut seen: Vec<i64> = set1.union(&set2).copied().collect();
        seen.sort_unstable();
        assert_eq!(
            seen,
            (1..=n).collect::<Vec<i64>>(),
            "paging lost or invented rows across the tie"
        );
        // The tiebreaker is `id DESC`, so page 1 is the high ids.
        assert_eq!(p1[0], n, "highest id first within a tie group");
    }

    /// `total` must count EXACTLY the rows the pages can return.
    ///
    /// `total` used to count every dated `series_scan_state` row while the page query is
    /// now driven from `suwayomi_series WHERE in_library = 1`. Scan state outlives
    /// library membership (removing a series from the library does not delete its scan
    /// row), so a stale row inflated `total` and with it `hasNextPage` and the reader's
    /// page count — offering a page that comes back empty.
    #[tokio::test]
    async fn updates_total_matches_paged_row_count() {
        let (s, pool) = setup_full(100).await;
        seed_feed_member(
            &pool,
            1,
            "In Library",
            Some("1751328000000"),
            "2026-07-20T00:00:00+00:00",
            1,
        )
        .await;
        // Same shape, same detection — but no longer a library member.
        seed_feed_member(
            &pool,
            2,
            "Dropped From Library",
            Some("1752969600000"), // NEWER release: would be first if it counted at all
            "2026-07-21T00:00:00+00:00",
            0,
        )
        .await;

        let r = exec(
            &s,
            r#"{ updates { items { id title } total hasNextPage } }"#,
            None,
            "1.2.3.4",
        )
        .await;
        assert!(r.errors.is_empty(), "updates failed: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        let items = data["updates"]["items"].as_array().unwrap();
        let titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
        assert_eq!(
            titles,
            vec!["In Library"],
            "a series that left the library must not appear"
        );
        assert_eq!(
            data["updates"]["total"],
            serde_json::json!(1),
            "total must not count the row the pages cannot return"
        );
        assert_eq!(data["updates"]["hasNextPage"], serde_json::json!(false));
        assert_eq!(
            items.len() as i64,
            data["updates"]["total"].as_i64().unwrap(),
            "single-page feed: items and total must agree exactly"
        );
    }

    /// A cheap pin on the OTHER updates feed, which is already correct.
    ///
    /// `canonicalUpdates` reads `feed_updates.latest_at` — the real chapter publish time
    /// — and has always ordered by it. That is now the SAME contract the Suwayomi
    /// `updates` feed follows ("sort by the clock you display"), and the two feeds are
    /// merged into one list client-side, so if this one ever drifted onto a different
    /// clock the merged Updates grid would silently interleave two orderings again. This
    /// asserts the alignment rather than assuming it.
    #[tokio::test]
    async fn canonical_updates_orders_by_latest_at() {
        let (s, pool) = setup_full(100).await;
        // `seed_canonical` publishes chapter `ch` at 2026-07-0{ch}, so the digit IS the
        // release date — seeded here in deliberately NON-descending order.
        seed_canonical(&pool, "md-mid", "Mid Release", false, "2").await;
        seed_canonical(&pool, "md-new", "New Release", false, "3").await;
        seed_canonical(&pool, "md-old", "Old Release", false, "1").await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let r = exec(
            &s,
            r#"{ canonicalUpdates { title latestAt } }"#,
            None,
            "1.2.3.4",
        )
        .await;
        assert!(
            r.errors.is_empty(),
            "canonicalUpdates failed: {:?}",
            r.errors
        );
        let data = r.data.into_json().unwrap();
        let rows = data["canonicalUpdates"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        let at: Vec<&str> = rows
            .iter()
            .map(|x| x["latestAt"].as_str().expect("latestAt is NOT NULL"))
            .collect();
        let mut sorted = at.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(at, sorted, "canonicalUpdates must descend by latestAt");
        let titles: Vec<&str> = rows.iter().map(|x| x["title"].as_str().unwrap()).collect();
        assert_eq!(titles, vec!["New Release", "Mid Release", "Old Release"]);
    }

    #[tokio::test]
    async fn comment_votes_and_notifications_flow() {
        let (s, _pool) = setup_full(100).await;
        // admin posts a root comment on series "s1".
        let r = exec(
            &s,
            r#"mutation { postComment(input: { targetType: "series", targetId: "s1", body: "hi", hasSpoiler: false }) { id likes myVote } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "postComment: {:?}", r.errors);
        let cid = r.data.into_json().unwrap()["postComment"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // bob replies -> admin gets a 'reply' notification.
        let reply = format!(
            r#"mutation {{ postComment(input: {{ targetType: "series", targetId: "s1", parentId: "{cid}", body: "yo", hasSpoiler: false }}) {{ id }} }}"#
        );
        let r = exec(&s, &reply, Some("bobtok"), "2.2.2.2").await;
        assert!(r.errors.is_empty(), "reply: {:?}", r.errors);

        // bob likes admin's comment -> tally = 1 like, and admin gets a 'like_milestone'.
        let vote = format!(
            r#"mutation {{ voteComment(commentId: "{cid}", value: 1) {{ likes dislikes myVote }} }}"#
        );
        let r = exec(&s, &vote, Some("bobtok"), "2.2.2.2").await;
        assert!(r.errors.is_empty(), "vote: {:?}", r.errors);
        let v = r.data.into_json().unwrap()["voteComment"].clone();
        assert_eq!(v["likes"], serde_json::json!(1));
        assert_eq!(v["myVote"], serde_json::json!(1));

        // admin sees 2 unread notifications (reply + like_milestone).
        let r = exec(
            &s,
            r#"{ unreadNotificationCount notifications { kind count actor { username } commentExcerpt } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        let d = r.data.into_json().unwrap();
        assert_eq!(d["unreadNotificationCount"], serde_json::json!(2));
        let notifs = d["notifications"].as_array().unwrap();
        let kinds: Vec<&str> = notifs.iter().map(|n| n["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"reply") && kinds.contains(&"like_milestone"));
        // The reply notification names the actor; the milestone carries the count.
        let reply_n = notifs.iter().find(|n| n["kind"] == "reply").unwrap();
        assert_eq!(reply_n["actor"]["username"], serde_json::json!("bob"));
        let ms = notifs
            .iter()
            .find(|n| n["kind"] == "like_milestone")
            .unwrap();
        assert_eq!(ms["count"], serde_json::json!(1));

        // The comments query shows the like tally; bob (the liker) sees myVote = 1.
        let r = exec(
            &s,
            r#"{ comments(targetType: "series", targetId: "s1") { items { id likes myVote } } }"#,
            Some("bobtok"),
            "2.2.2.2",
        )
        .await;
        let items = r.data.into_json().unwrap()["comments"]["items"]
            .as_array()
            .unwrap()
            .clone();
        let root = items
            .iter()
            .find(|c| c["id"] == serde_json::json!(cid))
            .unwrap();
        assert_eq!(root["likes"], serde_json::json!(1));
        assert_eq!(root["myVote"], serde_json::json!(1));

        // admin marks all read -> 0 unread.
        let r = exec(
            &s,
            r#"mutation { markNotificationsRead }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "markRead: {:?}", r.errors);
        let r = exec(
            &s,
            r#"{ unreadNotificationCount }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(
            r.data.into_json().unwrap()["unreadNotificationCount"],
            serde_json::json!(0)
        );

        // bob (the replier/liker) is never notified of his own actions.
        let r = exec(
            &s,
            r#"{ unreadNotificationCount }"#,
            Some("bobtok"),
            "2.2.2.2",
        )
        .await;
        assert_eq!(
            r.data.into_json().unwrap()["unreadNotificationCount"],
            serde_json::json!(0)
        );
    }

    #[tokio::test]
    async fn vote_self_rejected_and_milestone_dedupes() {
        let (s, _pool) = setup_full(100).await;
        let r = exec(
            &s,
            r#"mutation { postComment(input: { targetType: "series", targetId: "s1", body: "x", hasSpoiler: false }) { id } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        let cid = r.data.into_json().unwrap()["postComment"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // The author can't vote on their own comment.
        let self_vote =
            format!(r#"mutation {{ voteComment(commentId: "{cid}", value: 1) {{ likes }} }}"#);
        let r = exec(&s, &self_vote, Some("admintok"), "1.1.1.1").await;
        assert!(!r.errors.is_empty(), "self-vote must be rejected");

        // bob likes (milestone 1) → unlikes → re-likes: still exactly ONE like_milestone
        // (idempotent per comment+count), never a duplicate on re-crossing.
        let like =
            format!(r#"mutation {{ voteComment(commentId: "{cid}", value: 1) {{ likes }} }}"#);
        let clear =
            format!(r#"mutation {{ voteComment(commentId: "{cid}", value: 0) {{ likes }} }}"#);
        for q in [&like, &clear, &like] {
            let r = exec(&s, q, Some("bobtok"), "2.2.2.2").await;
            assert!(r.errors.is_empty(), "vote failed: {:?}", r.errors);
        }

        let r = exec(
            &s,
            r#"{ notifications { kind count } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        let notifs = r.data.into_json().unwrap()["notifications"]
            .as_array()
            .unwrap()
            .clone();
        let milestones: Vec<_> = notifs
            .iter()
            .filter(|n| n["kind"] == "like_milestone")
            .collect();
        assert_eq!(
            milestones.len(),
            1,
            "milestone-1 must not duplicate on unlike/relike"
        );
        assert_eq!(milestones[0]["count"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn chapter_comment_notification_resolves_owning_series() {
        // A reply on a CHAPTER thread notifies with the owning series id resolved, so
        // the client can deep-link to `/read/<seriesId>?ch=<chapterId>`.
        let (s, pool) = setup_full(100).await;
        // A numeric Suwayomi chapter 9001 belonging to series 500.
        sqlx::query(
            "INSERT INTO suwayomi_chapter (id, manga_id, name, chapter_number, page_count, updated_at) \
             VALUES (9001, 500, 'Ch 1', 1, 10, '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // admin comments on the chapter thread; bob replies.
        let r = exec(
            &s,
            r#"mutation { postComment(input: { targetType: "chapter", targetId: "9001", body: "root", hasSpoiler: false }) { id } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "postComment: {:?}", r.errors);
        let cid = r.data.into_json().unwrap()["postComment"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let reply = format!(
            r#"mutation {{ postComment(input: {{ targetType: "chapter", targetId: "9001", parentId: "{cid}", body: "re", hasSpoiler: false }}) {{ id }} }}"#
        );
        let r = exec(&s, &reply, Some("bobtok"), "2.2.2.2").await;
        assert!(r.errors.is_empty(), "reply: {:?}", r.errors);

        // admin's notification carries targetType=chapter, targetId=9001, seriesId=500.
        let r = exec(
            &s,
            r#"{ notifications { kind targetType targetId seriesId } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        let n = r.data.into_json().unwrap()["notifications"][0].clone();
        assert_eq!(n["kind"], serde_json::json!("reply"));
        assert_eq!(n["targetType"], serde_json::json!("chapter"));
        assert_eq!(n["targetId"], serde_json::json!("9001"));
        assert_eq!(n["seriesId"], serde_json::json!("500"));
    }

    #[tokio::test]
    async fn record_view_counts_anonymously_and_surfaces_on_series() {
        // Views are the popularity signal: `recordView` needs NO auth (anonymous reads
        // count too), and the count surfaces on `series.views` across all windows. Here
        // a cached numeric series is viewed three times by an anonymous client (no
        // token), then read back.
        let (s, pool) = setup_full(100).await;
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, status, source_id, chapter_count, updated_at) \
             VALUES (777, 'Viewed Series', 'ONGOING', 'src', 0, '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        for _ in 0..3 {
            let r = exec(
                &s,
                r#"mutation { recordView(seriesId: "777") }"#,
                None,
                "9.9.9.9",
            )
            .await;
            assert!(r.errors.is_empty(), "recordView failed: {:?}", r.errors);
            assert_eq!(
                r.data.into_json().unwrap()["recordView"],
                serde_json::json!(true)
            );
        }

        let r = exec(
            &s,
            r#"{ series(id: "777") { views { total last7d last24h } } }"#,
            None,
            "9.9.9.9",
        )
        .await;
        assert!(r.errors.is_empty(), "series query failed: {:?}", r.errors);
        let v = r.data.into_json().unwrap()["series"]["views"].clone();
        assert_eq!(v["total"], serde_json::json!(3));
        assert_eq!(v["last24h"], serde_json::json!(3));
        assert_eq!(v["last7d"], serde_json::json!(3));
    }

    fn data_json(resp: &async_graphql::Response) -> String {
        serde_json::to_string(&resp.data).unwrap()
    }

    #[tokio::test]
    async fn set_show_nsfw_persists_and_requires_auth() {
        let s = setup().await;
        // Default is hidden.
        let r = exec(
            &s,
            r#"{ session { user { showNsfw } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(data_json(&r).contains("\"showNsfw\":false"));
        // Toggle on.
        let r = exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let r = exec(
            &s,
            r#"{ session { user { showNsfw } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(data_json(&r).contains("\"showNsfw\":true"));
        // Anonymous cannot set it.
        let r = exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "Not authenticated");
    }

    #[tokio::test]
    async fn update_profile_persists_and_is_reflected_in_session() {
        let s = setup().await;
        // Fresh account: display name/bio are null, joinedAt is present.
        let r = exec(
            &s,
            r#"{ session { user { displayName bio joinedAt } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let j = data_json(&r);
        assert!(j.contains("\"displayName\":null"), "unexpected: {j}");
        assert!(j.contains("\"bio\":null"), "unexpected: {j}");
        assert!(j.contains("\"joinedAt\":\"2020-01-01"), "unexpected: {j}");

        // Update both; the mutation returns the refreshed user.
        let r = exec(
            &s,
            r#"mutation { updateProfile(input: { displayName: "  Bob the Reader  ", bio: "I read manga." }) { displayName bio } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let j = data_json(&r);
        assert!(
            j.contains("\"displayName\":\"Bob the Reader\""),
            "trimmed: {j}"
        );
        assert!(j.contains("\"bio\":\"I read manga.\""), "{j}");

        // A blank display name clears it (falls back to username in the UI).
        let r = exec(
            &s,
            r#"mutation { updateProfile(input: { displayName: "   ", bio: "still here" }) { displayName bio } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(data_json(&r).contains("\"displayName\":null"), "cleared");

        // Anonymous cannot update.
        let r = exec(
            &s,
            r#"mutation { updateProfile(input: { bio: "x" }) { bio } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "Not authenticated");
    }

    #[tokio::test]
    async fn update_profile_rejects_overlong_fields() {
        let s = setup().await;
        let long_name = "a".repeat(51);
        let r = exec(
            &s,
            &format!(
                r#"mutation {{ updateProfile(input: {{ displayName: "{long_name}" }}) {{ id }} }}"#
            ),
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(
            first_error(&r),
            "display name must be at most 50 characters"
        );
    }

    #[tokio::test]
    async fn my_activity_records_reviews_and_is_empty_when_signed_out() {
        let s = setup().await;
        // Signed out → empty feed, never an error.
        let r = exec(&s, r#"{ myActivity { id } }"#, None, "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        assert!(data_json(&r).contains("\"myActivity\":[]"));

        // Posting a review records a 'review' activity targeting the series.
        let r = exec(
            &s,
            r#"mutation { postReview(input: { seriesId: "42", score: 8, body: "great", hasSpoiler: false }) { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);

        let r = exec(
            &s,
            r#"{ myActivity { kind targetType targetId } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let j = data_json(&r);
        assert!(j.contains("\"kind\":\"review\""), "{j}");
        assert!(j.contains("\"targetType\":\"series\""), "{j}");
        assert!(j.contains("\"targetId\":\"42\""), "{j}");
    }

    #[tokio::test]
    async fn introspection_toggles_with_flag() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let mk_state = || {
            std::sync::Arc::new(AppState {
                pool: pool.clone(),
                cover_pool: pool.clone(),
                suwayomi: crate::suwayomi::SuwayomiClient::new(
                    "http://127.0.0.1:1".into(),
                    None,
                    None,
                ),
                mangadex: std::sync::Arc::new(crate::mangadex::MangaDexClient::new(
                    "test-ua", 5.0, 40.0,
                )),
                admin_users: vec![],
                scan_health: Mutex::new(ScanHealth::default()),
                auth_limiter: RateLimiter::new(100, 60),
                federated_limiter: RateLimiter::new(100, 60),
                session_ttl_secs: 30 * 24 * 60 * 60,
                series_inflight: KeyedLocks::default(),
                chapters_inflight: KeyedLocks::default(),
                cover_crawl_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                catalogue_cover_phash: false,
                ext_icons_dir: std::path::PathBuf::new(),
            })
        };
        const Q: &str = "{ __schema { queryType { name } } }";
        // Disabled (production default): `__schema` resolves to null, so the API
        // surface is not enumerable.
        let disabled = build_schema(mk_state(), true);
        let r = disabled.execute(Q).await;
        assert_eq!(
            serde_json::to_string(&r.data).unwrap(),
            "{\"__schema\":null}",
            "disabled introspection must not leak the schema (errors: {:?})",
            r.errors
        );
        // Enabled (dev): introspection returns the real schema.
        let enabled = build_schema(mk_state(), false);
        let r = enabled.execute(Q).await;
        let json = serde_json::to_string(&r.data).unwrap();
        assert!(
            r.errors.is_empty() && json.contains("queryType") && json != "{\"__schema\":null}",
            "introspection should work when enabled: {json} (errors: {:?})",
            r.errors
        );
    }

    #[tokio::test]
    async fn provision_admins_creates_promotes_and_is_idempotent() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();

        // Missing admin + password → created as admin with the configured email.
        provision_admins(&pool, &["admin".into()], Some("s3cret-pw"), Some("a@b.com"))
            .await
            .unwrap();
        let (is_admin, email, hash): (i64, String, String) = sqlx::query_as(
            "SELECT is_admin, email, password_hash FROM users WHERE username='admin'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(is_admin, 1);
        assert_eq!(email, "a@b.com");
        assert!(auth::verify_password("s3cret-pw", &hash), "password usable");

        // Idempotent re-run with a different password: no duplicate, existing
        // password preserved (never re-passworded).
        provision_admins(
            &pool,
            &["admin".into()],
            Some("different-pw"),
            Some("a@b.com"),
        )
        .await
        .unwrap();
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE username='admin'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "no duplicate admin row");
        let (hash2,): (String,) =
            sqlx::query_as("SELECT password_hash FROM users WHERE username='admin'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            auth::verify_password("s3cret-pw", &hash2),
            "existing password preserved, not reset"
        );

        // Existing non-admin gets promoted; no password required.
        seed_user(&pool, "bob-id", "bob", 0, 0).await;
        provision_admins(&pool, &["bob".into()], None, None)
            .await
            .unwrap();
        let (bob_admin,): (i64,) =
            sqlx::query_as("SELECT is_admin FROM users WHERE username='bob'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(bob_admin, 1, "existing account promoted");
    }

    #[tokio::test]
    async fn register_rejects_reserved_admin_username() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let state = std::sync::Arc::new(AppState {
            pool: pool.clone(),
            cover_pool: pool.clone(),
            suwayomi: crate::suwayomi::SuwayomiClient::new("http://127.0.0.1:1".into(), None, None),
            mangadex: std::sync::Arc::new(crate::mangadex::MangaDexClient::new(
                "test-ua", 5.0, 40.0,
            )),
            admin_users: vec!["admin".into()],
            scan_health: Mutex::new(ScanHealth::default()),
            auth_limiter: RateLimiter::new(100, 60),
            federated_limiter: RateLimiter::new(100, 60),
            session_ttl_secs: 30 * 24 * 60 * 60,
            series_inflight: KeyedLocks::default(),
            chapters_inflight: KeyedLocks::default(),
            cover_crawl_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            catalogue_cover_phash: false,
            ext_icons_dir: std::path::PathBuf::new(),
        });
        let s = build_schema(state, false);
        // A configured admin name is reserved (case-insensitive) — open
        // registration cannot squat it or self-elevate.
        let r = exec(
            &s,
            r#"mutation { register(input:{username:"Admin", email:"x@y.com", password:"password123"}) { token } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "This username is reserved.");
        // A normal name registers fine and is NOT admin.
        let r = exec(
            &s,
            r#"mutation { register(input:{username:"alice", email:"a@y.com", password:"password123"}) { user { isAdmin } } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        assert!(data_json(&r).contains("\"isAdmin\":false"));
    }

    #[tokio::test]
    async fn expired_session_token_does_not_resolve() {
        let s = setup().await;
        // A live session resolves.
        let r = exec(
            &s,
            r#"{ session { user { username } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(data_json(&r).contains("\"username\":\"bob\""));
        // The seeded expired token (expires_at 2020-02-01) must resolve to null.
        let r = exec(
            &s,
            r#"{ session { user { username } } }"#,
            Some("expiredtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        assert_eq!(data_json(&r), "{\"session\":null}");
        // An expired token is also treated as anonymous for auth-gated mutations.
        let r = exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("expiredtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "Not authenticated");
    }

    async fn seed_canonical(pool: &SqlitePool, md_id: &str, title: &str, nsfw: bool, ch: &str) {
        let input = crate::catalog::WorkInput {
            primary_title: Some(title.to_string()),
            is_nsfw: nsfw,
            ..Default::default()
        };
        crate::catalog::upsert_work_from_mangadex(pool, md_id, &input)
            .await
            .unwrap();
        let ssid = crate::catalog::find_source_series_id(pool, "mangadex", "mangadex", md_id)
            .await
            .unwrap()
            .unwrap();
        crate::catalog::upsert_chapter(
            pool,
            &ssid,
            &crate::catalog::ChapterInput {
                external_id: format!("{md_id}-{ch}"),
                number: Some(ch.to_string()),
                lang: Some("en".into()),
                published_at: Some(format!("2026-07-0{ch}T00:00:00Z")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    /// REGRESSION: `work_redirect` was followed by `canonicalSeries` ALONE. The reader
    /// opens a canonical series page by firing `canonicalSeries`, `canonicalChapters`,
    /// `aggregatedChapters` and `workSources` in parallel, every one of them with the
    /// `w_` id from the URL — so a bookmark to a merged-away work rendered a title and a
    /// cover with no chapters and no translator, which is worse than the clean 404 the
    /// redirect was added to remove. All four must follow it.
    #[tokio::test]
    async fn a_merged_away_work_id_redirects_on_every_canonical_resolver() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-keep", "Surviving Work", false, "1").await;
        seed_canonical(&pool, "md-gone", "Folded Work", false, "2").await;
        let id_of = |key: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT work_id FROM source_series WHERE source_key = ?",
                )
                .bind(key)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let survivor = id_of("md-keep").await;
        let retired = id_of("md-gone").await;
        catalog::merge_works_ex(&pool, None, &retired, &survivor)
            .await
            .unwrap();
        // Precondition: the id really is gone from `work`, and the redirect exists.
        let gone: Option<String> = sqlx::query_scalar("SELECT id FROM work WHERE id = ?")
            .bind(&retired)
            .fetch_optional(&pool)
            .await
            .unwrap();
        assert!(gone.is_none(), "merge must delete the losing work row");

        // canonicalSeries already did this — it is asserted here as the baseline the
        // other three have to match.
        let r = exec(
            &s,
            &format!(r#"{{ canonicalSeries(workId: "{retired}") {{ id title }} }}"#),
            None,
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "canonicalSeries: {:?}", r.errors);
        assert_eq!(
            r.data.into_json().unwrap()["canonicalSeries"]["id"],
            serde_json::json!(survivor),
            "the survivor's id is returned so a stale bookmark self-corrects"
        );

        let r = exec(
            &s,
            &format!(r#"{{ canonicalChapters(workId: "{retired}") {{ id number }} }}"#),
            None,
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "canonicalChapters: {:?}", r.errors);
        assert!(
            !r.data.into_json().unwrap()["canonicalChapters"]
                .as_array()
                .unwrap()
                .is_empty(),
            "canonicalChapters must serve the survivor's chapters, not 'No such work'"
        );

        let r = exec(
            &s,
            &format!(r#"{{ aggregatedChapters(workId: "{retired}") {{ number }} }}"#),
            None,
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "aggregatedChapters: {:?}", r.errors);
        assert!(
            !r.data.into_json().unwrap()["aggregatedChapters"]
                .as_array()
                .unwrap()
                .is_empty(),
            "aggregatedChapters must follow the redirect too"
        );

        let r = exec(
            &s,
            &format!(r#"{{ workSources(workId: "{retired}") {{ sourceKey }} }}"#),
            None,
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "workSources: {:?}", r.errors);
        assert!(
            !r.data.into_json().unwrap()["workSources"]
                .as_array()
                .unwrap()
                .is_empty(),
            "workSources must resolve the survivor's mappings, not an empty list"
        );

        // A genuinely unknown id is still a clean not-found — the redirect must not turn
        // every miss into something else.
        let r = exec(
            &s,
            r#"{ canonicalChapters(workId: "w_nope") { id } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "No such work");
        let r = exec(
            &s,
            r#"{ workSources(workId: "w_nope") { sourceKey } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        assert!(r.data.into_json().unwrap()["workSources"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    /// PRIVILEGE TRACE for the `includeNsfw` escape hatch. It is the one argument on
    /// these two resolvers that can widen what a caller sees, so every principal is
    /// asserted explicitly: it must grant NOTHING to an anonymous or ordinary
    /// signed-in viewer, and must not fire for an admin who did not ask for it.
    #[tokio::test]
    async fn include_nsfw_is_honoured_only_for_admins() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-safe", "Safe Work", false, "2").await;
        seed_canonical(&pool, "md-nsfw", "Spicy Work", true, "1").await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();
        crate::catalog::refresh_work_fts(&pool).await.unwrap();

        // (query, token, must the NSFW work be visible?)
        let cases: [(&str, Option<&str>, bool); 10] = [
            // --- anonymous: the argument is ignored outright ---
            (
                r#"{ canonicalUpdates(includeNsfw: true) { title } }"#,
                None,
                false,
            ),
            (
                r#"{ search(query: "Work", includeNsfw: true) { items { title } } }"#,
                None,
                false,
            ),
            // --- ordinary signed-in viewer (show_nsfw = false): also ignored ---
            (
                r#"{ canonicalUpdates(includeNsfw: true) { title } }"#,
                Some("bobtok"),
                false,
            ),
            (
                r#"{ search(query: "Work", includeNsfw: true) { items { title } } }"#,
                Some("bobtok"),
                false,
            ),
            // --- admin who did NOT ask: their own show_nsfw = false still wins ---
            (r#"{ canonicalUpdates { title } }"#, Some("admintok"), false),
            (
                r#"{ search(query: "Work") { items { title } } }"#,
                Some("admintok"),
                false,
            ),
            // --- admin who explicitly passed false: same ---
            (
                r#"{ canonicalUpdates(includeNsfw: false) { title } }"#,
                Some("admintok"),
                false,
            ),
            (
                r#"{ search(query: "Work", includeNsfw: false) { items { title } } }"#,
                Some("admintok"),
                false,
            ),
            // --- admin who asked: the console can finally see mis-flagged works ---
            (
                r#"{ canonicalUpdates(includeNsfw: true) { title } }"#,
                Some("admintok"),
                true,
            ),
            (
                r#"{ search(query: "Work", includeNsfw: true) { items { title } } }"#,
                Some("admintok"),
                true,
            ),
        ];
        for (q, tok, expect_nsfw) in cases {
            let r = exec(&s, q, tok, "1.1.1.1").await;
            assert!(r.errors.is_empty(), "{q} as {tok:?}: {:?}", r.errors);
            let json = data_json(&r);
            assert!(
                json.contains("Safe Work"),
                "{q} as {tok:?} lost the safe work: {json}"
            );
            assert_eq!(
                json.contains("Spicy Work"),
                expect_nsfw,
                "{q} as {tok:?}: NSFW visibility is wrong: {json}"
            );
        }

        // A banned/expired token is not a user at all, so it cannot borrow the hatch.
        let r = exec(
            &s,
            r#"{ canonicalUpdates(includeNsfw: true) { title } }"#,
            Some("not-a-real-token"),
            "1.1.1.1",
        )
        .await;
        assert!(
            !data_json(&r).contains("Spicy Work"),
            "an unrecognised token must resolve to anonymous"
        );
    }

    #[tokio::test]
    async fn canonical_updates_filters_nsfw_by_preference() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-safe", "Safe Work", false, "2").await;
        seed_canonical(&pool, "md-nsfw", "Spicy Work", true, "1").await;
        // canonicalUpdates now reads the materialized feed_updates table, so build it
        // from the seeded chapters before querying (in production the mangadex sync and
        // a boot task do this).
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        // Default (hidden): only the safe work; newest chapter first.
        let r = exec(
            &s,
            r#"{ canonicalUpdates { title latestChapter isNsfw } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let json = data_json(&r);
        assert!(json.contains("Safe Work"), "{json}");
        assert!(
            !json.contains("Spicy Work"),
            "nsfw work must be hidden: {json}"
        );

        // Opt in → both appear.
        exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let r = exec(
            &s,
            r#"{ canonicalUpdates { title } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let json = data_json(&r);
        assert!(
            json.contains("Safe Work") && json.contains("Spicy Work"),
            "{json}"
        );
    }

    /// The refresh must exclude chapters whose `published_at` is in the future —
    /// MangaDex uses far-future dates for scheduled releases, and they were filling the
    /// feed's first pages with unpublished content.
    #[tokio::test]
    async fn feed_updates_excludes_far_future_chapters() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-real", "Released Work", false, "1").await;

        // A work whose only chapter is scheduled for 2037.
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-future",
            &crate::catalog::WorkInput {
                primary_title: Some("Scheduled Work".into()),
                is_nsfw: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let ssid =
            crate::catalog::find_source_series_id(&pool, "mangadex", "mangadex", "md-future")
                .await
                .unwrap()
                .unwrap();
        crate::catalog::upsert_chapter(
            &pool,
            &ssid,
            &crate::catalog::ChapterInput {
                external_id: "md-future-1".into(),
                number: Some("1".into()),
                lang: Some("en".into()),
                published_at: Some("2037-12-31T15:00:00+00:00".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let r = exec(
            &s,
            r#"{ canonicalUpdates { title latestAt } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let json = data_json(&r);
        assert!(json.contains("Released Work"), "{json}");
        assert!(
            !json.contains("Scheduled Work"),
            "far-future scheduled chapter must not enter the feed: {json}"
        );
    }

    // ---- updatesFeed / feed_series_updates (migration 0064) ------------------
    //
    // The merged Updates feed. Every test below builds the table through the real
    // `refresh_feed_updates` (which now refreshes both feed tables), so the SQL that
    // ships is the SQL under test.

    /// Attach a Suwayomi source series to a canonical work and give it the two columns
    /// that make it a scanner-half feed member: a real upstream release time on
    /// `suwayomi_series` and a detection stamp on `series_scan_state`.
    ///
    /// `latest_ms` is epoch-millis TEXT (migration 0050's encoding) or `None` for a series
    /// with no datable chapter — which the feed EXCLUDES rather than sorting last.
    async fn seed_suwayomi_half(
        pool: &SqlitePool,
        work_id: &str,
        id: i64,
        title: &str,
        latest_ms: Option<&str>,
        detected_at: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO suwayomi_series \
               (id, title, thumbnail_url, status, source_id, chapter_count, in_library, \
                latest_chapter_at, updated_at) \
             VALUES (?, ?, '/thumb.png', 'ONGOING', 'src', 42, 1, ?, '2026-07-15T00:00:00+00:00')",
        )
        .bind(id)
        .bind(title)
        .bind(latest_ms)
        .execute(pool)
        .await
        .unwrap();
        if let Some(d) = detected_at {
            sqlx::query(
                "INSERT INTO series_scan_state \
                   (series_id, avg_interval_hours, known_chapter_count, last_new_chapter_at, updated_at) \
                 VALUES (?, 0, 5, ?, '2026-07-15T00:00:00+00:00')",
            )
            .bind(id.to_string())
            .bind(d)
            .execute(pool)
            .await
            .unwrap();
        }
        crate::catalog::upsert_source_series(
            pool,
            work_id,
            "suwayomi",
            "src",
            &id.to_string(),
            None,
            false,
        )
        .await
        .unwrap();
    }

    /// The `w_` id of the work anchored to a MangaDex key.
    async fn work_id_of(pool: &SqlitePool, md_id: &str) -> String {
        sqlx::query_scalar::<_, String>(
            "SELECT work_id FROM source_series WHERE source_type = 'mangadex' AND source_key = ?",
        )
        .bind(md_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// THE reason this feed is materialized: a series present in BOTH halves must be ONE
    /// row, carrying the newer real release time, the scanner's detection stamp, and the
    /// canonical `w_` id to open.
    ///
    /// The reader used to merge page 1 of `updates` with page 1 of `canonicalUpdates` and
    /// dedupe by lowercased TITLE. That dedupe is why nothing could be paged: it removed
    /// rows AFTER both pages arrived, so pages under-filled and `total`/`hasNextPage`
    /// became fiction. Here the dedupe is by work IDENTITY and happens once, at refresh.
    #[tokio::test]
    async fn updates_feed_folds_both_halves_into_one_row_per_work() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-both", "Two Identities", false, "3").await;
        let wid = work_id_of(&pool, "md-both").await;
        // The Suwayomi source of the SAME work, with a NEWER release (2026-07-25) than the
        // mirror's chapter 3 (2026-07-03) and the only detection stamp in the pair.
        seed_suwayomi_half(
            &pool,
            &wid,
            77,
            "Two Identities",
            Some("1784937600000"), // 2026-07-25T00:00:00Z
            Some("2026-07-25T09:00:00+00:00"),
        )
        .await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let r = exec(
            &s,
            r#"{ updatesFeed { total items { id workId title releasedAt detectedAt } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "updatesFeed failed: {:?}", r.errors);
        let d = r.data.into_json().unwrap();
        let items = d["updatesFeed"]["items"].as_array().unwrap();
        assert_eq!(
            items.len(),
            1,
            "the two identities must collapse to one row: {items:?}"
        );
        assert_eq!(d["updatesFeed"]["total"], serde_json::json!(1));
        assert_eq!(items[0]["workId"], serde_json::json!(wid));
        assert_eq!(
            items[0]["id"],
            serde_json::json!(wid),
            "a MangaDex-anchored work must open on the canonical path, not the numeric one"
        );
        // The max-merge picked the scanner half's newer release...
        assert!(
            items[0]["releasedAt"]
                .as_str()
                .unwrap()
                .starts_with("2026-07-25"),
            "released_at must be the NEWER of the two halves: {items:?}"
        );
        // ...and kept the detection stamp, which only the scanner half has.
        assert!(
            items[0]["detectedAt"]
                .as_str()
                .unwrap()
                .starts_with("2026-07-25"),
            "detected_at must survive the fold: {items:?}"
        );
    }

    /// A Suwayomi-only work carries its NUMERIC id, because `canonicalSeries` rejects a
    /// work with no MangaDex anchor outright — a single-id scheme would produce cards that
    /// 404 on click. And a series with no datable chapter is not an "update" at all: it is
    /// excluded, so every counted row is also a placeable row and the pager's arithmetic
    /// stays honest.
    #[tokio::test]
    async fn updates_feed_reader_id_and_undated_exclusion() {
        let (s, pool) = setup_full(100).await;
        let dated = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Suwayomi Only".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        seed_suwayomi_half(
            &pool,
            &dated,
            11,
            "Suwayomi Only",
            Some("1784937600000"),
            Some("2026-07-25T09:00:00+00:00"),
        )
        .await;
        // Detected, in library — but no upstream release time we can place it by.
        let undated = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Undated Series".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        seed_suwayomi_half(
            &pool,
            &undated,
            12,
            "Undated Series",
            None,
            Some("2026-07-26T09:00:00+00:00"),
        )
        .await;
        // Dated and in library, but our scanner has never detected a new chapter.
        let undetected = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Never Detected".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        seed_suwayomi_half(
            &pool,
            &undetected,
            13,
            "Never Detected",
            Some("1784937600000"),
            None,
        )
        .await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let r = exec(
            &s,
            r#"{ updatesFeed { total items { id workId title chapterCount } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "updatesFeed failed: {:?}", r.errors);
        let d = r.data.into_json().unwrap();
        let items = d["updatesFeed"]["items"].as_array().unwrap();
        let titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
        assert_eq!(
            titles,
            vec!["Suwayomi Only"],
            "only the dated + detected series is an update: {items:?}"
        );
        assert_eq!(d["updatesFeed"]["total"], serde_json::json!(1));
        assert_eq!(
            items[0]["id"],
            serde_json::json!("11"),
            "a work with no MangaDex anchor must open on the Suwayomi path"
        );
        assert_eq!(items[0]["workId"], serde_json::json!(dated));
        assert_eq!(
            items[0]["chapterCount"],
            serde_json::json!(42),
            "the scanner half labels with the chapter COUNT, as it did before the merge"
        );
    }

    /// Page boundaries: disjoint id sets, non-increasing release times ACROSS the
    /// boundary, `total` equal to a full walk, and `hasNextPage` derived from `total`
    /// rather than from a short page.
    ///
    /// This is the property approach (b) — "page one feed and splice the other in" —
    /// cannot have: a row whose release time falls between rows 20 and 21 of the driving
    /// feed either disappears or appears on BOTH pages, and a duplicate `{#each}` key
    /// throws `each_key_duplicate` in production, killing the page.
    #[tokio::test]
    async fn updates_feed_pages_are_disjoint_and_total_matches_a_full_walk() {
        let (s, pool) = setup_full(100).await;
        // 25 works split across the two halves, every one with a DISTINCT release time.
        // The mirror rows land on 2026-07-01..13 and the scanner rows on 2026-07-04..15, so
        // the two halves genuinely INTERLEAVE — the merged order is not "all of one half
        // then all of the other", which is what makes the disjointness assertion meaningful.
        for i in 1..=13 {
            seed_canonical(
                &pool,
                &format!("md-{i}"),
                &format!("Mirror {i}"),
                false,
                "1",
            )
            .await;
            // Push each mirror row to a distinct release time.
            sqlx::query("UPDATE chapter SET published_at = ? WHERE external_id = ?")
                .bind(format!("2026-07-{:02}T00:00:00+00:00", i))
                .bind(format!("md-{i}-1"))
                .execute(&pool)
                .await
                .unwrap();
        }
        for i in 1..=12 {
            let w = crate::catalog::create_work(
                &pool,
                &crate::catalog::WorkInput {
                    primary_title: Some(format!("Scanner {i}")),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            // 2026-07-04 .. 2026-07-15, interleaved with the mirror rows above.
            let ms = 1_783_036_800_000_i64 + (i as i64) * 86_400_000;
            seed_suwayomi_half(
                &pool,
                &w,
                100 + i as i64,
                &format!("Scanner {i}"),
                Some(&ms.to_string()),
                Some("2026-07-26T00:00:00+00:00"),
            )
            .await;
        }
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let fetch = |p: i32| {
            let s = &s;
            async move {
                let q = format!(
                    "{{ updatesFeed(page: {p}) {{ page total hasNextPage \
                       items {{ id releasedAt }} }} }}"
                );
                let r = exec(s, &q, Some("bobtok"), "1.1.1.1").await;
                assert!(r.errors.is_empty(), "page {p}: {:?}", r.errors);
                r.data.into_json().unwrap()["updatesFeed"].clone()
            }
        };
        let p1 = fetch(1).await;
        let p2 = fetch(2).await;
        assert_eq!(p1["total"], serde_json::json!(25));
        assert_eq!(p2["total"], serde_json::json!(25), "total must be stable");
        assert_eq!(p1["page"], serde_json::json!(1), "the page is echoed back");
        assert_eq!(p1["hasNextPage"], serde_json::json!(true));
        assert_eq!(
            p2["hasNextPage"],
            serde_json::json!(false),
            "25 rows at 20/page is exactly two pages"
        );

        let ids = |v: &serde_json::Value| -> Vec<String> {
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["id"].as_str().unwrap().to_string())
                .collect()
        };
        let times = |v: &serde_json::Value| -> Vec<String> {
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["releasedAt"].as_str().unwrap().to_string())
                .collect()
        };
        let (i1, i2) = (ids(&p1), ids(&p2));
        assert_eq!(i1.len(), 20, "a full page must be full");
        assert_eq!(i2.len(), 5);
        let walked: std::collections::HashSet<&String> = i1.iter().chain(i2.iter()).collect();
        assert_eq!(
            walked.len(),
            25,
            "walking every page must yield exactly `total` DISTINCT rows — no row skipped, \
             none emitted twice"
        );
        // Monotonic non-increasing within each page and across the boundary — the visible
        // clock, which is the whole point of the shared sort key.
        let all: Vec<String> = times(&p1).into_iter().chain(times(&p2)).collect();
        let mut sorted = all.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(
            all, sorted,
            "release times must descend within AND across the page boundary"
        );
    }

    /// Two rows sharing a release time to the millisecond must still have a total order,
    /// or LIMIT/OFFSET repeats or skips one of them at a page boundary. Production has 34
    /// such groups covering 143 rows, so this is a live hazard, not a hypothetical.
    #[tokio::test]
    async fn updates_feed_tiebreaks_on_work_id_across_a_page_boundary() {
        let (s, pool) = setup_full(100).await;
        // 21 works, ALL released at the same instant → the only order is the tiebreaker.
        for i in 1..=21 {
            let w = crate::catalog::create_work(
                &pool,
                &crate::catalog::WorkInput {
                    primary_title: Some(format!("Tied {i}")),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            seed_suwayomi_half(
                &pool,
                &w,
                200 + i as i64,
                &format!("Tied {i}"),
                Some("1784937600000"),
                Some("2026-07-26T00:00:00+00:00"),
            )
            .await;
        }
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let mut seen: Vec<String> = Vec::new();
        for p in 1..=2 {
            let q = format!("{{ updatesFeed(page: {p}) {{ items {{ workId }} }} }}");
            let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
            assert!(r.errors.is_empty(), "page {p}: {:?}", r.errors);
            for it in r.data.into_json().unwrap()["updatesFeed"]["items"]
                .as_array()
                .unwrap()
            {
                seen.push(it["workId"].as_str().unwrap().to_string());
            }
        }
        assert_eq!(seen.len(), 21, "both pages together must return every row");
        let distinct: std::collections::HashSet<&String> = seen.iter().collect();
        assert_eq!(
            distinct.len(),
            21,
            "a tie without a tiebreaker repeats or drops rows across the boundary: {seen:?}"
        );
        // `work_id DESC`, matching canonical_updates, so the order is a refinement of it.
        let mut expect = seen.clone();
        expect.sort_by(|a, b| b.cmp(a));
        assert_eq!(seen, expect, "the tiebreaker must be work_id DESCENDING");
    }

    /// NSFW is filtered in SQL, so `total` and `hasNextPage` count only rows the viewer
    /// can see — no page that under-fills yet claims another page exists.
    #[tokio::test]
    async fn updates_feed_total_and_items_follow_the_nsfw_preference() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-safe", "Safe Work", false, "2").await;
        seed_canonical(&pool, "md-nsfw", "Spicy Work", true, "1").await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let ask = |tok: Option<&'static str>| {
            let s = &s;
            async move {
                let r = exec(
                    s,
                    r#"{ updatesFeed { total items { title isNsfw } } }"#,
                    tok,
                    "1.1.1.1",
                )
                .await;
                assert!(r.errors.is_empty(), "updatesFeed failed: {:?}", r.errors);
                r.data.into_json().unwrap()["updatesFeed"].clone()
            }
        };
        // Anonymous: safe only, and `total` says so.
        let anon = ask(None).await;
        assert_eq!(anon["total"], serde_json::json!(1));
        let json = anon.to_string();
        assert!(json.contains("Safe Work"), "{json}");
        assert!(!json.contains("Spicy Work"), "nsfw must be hidden: {json}");

        // Opted in: both, and `total` grows with them.
        exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let opted = ask(Some("bobtok")).await;
        assert_eq!(
            opted["total"],
            serde_json::json!(2),
            "total must describe the viewer's own slice, not everyone's"
        );
        let json = opted.to_string();
        assert!(
            json.contains("Safe Work") && json.contains("Spicy Work"),
            "{json}"
        );
    }

    /// The format facet is a SERVER filter, which is the only reason `comic_type` is
    /// materialized: filtering a 20-row page client-side would narrow the page rather than
    /// the feed, and `total` would keep describing the unfiltered set.
    #[tokio::test]
    async fn updates_feed_type_filter_narrows_the_whole_feed() {
        let (s, pool) = setup_full(100).await;
        for (md, title, lang) in [
            ("md-jp", "Japanese Work", "ja"),
            ("md-kr", "Korean Work", "ko"),
            ("md-cn", "Chinese Work", "zh"),
        ] {
            crate::catalog::upsert_work_from_mangadex(
                &pool,
                md,
                &crate::catalog::WorkInput {
                    primary_title: Some(title.to_string()),
                    original_language: Some(lang.to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let ssid = crate::catalog::find_source_series_id(&pool, "mangadex", "mangadex", md)
                .await
                .unwrap()
                .unwrap();
            crate::catalog::upsert_chapter(
                &pool,
                &ssid,
                &crate::catalog::ChapterInput {
                    external_id: format!("{md}-1"),
                    number: Some("1".into()),
                    lang: Some("en".into()),
                    published_at: Some("2026-07-01T00:00:00Z".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        for (arg, want_title, want_type) in [
            ("MANGA", "Japanese Work", "MANGA"),
            ("MANHWA", "Korean Work", "MANHWA"),
            ("MANHUA", "Chinese Work", "MANHUA"),
            // WEBTOON folds into MANHWA — the stored word is collapsed the way every
            // reader surface renders it, so asking for WEBTOON returns the manhwa set
            // rather than nothing.
            ("WEBTOON", "Korean Work", "MANHWA"),
        ] {
            let q = format!("{{ updatesFeed(type: {arg}) {{ total items {{ title type }} }} }}");
            let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
            assert!(r.errors.is_empty(), "type {arg}: {:?}", r.errors);
            let d = r.data.into_json().unwrap();
            assert_eq!(
                d["updatesFeed"]["total"],
                serde_json::json!(1),
                "type {arg}: total must describe the FILTERED feed"
            );
            let items = d["updatesFeed"]["items"].as_array().unwrap();
            assert_eq!(
                items[0]["title"],
                serde_json::json!(want_title),
                "type {arg}"
            );
            assert_eq!(items[0]["type"], serde_json::json!(want_type), "type {arg}");
        }

        // Unfiltered: all three, so the filter really is narrowing and not just missing.
        let r = exec(
            &s,
            r#"{ updatesFeed { total } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(
            r.data.into_json().unwrap()["updatesFeed"]["total"],
            serde_json::json!(3)
        );
    }

    /// An over-range page ECHOES the page it was asked for and returns no rows, rather
    /// than clamping — the reader repairs it by navigating to the last real page (the same
    /// contract the admin review queue relies on). Page 0 / negative pages clamp to 1 so a
    /// hand-edited link can never produce a negative OFFSET.
    #[tokio::test]
    async fn updates_feed_over_range_and_nonsense_pages_are_safe() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-one", "Only Work", false, "1").await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let r = exec(
            &s,
            r#"{ updatesFeed(page: 9999) { page total hasNextPage items { title } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let d = r.data.into_json().unwrap();
        assert_eq!(d["updatesFeed"]["page"], serde_json::json!(9999));
        assert_eq!(d["updatesFeed"]["total"], serde_json::json!(1));
        assert_eq!(d["updatesFeed"]["hasNextPage"], serde_json::json!(false));
        assert!(d["updatesFeed"]["items"].as_array().unwrap().is_empty());

        for p in ["0", "-5"] {
            let q = format!("{{ updatesFeed(page: {p}) {{ page items {{ title }} }} }}");
            let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
            assert!(r.errors.is_empty(), "page {p}: {:?}", r.errors);
            let d = r.data.into_json().unwrap();
            assert_eq!(
                d["updatesFeed"]["items"].as_array().unwrap().len(),
                1,
                "page {p} must clamp to page 1, not run a negative OFFSET"
            );
            assert_eq!(d["updatesFeed"]["page"], serde_json::json!(1), "page {p}");
        }
    }

    /// The two halves store their clocks in INCOMPATIBLE TEXT encodings —
    /// `feed_updates.latest_at` is ISO-8601, `suwayomi_series.latest_chapter_at` is
    /// 13-digit epoch-millis TEXT. Compared as text under BINARY collation every '2…' ISO
    /// string sorts above every '1…' millis string, so a TEXT sort key would have put the
    /// whole mirror half above the whole scanner half and called it chronological. The
    /// column is epoch millis for exactly this reason; this test fails if it regresses.
    #[tokio::test]
    async fn updates_feed_orders_the_two_encodings_on_one_real_clock() {
        let (s, pool) = setup_full(100).await;
        // Mirror row: ISO '2026-07-05…'. Its text form starts with '2'.
        seed_canonical(&pool, "md-mirror", "Mirror Row", false, "5").await;
        // Scanner row: millis '1784937600000' = 2026-07-25, i.e. GENUINELY NEWER than the
        // mirror row's 2026-07-05 — but its text form starts with '1' and would therefore
        // sort LAST under a text comparison against the mirror's '2026-…'.
        let w = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Scanner Row".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        seed_suwayomi_half(
            &pool,
            &w,
            301,
            "Scanner Row",
            Some("1784937600000"), // 2026-07-25
            Some("2026-07-25T00:00:00+00:00"),
        )
        .await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let r = exec(
            &s,
            r#"{ updatesFeed { items { title releasedAt } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let d = r.data.into_json().unwrap();
        let items = d["updatesFeed"]["items"].as_array().unwrap();
        let titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
        assert_eq!(
            titles,
            vec!["Scanner Row", "Mirror Row"],
            "the genuinely newer row must be first regardless of which half it came from"
        );
        // And both halves report ISO on the wire, not one ISO and one epoch-millis blob.
        for it in items {
            let at = it["releasedAt"].as_str().unwrap();
            assert!(
                at.starts_with("2026-07-"),
                "releasedAt must be ISO-8601 on the wire, got {at:?}"
            );
        }
    }

    /// REGRESSION: the rebuild committed its DELETE + INSERTs and only THEN ran the
    /// Rust-side `comic_type` fill. For the whole duration of that fill every row read
    /// `comic_type IS NULL`, and `updatesFeed(type:)` filters on a single equality — so
    /// the reader's format tabs served an EMPTY feed (`total: 0` included) on every
    /// rebuild, at boot and once per catalogue-sync cycle.
    ///
    /// The fix is that the fill now runs INSIDE the rebuild transaction, which this test
    /// pins from the other side: with the phases joined, a fill that fails must take the
    /// whole rebuild down with it and leave the PREVIOUS generation intact. With the
    /// phases split, the new generation was already committed — untyped — before the fill
    /// was even attempted, which is exactly the state the reader could observe.
    ///
    /// The fault is injected by dropping the table the fill reads first. It is a blunt
    /// instrument on purpose: the assertion is about the transaction boundary, not about
    /// any particular way the fill can fail.
    #[tokio::test]
    async fn updates_feed_rebuild_never_commits_an_untyped_generation() {
        let (_s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-gen1", "Generation One", false, "1").await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();
        let gen1 = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT title, comic_type FROM feed_series_updates ORDER BY title",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(gen1.len(), 1);
        assert_eq!(gen1[0].1.as_deref(), Some("MANGA"), "generation 1 is typed");

        // A mirror-half row generation 2 WOULD pick up, so the rebuild has a visible
        // change to make — and then a fill that cannot run.
        seed_canonical(&pool, "md-gen2", "Generation Two", false, "2").await;
        sqlx::query(
            "INSERT INTO feed_updates (work_id, mangadex_id, title, is_nsfw, latest_at) \
             VALUES ((SELECT work_id FROM source_series WHERE source_key = 'md-gen2'), \
                     'md-gen2', 'Generation Two', 0, '2026-07-02T00:00:00+00:00')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DROP TABLE work_tag")
            .execute(&pool)
            .await
            .unwrap();

        let err = crate::catalog::refresh_feed_series_updates(&pool).await;
        assert!(
            err.is_err(),
            "a fill that cannot run must fail the rebuild, not be skipped"
        );
        let after = sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT title, comic_type FROM feed_series_updates ORDER BY title",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            after, gen1,
            "the rebuild must have rolled back to the previous generation — neither the \
             new row nor a comic_type NULL may be visible; a committed generation with \
             comic_type NULL is the bug this test exists for"
        );
    }

    /// Every committed row carries a format, so `updatesFeed(type:)` partitions the feed
    /// rather than sampling it: the three per-type totals must add up to the unfiltered
    /// total. A NULL `comic_type` is invisible to BOTH the filtered page and the filtered
    /// count, so it would silently shrink the facet instead of erroring.
    #[tokio::test]
    async fn updates_feed_type_facets_partition_the_whole_feed() {
        let (s, pool) = setup_full(100).await;
        for (md, title) in [
            ("md-p1", "Plain One"),
            ("md-p2", "Plain Two"),
            ("md-p3", "한국 작품"),
            ("md-p4", "中文作品"),
        ] {
            seed_canonical(&pool, md, title, false, "1").await;
        }
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();
        let untyped: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM feed_series_updates WHERE comic_type IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            untyped, 0,
            "a committed feed row must always carry a format"
        );

        let total_of = |q: String| {
            let s = &s;
            async move {
                let r = exec(s, &q, Some("bobtok"), "1.1.1.1").await;
                assert!(r.errors.is_empty(), "{q}: {:?}", r.errors);
                r.data.into_json().unwrap()["updatesFeed"]["total"]
                    .as_i64()
                    .unwrap()
            }
        };
        let all = total_of("{ updatesFeed { total } }".into()).await;
        let mut sum = 0;
        for t in ["MANGA", "MANHWA", "MANHUA"] {
            sum += total_of(format!("{{ updatesFeed(type: {t}) {{ total }} }}")).await;
        }
        assert_eq!(all, 4);
        assert_eq!(
            sum, all,
            "the three format facets must partition the feed exactly"
        );
    }

    /// The feed stores a COPY of the effective NSFW flag so the resolver can pin
    /// `is_nsfw = 0` as an index prefix. That copy is only rewritten by the periodic
    /// rebuild, so without an explicit resync an admin's "mark NSFW" left the work
    /// visible to opted-out viewers on `updatesFeed` for HOURS — a gap `graphql::updates`
    /// never had, because it evaluates the same COALESCE live. `updatesFeed` supersedes
    /// `updates` as the reader's Updates surface, so the gap had to be closed, not
    /// inherited.
    #[tokio::test]
    async fn updates_feed_honours_an_admin_nsfw_mark_before_the_next_rebuild() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-flip", "Reclassified Work", false, "1").await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();
        let wid = work_id_of(&pool, "md-flip").await;

        let titles = |tok: &'static str| {
            let s = &s;
            async move {
                let r = exec(
                    s,
                    r#"{ updatesFeed { total items { title } } }"#,
                    Some(tok),
                    "1.1.1.1",
                )
                .await;
                assert!(r.errors.is_empty(), "{:?}", r.errors);
                r.data.into_json().unwrap()["updatesFeed"].clone()
            }
        };
        assert_eq!(titles("bobtok").await["total"], serde_json::json!(1));

        // Mark it NSFW through the real admin mutation — no feed rebuild in between.
        let m = format!(
            r#"mutation {{ updateSeriesMetadata(input: {{ seriesId: "{wid}", isNsfw: true }}) {{ id }} }}"#
        );
        let r = exec(&s, &m, Some("admintok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "mark failed: {:?}", r.errors);

        let d = titles("bobtok").await;
        assert_eq!(
            d["total"],
            serde_json::json!(0),
            "an opted-out viewer must not see a work the admin just marked NSFW, and \
             `total` must agree with the empty page"
        );
        assert!(d["items"].as_array().unwrap().is_empty());
    }

    // ---- Browse / search(query: "") over the canonical catalogue (migration 0068) -----
    //
    // Browse used to read `suwayomi_series WHERE in_library = 1` (13,847 of 48,526
    // readable works) with a fixed ORDER BY and no format/status/rating filter. It now
    // pages `feed_series_updates`. Every test below builds the table through the real
    // `refresh_feed_updates`, so the SQL that ships is the SQL under test.

    /// Seed one canonical work with the columns Browse filters and sorts on, plus `chapters`
    /// English chapters so `en_chapter_count` is a real number.
    #[allow(clippy::too_many_arguments)]
    async fn seed_browse_work(
        pool: &SqlitePool,
        md_id: &str,
        title: &str,
        lang: &str,
        status: &str,
        content_rating: &str,
        nsfw: bool,
        chapters: usize,
        tags: &[&str],
    ) -> String {
        let work_id = crate::catalog::upsert_work_from_mangadex(
            pool,
            md_id,
            &crate::catalog::WorkInput {
                primary_title: Some(title.to_string()),
                original_language: Some(lang.to_string()),
                status: Some(status.to_string()),
                content_rating: Some(content_rating.to_string()),
                is_nsfw: nsfw,
                tags: tags.iter().map(|t| t.to_string()).collect(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let ssid = crate::catalog::find_source_series_id(pool, "mangadex", "mangadex", md_id)
            .await
            .unwrap()
            .unwrap();
        for i in 1..=chapters {
            crate::catalog::upsert_chapter(
                pool,
                &ssid,
                &crate::catalog::ChapterInput {
                    external_id: format!("{md_id}-{i}"),
                    number: Some(i.to_string()),
                    lang: Some("en".into()),
                    // Distinct, ordered publish times so `released_at` is deterministic.
                    published_at: Some(format!("2026-07-{:02}T00:00:00Z", (i % 27) + 1)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        work_id
    }

    /// PAGE SIZE. Browse pages at 30 while the rest of the API stays at 20 — the two are
    /// separate constants because `PAGE_SIZE` is shared by ~24 unrelated sites (and asserted
    /// as 20 by their own tests). The grid needs a page that fills a wide viewport; the
    /// admin lists and social feeds do not.
    ///
    /// Both branches of `search` use the SAME size, which is the part that can silently
    /// break: they answer one field, so the client drives them with one pager and one page
    /// number — if they diverged, page 2 of a text search would start 10 rows past where
    /// page 1 ended.
    #[tokio::test]
    async fn browse_pages_at_thirty_while_the_rest_of_the_api_stays_at_twenty() {
        assert_eq!(BROWSE_PAGE_SIZE, 30);
        assert_eq!(PAGE_SIZE, 20, "shared page size must not move");
        let (s, pool) = setup_full(100).await;
        let n = BROWSE_PAGE_SIZE + 5;
        for i in 0..n {
            seed_canonical(
                &pool,
                &format!("md-b{i:03}"),
                &format!("Browse {i:03}"),
                false,
                "1",
            )
            .await;
        }
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();
        // `setup_full` also seeds the two `w_s1`/`w_s2` comment-target works, which have a
        // `source_series` and NO chapter — so they are absent from `feed_series_updates` and
        // PRESENT in `browse_catalogue` (migration 0069). Browse is the whole browsable
        // catalogue now, so they count. That is the behaviour change, not an artefact: before
        // 0069 this total was `n`.
        let total = n + 2;

        let page = |p: i64| {
            let s = &s;
            async move {
                let q = format!(
                    r#"{{ search(query: "", page: {p}, sort: NEWEST) {{ total hasNextPage items {{ id }} }} }}"#
                );
                let r = exec(s, &q, Some("bobtok"), "1.1.1.1").await;
                assert!(r.errors.is_empty(), "page {p}: {:?}", r.errors);
                r.data.into_json().unwrap()["search"].clone()
            }
        };
        let p1 = page(1).await;
        assert_eq!(p1["total"], serde_json::json!(total));
        assert_eq!(
            p1["items"].as_array().unwrap().len() as i64,
            BROWSE_PAGE_SIZE
        );
        assert_eq!(p1["hasNextPage"], serde_json::json!(true));
        let p2 = page(2).await;
        assert_eq!(
            p2["items"].as_array().unwrap().len() as i64,
            total - BROWSE_PAGE_SIZE
        );
        assert_eq!(p2["hasNextPage"], serde_json::json!(false));

        // Pages are DISJOINT and cover the set exactly — the each_key_duplicate trap.
        let mut ids: Vec<String> = [p1, p2]
            .iter()
            .flat_map(|p| {
                p["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["id"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(ids.len() as i64, total);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len() as i64, total, "a work appeared on both pages");

        // The `updatesFeed` page size is untouched at PAGE_SIZE.
        let r = exec(
            &s,
            r#"{ updatesFeed { items { id } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(
            r.data.into_json().unwrap()["updatesFeed"]["items"]
                .as_array()
                .unwrap()
                .len() as i64,
            PAGE_SIZE
        );
    }

    /// `Series.id` MUST be `browse_catalogue.reader_id`, never `work_id`.
    ///
    /// 1,897 of the 115,567 browse rows have no MangaDex anchor, and `canonicalSeries`
    /// rejects those outright (`if work.mangadex_id.is_none() { Err("No such work") }`) — so
    /// returning `work_id` would put 1,897 cards on the grid that 404 the moment they are
    /// clicked. 0064 stores `reader_id` for exactly this reason and 0069 keeps the rule: such
    /// a work is reachable only by its numeric Suwayomi id.
    #[tokio::test]
    async fn browse_id_is_the_reader_id_so_an_unanchored_work_stays_openable() {
        let (s, pool) = setup_full(100).await;
        // A work with NO MangaDex source — only a scanner-detected Suwayomi one.
        let wid = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Suwayomi Only".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        seed_suwayomi_half(
            &pool,
            &wid,
            4242,
            "Suwayomi Only",
            Some("1784937600000"),
            Some("2026-07-25T00:00:00+00:00"),
        )
        .await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let r = exec(
            &s,
            r#"{ search(query: "", sort: NEWEST) { items { id title } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let items = r.data.into_json().unwrap()["search"]["items"].clone();
        assert_eq!(items[0]["title"], serde_json::json!("Suwayomi Only"));
        assert_eq!(
            items[0]["id"],
            serde_json::json!("4242"),
            "an unanchored work must be handed out under its numeric Suwayomi id"
        );

        // And the proof that the alternative would 404: the canonical path refuses the
        // work id for exactly this work.
        let q = format!(r#"{{ canonicalSeries(workId: "{wid}") {{ id }} }}"#);
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert_eq!(first_error(&r), "No such work");
    }

    /// `browse_catalogue` must hold EVERY browsable work — one row per `work` that has at
    /// least one `source_series` — and nothing else.
    ///
    /// The membership rule is the whole contract of migration 0069, and both halves of it are
    /// easy to break in the same edit: a stricter join silently re-shrinks Browse to the works
    /// with chapters (which is what it was), and a missing exclusion puts a card on the grid
    /// that opens nothing on either the canonical or the Suwayomi path (2 such works in
    /// production).
    #[tokio::test]
    async fn browse_catalogue_holds_every_browsable_work_and_excludes_the_sourceless() {
        let (_s, pool) = setup_full(100).await;
        // One of each kind: MangaDex-anchored WITH chapters, MangaDex-anchored WITHOUT,
        // Suwayomi-only, and a work with no source at all.
        seed_canonical(&pool, "md-has", "Has Chapters", false, "1").await;
        seed_browse_work(
            &pool,
            "md-none",
            "No Chapters",
            "ja",
            "ONGOING",
            "safe",
            false,
            0,
            &[],
        )
        .await;
        let suw = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Suwayomi Only".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        seed_suwayomi_half(&pool, &suw, 4242, "Suwayomi Only", None, None).await;
        let orphan = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("No Source At All".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let count = |sql: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        let browsable = count(
            "SELECT COUNT(*) FROM work w WHERE EXISTS \
                 (SELECT 1 FROM source_series ss WHERE ss.work_id = w.id)",
        )
        .await;
        assert_eq!(
            count("SELECT COUNT(*) FROM browse_catalogue").await,
            browsable,
            "one row per work with a source — no more (a sourceless work leaked in) and no \
             fewer (the chapter join came back)"
        );
        // And the excluded work really is the sourceless one.
        let has_orphan: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM browse_catalogue WHERE work_id = ?")
                .bind(&orphan)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            has_orphan, 0,
            "a work with nothing to open must not be a card"
        );
        // The feed remains the strictly smaller set, which is what makes this table exist.
        assert!(
            count("SELECT COUNT(*) FROM feed_series_updates").await < browsable,
            "the updates feed must stay the subset it is defined as"
        );
    }

    /// A chapterless work must be BROWSABLE and its card must OPEN.
    ///
    /// This is the entire user-visible point of migration 0069, and the failure mode it is
    /// guarding is specific: `browse_catalogue.reader_id` is what the grid navigates to, and
    /// `canonicalSeries` hard-rejects a work with no MangaDex anchor — so a chapterless
    /// MangaDex work MUST be handed out as its `w_…` id (which resolves) and a chapterless
    /// Suwayomi-only work as its numeric id (which the canonical path would 404).
    #[tokio::test]
    async fn a_chapterless_work_is_browsable_and_its_card_opens() {
        let (s, pool) = setup_full(100).await;
        // The real-world shape: a licensed series MangaDex has removed every chapter from.
        // Anchored, catalogued, zero chapters.
        let wid = seed_browse_work(
            &pool,
            "md-licensed",
            "Boku no Hero Academia",
            "ja",
            "ONGOING",
            "safe",
            false,
            0,
            &[],
        )
        .await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let r = exec(
            &s,
            r#"{ search(query: "", sort: NEWEST) { total items { id title chapterCount latestChapterAt updatedAt } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let d = r.data.into_json().unwrap()["search"].clone();
        let card = d["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"] == serde_json::json!("Boku no Hero Academia"))
            .expect("a chapterless work must appear in Browse")
            .clone();
        assert_eq!(card["id"], serde_json::json!(wid), "anchored → its `w_` id");
        assert_eq!(card["chapterCount"], serde_json::json!(0));
        assert_eq!(
            card["latestChapterAt"],
            serde_json::json!(""),
            "`latestChapterAt` is documented as EMPTY when nothing is dated — never the epoch"
        );
        let entered: String = sqlx::query_scalar("SELECT created_at FROM work WHERE id = ?")
            .bind(&wid)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            card["updatedAt"],
            serde_json::json!(entered),
            "`updatedAt` is String! and has no empty contract, so it falls back to the work's \
             catalogue-entry time — the last thing we actually know about the work"
        );

        // The click. Anything less than this passing means 67,000 cards that 404.
        let q = format!(r#"{{ canonicalSeries(workId: "{wid}") {{ id title }} }}"#);
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(
            r.errors.is_empty(),
            "a chapterless work's card must open: {:?}",
            r.errors
        );
    }

    /// REGRESSION GUARD, the one that matters most about migration 0069: Browse's move to a
    /// wider table must NOT have leaked chapterless works into the UPDATES feed.
    ///
    /// `feed_series_updates.released_at` is NOT NULL by contract — "a work with no dated
    /// chapter is not an update, so it is EXCLUDED" (0064's header) — and the pager's
    /// arithmetic on `/updates` is only honest if every counted row is a placeable row. 0069
    /// deliberately built a second table instead of widening this one; this test is what says
    /// so in executable form.
    #[tokio::test]
    async fn chapterless_works_stay_out_of_the_updates_feed() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-dated", "Dated Work", false, "1").await;
        seed_browse_work(
            &pool,
            "md-undated",
            "Undated Work",
            "ja",
            "ONGOING",
            "safe",
            false,
            0,
            &[],
        )
        .await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        // The table: present in Browse's, absent from the feed's, and the feed still has no
        // NULL sort key anywhere.
        let one = |sql: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            one("SELECT COUNT(*) FROM browse_catalogue WHERE released_at IS NULL").await,
            3,
            "the undated work plus `setup_full`'s two chapterless comment targets"
        );
        assert_eq!(
            one(
                "SELECT COUNT(*) FROM feed_series_updates \
                 WHERE work_id IN (SELECT work_id FROM browse_catalogue WHERE released_at IS NULL)"
            )
            .await,
            0,
            "a chapterless work must never gain a feed row"
        );

        // And the resolver: `updatesFeed` shows only the dated work.
        let r = exec(
            &s,
            r#"{ updatesFeed { total items { title } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let d = r.data.into_json().unwrap()["updatesFeed"].clone();
        let titles: Vec<&str> = d["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["title"].as_str().unwrap())
            .collect();
        assert_eq!(
            titles,
            ["Dated Work"],
            "the updates feed is unchanged by 0069"
        );
        assert_eq!(d["total"], serde_json::json!(1));
    }

    /// Format / status / content-rating filters narrow the WHOLE catalogue, so `total`
    /// describes the filtered set rather than a slice of it — the thing the old
    /// `suwayomi_series` path could not do at all (it had no column for any of the three).
    #[tokio::test]
    async fn browse_filters_narrow_the_whole_catalogue_and_total_follows() {
        let (s, pool) = setup_full(100).await;
        // ja/ONGOING/safe, ko/COMPLETED/suggestive, zh/ON_HIATUS/erotica(+nsfw).
        seed_browse_work(
            &pool,
            "md-f1",
            "Alpha",
            "ja",
            "ONGOING",
            "safe",
            false,
            3,
            &["Action"],
        )
        .await;
        seed_browse_work(
            &pool,
            "md-f2",
            "Beta",
            "ko",
            "COMPLETED",
            "suggestive",
            false,
            5,
            &["Romance"],
        )
        .await;
        seed_browse_work(
            &pool,
            "md-f3",
            "Gamma",
            "zh",
            "ON_HIATUS",
            "erotica",
            true,
            2,
            &["Action"],
        )
        .await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let ask = |args: String| {
            let s = &s;
            async move {
                let q = format!(r#"{{ search(query: "", {args}) {{ total items {{ title }} }} }}"#);
                let r = exec(s, &q, Some("bobtok"), "1.1.1.1").await;
                assert!(r.errors.is_empty(), "{q}: {:?}", r.errors);
                let d = r.data.into_json().unwrap()["search"].clone();
                let titles: Vec<String> = d["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["title"].as_str().unwrap().to_string())
                    .collect();
                (d["total"].as_i64().unwrap(), titles)
            }
        };
        // bobtok is opted OUT, so Gamma (nsfw) is invisible from the start. The other two
        // rows are `setup_full`'s chapterless `w_s1`/`w_s2` comment-target works: they have a
        // `source_series` and no chapter, so they are absent from `feed_series_updates` and
        // PRESENT in `browse_catalogue` (migration 0069) — before it, this total was 2. Every
        // per-facet assertion below therefore says `hasChapters: true`, which scopes it back
        // to the three works this test seeds rather than re-baselining nine expectations
        // against a shared fixture.
        assert_eq!(ask("sort: NEWEST".into()).await.0, 4);
        // ...and `hasChapters: true` is what recovers the readable-only view.
        assert_eq!(
            ask("hasChapters: true, sort: NEWEST".into()).await,
            (2, vec!["Beta".to_string(), "Alpha".to_string()]),
            "hasChapters must narrow to the works with a known chapter, total included"
        );
        assert_eq!(
            ask("hasChapters: false, sort: NEWEST".into()).await.0,
            2,
            "and its complement is the chapterless half, which is the new part of Browse"
        );
        // Format: `original_language` drives `comic_type`, collapsed at write time.
        assert_eq!(
            ask("types: [MANHWA], hasChapters: true, sort: NEWEST".into())
                .await
                .1,
            ["Beta"]
        );
        assert_eq!(
            ask("types: [MANGA], hasChapters: true, sort: NEWEST".into())
                .await
                .1,
            ["Alpha"]
        );
        // WEBTOON folds into MANHWA rather than returning nothing.
        assert_eq!(
            ask("types: [WEBTOON], hasChapters: true, sort: NEWEST".into())
                .await
                .1,
            ["Beta"]
        );
        // Status, over the NORMALIZED vocabulary. Gamma's upstream `ON_HIATUS` is stored as
        // `HIATUS`, which is the only word the enum can even express — un-normalized it
        // would be unreachable by every filter value.
        assert_eq!(
            ask("status: COMPLETED, hasChapters: true, sort: NEWEST".into())
                .await
                .1,
            ["Beta"]
        );
        assert_eq!(
            ask("status: ONGOING, hasChapters: true, sort: NEWEST".into())
                .await
                .1,
            ["Alpha"]
        );
        let hiatus_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM feed_series_updates WHERE status = 'HIATUS'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(hiatus_rows, 1, "ON_HIATUS must be stored as HIATUS");
        // Content-rating tiers are cumulative, and cannot widen past the NSFW gate: the
        // erotica work stays hidden from this opted-out viewer at EVERY tier.
        assert_eq!(
            ask("contentRating: SAFE, hasChapters: true, sort: NEWEST".into())
                .await
                .1,
            ["Alpha"]
        );
        assert_eq!(
            ask("contentRating: SUGGESTIVE, hasChapters: true, sort: NEWEST".into())
                .await
                .0,
            2
        );
        assert_eq!(
            ask("contentRating: PORNOGRAPHIC, hasChapters: true, sort: NEWEST".into()).await,
            ask("contentRating: SUGGESTIVE, hasChapters: true, sort: NEWEST".into()).await,
            "a rating tier must never widen the viewer's NSFW gate"
        );
        // Opted IN, the erotica work appears — and only then.
        exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(
            ask("contentRating: EROTICA, hasChapters: true, sort: NEWEST".into())
                .await
                .0,
            3
        );
    }

    /// The `chapterCount` a card prints and the key the CHAPTERS sort orders by must be the
    /// SAME expression (`en_chapter_count`), or "sort by chapters" visibly disagrees with
    /// the badge on the card it just ordered.
    #[tokio::test]
    async fn browse_chapter_count_is_the_chapters_sort_key() {
        let (s, pool) = setup_full(100).await;
        for (md, title, n) in [
            ("md-c1", "Few", 2),
            ("md-c2", "Many", 9),
            ("md-c3", "Some", 5),
        ] {
            seed_browse_work(&pool, md, title, "ja", "ONGOING", "safe", false, n, &[]).await;
        }
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();
        let r = exec(
            &s,
            r#"{ search(query: "", sort: CHAPTERS) { items { title chapterCount } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let items = r.data.into_json().unwrap()["search"]["items"].clone();
        let pairs: Vec<(String, i64)> = items
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                (
                    i["title"].as_str().unwrap().to_string(),
                    i["chapterCount"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            [
                ("Many".to_string(), 9),
                ("Some".to_string(), 5),
                ("Few".to_string(), 2),
                // `setup_full`'s two chapterless comment-target works, which Browse carries
                // as of migration 0069. They print 0 and sort LAST under `en_chapter_count
                // DESC` — where a work we know no chapter for belongs — and the reader turns
                // that 0 into "No chapters yet" rather than "Ch. 0" (see the browse page's
                // `cardSub`). `w_s2` before `w_s1` is the mandatory `work_id DESC` tiebreak.
                ("Fixture s2".to_string(), 0),
                ("Fixture s1".to_string(), 0),
            ],
            "the printed count must be the very key the order was computed from"
        );
    }

    /// The feed stores a COPY of the effective NSFW flag so Browse can pin `is_nsfw = 0` as
    /// an index prefix. `resync_feed_nsfw` rewrites that copy on every admin mark; without
    /// it, an admin's "mark NSFW" would leave the work on Browse — and counted in `total` —
    /// for up to a full sync cycle. This is the Browse half of the guarantee
    /// `updates_feed_honours_an_admin_nsfw_mark_before_the_next_rebuild` pins for Updates,
    /// and it is what `series_cache`'s override test used to cover through
    /// `search_catalogue`'s own `total`.
    #[tokio::test]
    async fn browse_total_and_page_follow_an_admin_nsfw_mark() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-mark", "Reclassified", false, "1").await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();
        let wid = work_id_of(&pool, "md-mark").await;

        let browse = || {
            let s = &s;
            async move {
                let r = exec(
                    s,
                    r#"{ search(query: "", sort: NEWEST) { total items { title } } }"#,
                    Some("bobtok"),
                    "1.1.1.1",
                )
                .await;
                assert!(r.errors.is_empty(), "{:?}", r.errors);
                r.data.into_json().unwrap()["search"].clone()
            }
        };
        // 3 = the marked work plus `setup_full`'s two chapterless comment-target works, which
        // Browse carries as of migration 0069.
        assert_eq!(browse().await["total"], serde_json::json!(3));

        let m = format!(
            r#"mutation {{ updateSeriesMetadata(input: {{ seriesId: "{wid}", isNsfw: true }}) {{ id }} }}"#
        );
        let r = exec(&s, &m, Some("admintok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "mark failed: {:?}", r.errors);

        let d = browse().await;
        assert_eq!(
            d["total"],
            serde_json::json!(2),
            "an opted-out viewer must not see a work the admin just marked NSFW, and \
             `total` must agree with the shortened page"
        );
        let titles: Vec<&str> = d["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["title"].as_str().unwrap())
            .collect();
        assert!(
            !titles.contains(&"Reclassified"),
            "the marked work is still on the page: {titles:?} — `resync_feed_nsfw` must \
             rewrite `browse_catalogue.is_nsfw`, not just the two feed tables"
        );
    }

    /// The one above covers the path that DOES resync the copy. This covers the path that
    /// does not.
    ///
    /// `catalog::mark_work_nsfw` escalates `work.is_nsfw` from a source-level signal on the
    /// ingest/link path. Until 2026-07-27 neither of its callers followed up with
    /// `resync_feed_nsfw`, so `browse_catalogue.is_nsfw` stayed 0 until the next rebuild —
    /// one catalogue-sync interval, 6h by default — and Browse gates on that copy.
    ///
    /// The Rust backstop was supposed to catch exactly this, but it re-tested
    /// `Series.is_nsfw`, which `map_browse_rows` had copied FROM the same materialized
    /// column the WHERE had already pinned: a tautology that could not drop a row. So the
    /// two defences failed together and an opted-out viewer was served the work.
    ///
    /// This drives the copy stale DIRECTLY rather than through the ingest path, so it pins
    /// the invariant itself — a stale copy must never surface the work — independently of
    /// which callers remember to resync.
    #[tokio::test]
    async fn a_stale_nsfw_copy_still_cannot_surface_the_work_on_browse() {
        let (s, pool) = setup_full(100).await;
        seed_canonical(&pool, "md-stale", "Stale Copy", false, "1").await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();
        let wid = work_id_of(&pool, "md-stale").await;

        // The authoritative column says NSFW; the materialized copy still says SFW. This is
        // precisely the state `mark_work_nsfw` used to leave behind.
        sqlx::query("UPDATE work SET is_nsfw = 1 WHERE id = ?")
            .bind(&wid)
            .execute(&pool)
            .await
            .unwrap();
        let stale: i64 =
            sqlx::query_scalar("SELECT is_nsfw FROM browse_catalogue WHERE work_id = ?")
                .bind(&wid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            stale, 0,
            "the copy must still be stale for this test to mean anything"
        );

        let r = exec(
            &s,
            r#"{ search(query: "", sort: NEWEST) { total items { title } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let d = r.data.into_json().unwrap()["search"].clone();
        let titles: Vec<&str> = d["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["title"].as_str().unwrap())
            .collect();
        assert!(
            !titles.contains(&"Stale Copy"),
            "an opted-out viewer was served a work whose LIVE flag is NSFW: {titles:?} — the \
             backstop must read `work`, not the row's own materialized copy"
        );
        // `total` is deliberately NOT asserted here. It comes from the SQL COUNT, which
        // shares the page query's WHERE and therefore also reads the stale copy, so it
        // over-counts by one until the next resync/rebuild. Serving a correct page with a
        // slightly high count is the right trade against serving the row.
    }

    /// A facet's COUNT must equal the number of results clicking that chip returns.
    ///
    /// It never did before: facets came from JSON-parsing 13,847 `suwayomi_series.genre`
    /// blobs per request (301 entries, no NSFW gate, top entry literally "Japanese") while
    /// the genre FILTER ran against `work_tag` over the canonical catalogue. Both sides now
    /// derive from `work_tag` joined to `feed_series_updates`, in the transaction that
    /// rebuilds the feed — so this equality is structural, and this test is what keeps the
    /// two statements from drifting.
    #[tokio::test]
    async fn genre_facet_counts_equal_what_the_genre_filter_returns() {
        let (s, pool) = setup_full(100).await;
        seed_browse_work(
            &pool,
            "md-g1",
            "One",
            "ja",
            "ONGOING",
            "safe",
            false,
            1,
            &["Action", "Drama"],
        )
        .await;
        seed_browse_work(
            &pool,
            "md-g2",
            "Two",
            "ja",
            "ONGOING",
            "safe",
            false,
            1,
            &["Action"],
        )
        .await;
        // NSFW, and the only carrier of "Ecchi" — so an opted-out viewer must not be offered
        // that chip at all (a chip that returns an empty page is worse than no chip).
        seed_browse_work(
            &pool,
            "md-g3",
            "Three",
            "ja",
            "ONGOING",
            "erotica",
            true,
            1,
            &["Action", "Ecchi"],
        )
        .await;
        crate::catalog::refresh_feed_updates(&pool).await.unwrap();

        let facets = |tok: &'static str| {
            let s = &s;
            async move {
                let r = exec(
                    s,
                    r#"{ genreFacets { genre count } }"#,
                    Some(tok),
                    "1.1.1.1",
                )
                .await;
                assert!(r.errors.is_empty(), "{:?}", r.errors);
                r.data.into_json().unwrap()["genreFacets"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|f| {
                        (
                            f["genre"].as_str().unwrap().to_string(),
                            f["count"].as_i64().unwrap(),
                        )
                    })
                    .collect::<Vec<(String, i64)>>()
            }
        };
        // Opted out: Action counts 2 (the NSFW carrier is excluded) and Ecchi is absent.
        let anon = facets("bobtok").await;
        assert_eq!(
            anon[0],
            ("Action".to_string(), 2),
            "most common first: {anon:?}"
        );
        assert!(
            !anon.iter().any(|(g, _)| g == "Ecchi"),
            "a tag only NSFW works carry must not be offered: {anon:?}"
        );
        // THE equality: every chip's count is the filter's `total`.
        for (genre, count) in &anon {
            let q = format!(
                r#"{{ search(query: "", genres: ["{genre}"], sort: NEWEST) {{ total }} }}"#
            );
            let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
            assert!(r.errors.is_empty(), "{q}: {:?}", r.errors);
            assert_eq!(
                r.data.into_json().unwrap()["search"]["total"]
                    .as_i64()
                    .unwrap(),
                *count,
                "facet {genre} promised {count}"
            );
        }
        // Opted in: the same chips carry the wider counts, Ecchi included.
        exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let opted = facets("bobtok").await;
        assert_eq!(opted[0], ("Action".to_string(), 3));
        assert!(opted.contains(&("Ecchi".to_string(), 1)));
    }

    /// `released_at` is stored as epoch millis and converted back to ISO on the wire.
    /// The conversion is on a DISPLAY field of a pure cache row, so a corrupt value must
    /// degrade to the epoch rather than panic and take the whole page down with it.
    #[test]
    fn epoch_ms_to_iso_round_trips_and_survives_garbage() {
        // Round-trip through the exact encoding the mirror half writes
        // (`strftime('%s', …) * 1000`, so always whole seconds) — UTC, no local offset.
        assert_eq!(
            epoch_ms_to_iso(1_785_073_611_000),
            "2026-07-26T13:46:51+00:00"
        );
        // Sub-second millis survive as a fraction rather than being truncated or lost.
        assert_eq!(
            epoch_ms_to_iso(1_785_073_611_123),
            "2026-07-26T13:46:51.123+00:00"
        );
        assert_eq!(epoch_ms_to_iso(0), "1970-01-01T00:00:00+00:00");
        // Pre-epoch and absurd values: an answer, never a panic.
        assert_eq!(epoch_ms_to_iso(-1000), "1969-12-31T23:59:59+00:00");
        assert_eq!(epoch_ms_to_iso(i64::MAX), "1970-01-01T00:00:00+00:00");
        assert_eq!(epoch_ms_to_iso(i64::MIN), "1970-01-01T00:00:00+00:00");
    }

    #[tokio::test]
    async fn canonical_series_and_chapters_are_readable_and_nsfw_gated() {
        let (s, pool) = setup_full(100).await;
        // A safe work with a cover + two chapters, and an NSFW work.
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-safe",
            &crate::catalog::WorkInput {
                primary_title: Some("Readable Work".into()),
                cover_file_name: Some("cover.jpg".into()),
                is_nsfw: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let ssid = crate::catalog::find_source_series_id(&pool, "mangadex", "mangadex", "md-safe")
            .await
            .unwrap()
            .unwrap();
        for (ext, num, lang) in [
            ("md-ch-2", "2", "en"),
            ("md-ch-1", "1", "en"),
            ("md-ch-3-es", "3", "es"), // non-English → must never surface
        ] {
            crate::catalog::upsert_chapter(
                &pool,
                &ssid,
                &crate::catalog::ChapterInput {
                    external_id: ext.into(),
                    number: Some(num.into()),
                    lang: Some(lang.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let work_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-safe'")
                .fetch_one(&pool)
                .await
                .unwrap();

        // canonicalSeries maps the work; an uncached cover now resolves to our own
        // lazy /covers/ route (serve_cover fetches + caches the MangaDex cover on first
        // hit) rather than the raw CDN URL.
        let q = format!(
            r#"{{ canonicalSeries(workId: "{work_id}") {{ id title coverUrl sourceId chapterCount }} }}"#
        );
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let json = data_json(&r);
        assert!(json.contains("Readable Work"), "{json}");
        assert!(json.contains(&format!("/covers/{work_id}.webp")), "{json}");
        assert!(json.contains("\"chapterCount\":2"), "{json}");

        // canonicalChapters returns ordered chapters keyed by MangaDex uuid.
        let q = format!(r#"{{ canonicalChapters(workId: "{work_id}") {{ id number seriesId }} }}"#);
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        let chs = data["canonicalChapters"].as_array().unwrap();
        assert_eq!(chs.len(), 2);
        assert_eq!(chs[0]["id"], serde_json::json!("md-ch-1"));
        assert_eq!(chs[0]["number"], serde_json::json!(1.0));
        assert_eq!(chs[0]["seriesId"], serde_json::json!(work_id));

        // An NSFW work is hidden from a viewer who hasn't opted in.
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-nsfw",
            &crate::catalog::WorkInput {
                primary_title: Some("Spicy Work".into()),
                is_nsfw: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let nsfw_work: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-nsfw'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let q = format!(r#"{{ canonicalSeries(workId: "{nsfw_work}") {{ title }} }}"#);
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert_eq!(first_error(&r), "No such work");
        // After opting in, it resolves.
        exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "nsfw opt-in failed: {:?}", r.errors);
        assert!(data_json(&r).contains("Spicy Work"));
    }

    /// P2.1: `seriesPage` bundles in ONE request what the reader used to assemble
    /// from `canonicalSeries` + `canonicalChapters` + `workSources` + a per-source
    /// `chapters`/`series` N+1: the canonical series, the spine chapters, every
    /// source mapping WITH its chapters and metadata, and (opt-in) the aggregation.
    #[tokio::test]
    async fn series_page_bundles_series_chapters_sources_and_aggregation() {
        let (s, pool) = setup_full(100).await;
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-bundle",
            &crate::catalog::WorkInput {
                primary_title: Some("Bundled Work".into()),
                cover_file_name: Some("cover.jpg".into()),
                is_nsfw: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let ssid =
            crate::catalog::find_source_series_id(&pool, "mangadex", "mangadex", "md-bundle")
                .await
                .unwrap()
                .unwrap();
        for (ext, num) in [("bd-ch-2", "2"), ("bd-ch-1", "1")] {
            crate::catalog::upsert_chapter(
                &pool,
                &ssid,
                &crate::catalog::ChapterInput {
                    external_id: ext.into(),
                    number: Some(num.into()),
                    lang: Some("en".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let work_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-bundle'")
                .fetch_one(&pool)
                .await
                .unwrap();
        // A Suwayomi mirror with a cached series row + cached chapters, so the bundle
        // can carry the per-source data without a live Suwayomi round trip.
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, status, source_id, chapter_count, updated_at) \
             VALUES (777, 'Mirror Source', 'ONGOING', 'src-a', 1, '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        crate::series_cache::put_chapters(
            &pool,
            777,
            &[crate::suwayomi::SuwayomiChapter {
                id: 9001,
                manga_id: 777,
                name: "Ch 1".into(),
                chapter_number: 1.0,
                scanlator: None,
                upload_date: None,
                is_read: false,
                is_bookmarked: false,
                is_downloaded: false,
                last_page_read: 0,
                page_count: 20,
            }],
        )
        .await
        .unwrap();
        crate::catalog::upsert_source_series(
            &pool, &work_id, "suwayomi", "src-a", "777", None, false,
        )
        .await
        .unwrap();

        let q = format!(
            r#"{{ seriesPage(workId: "{work_id}") {{
                workId
                series {{ title chapterCount }}
                spineChapters {{ id number }}
                sources {{
                    source {{ sourceType sourceKey }}
                    chapters {{ id number }}
                    series {{ title }}
                }}
                aggregated {{ number }}
            }} }}"#
        );
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let page = r.data.into_json().unwrap()["seriesPage"].clone();
        assert_eq!(page["workId"], serde_json::json!(work_id));
        assert_eq!(page["series"]["title"], serde_json::json!("Bundled Work"));
        assert_eq!(page["series"]["chapterCount"], serde_json::json!(2));
        // Spine chapters arrive ordered ascending, exactly as `canonicalChapters`.
        let spine = page["spineChapters"].as_array().unwrap();
        assert_eq!(spine.len(), 2);
        assert_eq!(spine[0]["id"], serde_json::json!("bd-ch-1"));
        assert_eq!(spine[1]["id"], serde_json::json!("bd-ch-2"));
        // MangaDex mapping first (spine carries its content: no duplicate payload),
        // then the Suwayomi mapping WITH its cached chapters and series metadata.
        let sources = page["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(
            sources[0]["source"]["sourceType"],
            serde_json::json!("mangadex")
        );
        assert_eq!(sources[0]["chapters"].as_array().unwrap().len(), 0);
        assert_eq!(sources[0]["series"], serde_json::Value::Null);
        assert_eq!(sources[1]["source"]["sourceKey"], serde_json::json!("777"));
        let suw_chs = sources[1]["chapters"].as_array().unwrap();
        assert_eq!(suw_chs.len(), 1, "cached per-source chapters ride along");
        assert_eq!(suw_chs[0]["number"], serde_json::json!(1.0));
        assert_eq!(
            sources[1]["series"]["title"],
            serde_json::json!("Mirror Source")
        );
        // The opt-in aggregation carries every mirrored chapter number, deduped and
        // ascending — the same contract as `aggregatedChapters`.
        let agg = page["aggregated"].as_array().unwrap();
        let numbers: Vec<f64> = agg.iter().map(|c| c["number"].as_f64().unwrap()).collect();
        assert_eq!(numbers, vec![1.0, 2.0], "{agg:?}");
    }

    /// P2.1: the single-round-trip resolver must apply the same NSFW gate as the
    /// resolvers it replaces, follow a merge redirect to the survivor (reporting the
    /// EFFECTIVE id), and — unlike `canonicalSeries` (CR3) — serve a federation-only
    /// work with `series: null`, because its source mappings are still readable.
    #[tokio::test]
    async fn series_page_gates_nsfw_follows_redirects_and_serves_federation_only() {
        let (s, pool) = setup_full(100).await;

        // NSFW gate: hidden until the viewer opts in.
        seed_canonical(&pool, "md-sp-nsfw", "Spicy Bundle", true, "1").await;
        let nsfw_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-sp-nsfw'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let q = format!(r#"{{ seriesPage(workId: "{nsfw_id}") {{ workId }} }}"#);
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert_eq!(first_error(&r), "No such work");

        // Redirect: a merged-away id answers with the SURVIVOR's bundle and id.
        seed_canonical(&pool, "md-sp-keep", "Surviving Bundle", false, "1").await;
        seed_canonical(&pool, "md-sp-gone", "Folded Bundle", false, "2").await;
        let survivor: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-sp-keep'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let retired: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-sp-gone'")
                .fetch_one(&pool)
                .await
                .unwrap();
        catalog::merge_works_ex(&pool, None, &retired, &survivor)
            .await
            .unwrap();
        let q = format!(r#"{{ seriesPage(workId: "{retired}") {{ workId series {{ title }} }} }}"#);
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "redirect must resolve: {:?}", r.errors);
        let page = r.data.into_json().unwrap()["seriesPage"].clone();
        assert_eq!(
            page["workId"],
            serde_json::json!(survivor),
            "the EFFECTIVE id is the survivor's — every downstream write keys on it"
        );
        assert_eq!(
            page["series"]["title"],
            serde_json::json!("Surviving Bundle")
        );

        // Federation-only: no MangaDex anchor → series null, sources still served.
        sqlx::query("INSERT INTO work (id, primary_title, is_nsfw, created_at, updated_at) VALUES ('w_fed', 'Fed Only', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) VALUES ('ss_fed', 'w_fed', 'suwayomi', 'src-a', '4242', '2026-01-01T00:00:00Z')")
            .execute(&pool)
            .await
            .unwrap();
        let q = r#"{ seriesPage(workId: "w_fed") { workId series { title } spineChapters { id } sources { source { sourceKey } } aggregated { number } } }"#;
        let r = exec(&s, q, Some("bobtok"), "1.1.1.1").await;
        assert!(
            r.errors.is_empty(),
            "federation-only must serve: {:?}",
            r.errors
        );
        let page = r.data.into_json().unwrap()["seriesPage"].clone();
        assert_eq!(page["workId"], serde_json::json!("w_fed"));
        assert_eq!(page["series"], serde_json::Value::Null);
        // No MangaDex anchor: an empty spine and an empty (not erroring) aggregation.
        assert_eq!(page["spineChapters"].as_array().unwrap().len(), 0);
        assert_eq!(page["aggregated"].as_array().unwrap().len(), 0);
        assert_eq!(
            page["sources"][0]["source"]["sourceKey"],
            serde_json::json!("4242")
        );
    }

    #[tokio::test]
    async fn nsfw_override_flips_gating_both_directions() {
        // The admin editor's is_nsfw_override must win over the source flag on EVERY
        // gate (regression for the half-enforced override: a leak when forced NSFW, a
        // broken un-gate when forced SFW).
        let (s, pool) = setup_full(100).await;

        // A source-SFW work an admin force-marks NSFW → hidden from an opted-out viewer.
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-forcensfw",
            &crate::catalog::WorkInput {
                primary_title: Some("Forced NSFW".into()),
                is_nsfw: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let forced_nsfw: String = sqlx::query_scalar(
            "SELECT work_id FROM source_series WHERE source_key = 'md-forcensfw'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE work SET is_nsfw_override = 1 WHERE id = ?")
            .bind(&forced_nsfw)
            .execute(&pool)
            .await
            .unwrap();
        let q = format!(r#"{{ canonicalSeries(workId: "{forced_nsfw}") {{ title }} }}"#);
        assert_eq!(
            first_error(&exec(&s, &q, Some("bobtok"), "1.1.1.1").await),
            "No such work",
            "force-NSFW must hide canonicalSeries"
        );
        let qa = format!(r#"{{ aggregatedChapters(workId: "{forced_nsfw}") {{ number }} }}"#);
        assert_eq!(
            first_error(&exec(&s, &qa, Some("bobtok"), "1.1.1.1").await),
            "No such work",
            "force-NSFW must hide aggregatedChapters"
        );

        // A source-NSFW work an admin force-marks SFW → visible to an opted-out viewer.
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-forcesfw",
            &crate::catalog::WorkInput {
                primary_title: Some("Forced SFW".into()),
                is_nsfw: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let forced_sfw: String = sqlx::query_scalar(
            "SELECT work_id FROM source_series WHERE source_key = 'md-forcesfw'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE work SET is_nsfw_override = 0 WHERE id = ?")
            .bind(&forced_sfw)
            .execute(&pool)
            .await
            .unwrap();
        let q = format!(r#"{{ canonicalSeries(workId: "{forced_sfw}") {{ title }} }}"#);
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(
            r.errors.is_empty(),
            "force-SFW must un-gate: {:?}",
            r.errors
        );
        assert!(data_json(&r).contains("Forced SFW"));
    }

    #[tokio::test]
    async fn series_exposes_localized_descriptions_and_credits() {
        // H2: the S2 enrichment tables are now readable via resolver fields on the
        // Series type (canonicalSeries path).
        let (s, pool) = setup_full(100).await;
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-enr",
            &crate::catalog::WorkInput {
                primary_title: Some("Enriched Work".into()),
                cover_file_name: Some("c.jpg".into()),
                descriptions: vec![
                    ("en".into(), "English blurb.".into()),
                    ("ja".into(), "日本語の紹介".into()),
                ],
                credits: vec![
                    ("author".into(), "Author One".into()),
                    ("artist".into(), "Artist Two".into()),
                ],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let work_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-enr'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let q = format!(
            r#"{{ canonicalSeries(workId: "{work_id}") {{ localizedDescriptions {{ lang description }} credits {{ role name }} }} }}"#
        );
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        let descs = data["canonicalSeries"]["localizedDescriptions"]
            .as_array()
            .unwrap();
        assert_eq!(descs.len(), 2, "both languages surface");
        assert_eq!(descs[0]["lang"], serde_json::json!("en"));
        assert_eq!(descs[0]["description"], serde_json::json!("English blurb."));
        assert_eq!(descs[1]["lang"], serde_json::json!("ja"));
        let credits = data["canonicalSeries"]["credits"].as_array().unwrap();
        assert_eq!(credits.len(), 2);
        assert_eq!(credits[0]["role"], serde_json::json!("artist"));
        assert_eq!(credits[0]["name"], serde_json::json!("Artist Two"));
        assert_eq!(credits[1]["role"], serde_json::json!("author"));
    }

    #[tokio::test]
    async fn work_sources_returns_mappings_ordered_and_extension_joined() {
        let (s, pool) = setup_full(100).await;
        // A MangaDex-native mapping (no extension) plus a Suwayomi mapping whose
        // source_id has catalogued extension coordinates.
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-x",
            &crate::catalog::WorkInput {
                primary_title: Some("Joined Work".into()),
                is_nsfw: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let work_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-x'")
                .fetch_one(&pool)
                .await
                .unwrap();
        crate::catalog::upsert_source_series(
            &pool, &work_id, "suwayomi", "1024", "slug-1", None, false,
        )
        .await
        .unwrap();
        crate::catalog::upsert_source_extension(
            &pool,
            "1024",
            &crate::catalog::SourceExtensionInput {
                pkg_name: "pkg.x".into(),
                repo_url: "https://r".into(),
                apk_name: None,
                version_code: Some(7),
                lang: Some("en".into()),
                is_nsfw: false,
            },
        )
        .await
        .unwrap();

        let q = format!(
            r#"{{ workSources(workId: "{work_id}") {{ sourceType sourceId sourceKey isNsfw lang extension {{ pkgName repoUrl versionCode }} }} }}"#
        );
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        let rows = data["workSources"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "both source mappings surface");
        // The MangaDex-native mapping sorts first and has no extension.
        assert_eq!(rows[0]["sourceType"], serde_json::json!("mangadex"));
        assert_eq!(rows[0]["extension"], serde_json::Value::Null);
        // The Suwayomi mapping carries its joined extension coordinates.
        assert_eq!(rows[1]["sourceType"], serde_json::json!("suwayomi"));
        assert_eq!(rows[1]["sourceKey"], serde_json::json!("slug-1"));
        assert_eq!(rows[1]["extension"]["pkgName"], serde_json::json!("pkg.x"));
        assert_eq!(
            rows[1]["extension"]["repoUrl"],
            serde_json::json!("https://r")
        );
        assert_eq!(rows[1]["extension"]["versionCode"], serde_json::json!(7));
        assert_eq!(rows[1]["lang"], serde_json::json!("en"));
    }

    /// `Series.workId` is the reader's only bridge from a numeric Suwayomi id to the
    /// work behind it — and therefore to that work's SIBLING sources. Browse links a
    /// Suwayomi-anchored work by its numeric `reader_id`, so without this the series
    /// page rendered a three-source work as single-source (no picker at all).
    ///
    /// Asserts the whole bridge, not just the field: numeric id → work → siblings.
    #[tokio::test]
    async fn series_work_id_bridges_a_numeric_id_to_its_sibling_sources() {
        let (s, pool) = setup_full(100).await;
        // Cached, so `series(id:)` resolves without a Suwayomi round trip.
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, status, source_id, chapter_count, updated_at) \
             VALUES (777, 'Multi Source', 'ONGOING', 'src-a', 0, '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-multi",
            &crate::catalog::WorkInput {
                primary_title: Some("Multi Source".into()),
                is_nsfw: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let work_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-multi'")
                .fetch_one(&pool)
                .await
                .unwrap();
        // Two Suwayomi mirrors of the one work; `777` is the id the reader arrives with.
        for (source_id, source_key) in [("src-a", "777"), ("src-b", "888")] {
            crate::catalog::upsert_source_series(
                &pool, &work_id, "suwayomi", source_id, source_key, None, false,
            )
            .await
            .unwrap();
        }

        let r = exec(
            &s,
            r#"{ series(id: "777") { id workId } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let resolved = r.data.into_json().unwrap()["series"]["workId"].clone();
        assert_eq!(
            resolved,
            serde_json::json!(work_id),
            "a numeric id must report the work it belongs to"
        );

        // The point of the bridge: that work carries the sources the picker needs.
        let q = format!(r#"{{ workSources(workId: "{work_id}") {{ sourceKey }} }}"#);
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let mut keys: Vec<String> = r.data.into_json().unwrap()["workSources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["sourceKey"].as_str().unwrap().to_string())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["777", "888", "md-multi"],
            "the work behind `777` exposes every sibling source, not just the one asked for"
        );
    }

    /// The fall-through case: a cached Suwayomi series the catalogue has no mapping for
    /// reports a null `workId`, so the reader renders it from that one source rather
    /// than chasing a work that doesn't exist.
    #[tokio::test]
    async fn series_work_id_is_null_for_an_uncatalogued_series() {
        let (s, pool) = setup_full(100).await;
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, status, source_id, chapter_count, updated_at) \
             VALUES (778, 'Orphan', 'ONGOING', 'src-a', 0, '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let r = exec(
            &s,
            r#"{ series(id: "778") { id workId } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        assert_eq!(
            r.data.into_json().unwrap()["series"]["workId"],
            serde_json::Value::Null
        );
    }

    #[tokio::test]
    async fn work_sources_hides_nsfw_for_opted_out_viewer() {
        let (s, pool) = setup_full(100).await;
        // A safe MangaDex mapping plus an NSFW Suwayomi mapping on the same work.
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-y",
            &crate::catalog::WorkInput {
                primary_title: Some("Mixed Work".into()),
                is_nsfw: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let work_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-y'")
                .fetch_one(&pool)
                .await
                .unwrap();
        crate::catalog::upsert_source_series(
            &pool,
            &work_id,
            "suwayomi",
            "2048",
            "spicy-slug",
            None,
            true,
        )
        .await
        .unwrap();

        let q =
            format!(r#"{{ workSources(workId: "{work_id}") {{ sourceType sourceKey isNsfw }} }}"#);
        // Opted-out viewer: only the safe mapping is visible.
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        let rows = data["workSources"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "nsfw mapping hidden for opted-out viewer");
        assert_eq!(rows[0]["sourceType"], serde_json::json!("mangadex"));

        // After opting in, the NSFW mapping appears.
        exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "nsfw opt-in failed: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        let rows = data["workSources"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "nsfw mapping visible after opt-in");
        assert!(
            rows.iter()
                .any(|row| row["sourceKey"] == serde_json::json!("spicy-slug")),
            "the nsfw mapping surfaces: {data}"
        );
    }

    /// P0-3: gating per-`source_series` row is not enough — an NSFW WORK served from an
    /// SFW source leaked its whole mapping (including the MangaDex UUID) to a viewer
    /// `canonicalSeries` correctly refused. Both `workSources` and `workSourcesBatch`
    /// must gate on the owning work, and on the ADMIN OVERRIDE too.
    #[tokio::test]
    async fn work_sources_gate_on_the_owning_work_including_the_override() {
        let (s, pool) = setup_full(100).await;
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-nsfw-work",
            &crate::catalog::WorkInput {
                primary_title: Some("Flagged Work".into()),
                is_nsfw: false, // derived flag says SFW…
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let work_id: String = sqlx::query_scalar(
            "SELECT work_id FROM source_series WHERE source_key = 'md-nsfw-work'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // …and the admin pins the OVERRIDE, which is all `markSourceNsfw` writes.
        sqlx::query("UPDATE work SET is_nsfw_override = 1 WHERE id = ?")
            .bind(&work_id)
            .execute(&pool)
            .await
            .unwrap();

        let single = format!(r#"{{ workSources(workId: "{work_id}") {{ sourceKey }} }}"#);
        let r = exec(&s, &single, None, "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        assert!(
            data["workSources"].as_array().unwrap().is_empty(),
            "an NSFW work must expose no source mappings anonymously: {data}"
        );

        let batch = format!(
            r#"{{ workSourcesBatch(workIds: ["{work_id}"]) {{ workId sources {{ sourceKey }} }} }}"#
        );
        let r = exec(&s, &batch, None, "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        assert!(
            data["workSourcesBatch"][0]["sources"]
                .as_array()
                .unwrap()
                .is_empty(),
            "the batch path must gate identically: {data}"
        );

        // An opted-in viewer still sees the mapping.
        exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let r = exec(&s, &single, Some("bobtok"), "1.1.1.1").await;
        let data = r.data.into_json().unwrap();
        assert_eq!(
            data["workSources"].as_array().unwrap().len(),
            1,
            "opted-in viewers keep the mapping: {data}"
        );
    }

    /// P1-1: `recordView` is unauthenticated. It must reject ids that resolve to
    /// nothing (so the view tables can't be seeded with arbitrary keys) and rate-limit
    /// per (ip, series) so a handful of requests can't buy the Trending top-10.
    #[tokio::test]
    async fn record_view_rejects_unknown_ids_and_rate_limits() {
        let (s, _pool) = setup_full(100).await;
        let r = exec(
            &s,
            r#"mutation { recordView(seriesId: "not-a-real-series") }"#,
            None,
            "8.8.8.8",
        )
        .await;
        assert_eq!(first_error(&r), "No such series");

        let long = "9".repeat(65);
        let r = exec(
            &s,
            &format!(r#"mutation {{ recordView(seriesId: "{long}") }}"#),
            None,
            "8.8.8.8",
        )
        .await;
        assert_eq!(first_error(&r), "invalid seriesId");

        // `42` is a seeded fixture series: the first 10 land, the 11th is limited.
        for i in 0..10 {
            let r = exec(
                &s,
                r#"mutation { recordView(seriesId: "42") }"#,
                None,
                "7.7.7.7",
            )
            .await;
            assert!(r.errors.is_empty(), "view {i} rejected: {:?}", r.errors);
        }
        let r = exec(
            &s,
            r#"mutation { recordView(seriesId: "42") }"#,
            None,
            "7.7.7.7",
        )
        .await;
        assert!(
            first_error(&r).contains("Too many views"),
            "expected a rate-limit error, got {:?}",
            r.errors
        );
        // A DIFFERENT ip keeps its own budget.
        let r = exec(
            &s,
            r#"mutation { recordView(seriesId: "42") }"#,
            None,
            "6.6.6.6",
        )
        .await;
        assert!(r.errors.is_empty(), "per-ip budget leaked: {:?}", r.errors);
    }

    /// P2-2: comment/review targets are FK-less TEXT columns; a signed-in user must not
    /// be able to open threads or file ratings against ids that don't exist.
    #[tokio::test]
    async fn social_writes_require_an_existing_target() {
        let (s, _pool) = setup_full(100).await;
        let r = exec(
            &s,
            r#"mutation { postReview(input: { seriesId: "ghost", score: 8, body: "", hasSpoiler: false }) { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "No such series");

        let r = exec(
            &s,
            r#"mutation { postComment(input: { targetType: "series", targetId: "ghost", body: "hi", hasSpoiler: false }) { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "No such series");

        let r = exec(
            &s,
            r#"mutation { postComment(input: { targetType: "chapter", targetId: "424242", body: "hi", hasSpoiler: false }) { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "No such chapter");
    }

    /// P2-1: the explicit id list on `markNotificationsRead` issues one UPDATE per id
    /// inside one transaction, so it must be capped.
    #[tokio::test]
    async fn mark_notifications_read_caps_the_id_list() {
        let (s, _pool) = setup_full(100).await;
        let ids = (0..201)
            .map(|i| format!("\"n{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let r = exec(
            &s,
            &format!(r#"mutation {{ markNotificationsRead(ids: [{ids}]) }}"#),
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(
            first_error(&r).contains("Too many notification ids"),
            "expected a cap error, got {:?}",
            r.errors
        );
    }

    #[tokio::test]
    async fn canonical_resolvers_reject_non_mangadex_anchored_work() {
        // A backfilled `w_<numeric>` work has no mangadex source (mangadex_id = None):
        // `isCanonicalId` would still route it here, and the resolver must return
        // "No such work" rather than a titleless/coverless/chapterless shell (CR3).
        let (s, pool) = setup_full(100).await;
        sqlx::query("INSERT INTO work (id, primary_title, is_nsfw, created_at, updated_at) VALUES ('w_42', 'Backfilled Shell', 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')")
            .execute(&pool)
            .await
            .unwrap();
        // A non-mangadex (suwayomi) source_series — no mangadex anchor exists.
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) VALUES ('ss_42', 'w_42', 'suwayomi', '', '42', '2026-01-01T00:00:00Z')")
            .execute(&pool)
            .await
            .unwrap();

        let q = r#"{ canonicalSeries(workId: "w_42") { title } }"#;
        let r = exec(&s, q, Some("bobtok"), "1.1.1.1").await;
        assert_eq!(first_error(&r), "No such work");

        let q = r#"{ canonicalChapters(workId: "w_42") { id } }"#;
        let r = exec(&s, q, Some("bobtok"), "1.1.1.1").await;
        assert_eq!(first_error(&r), "No such work");

        // A normal mangadex-anchored canonical work still resolves.
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-anchored",
            &crate::catalog::WorkInput {
                primary_title: Some("Anchored Work".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let anchored: String = sqlx::query_scalar(
            "SELECT work_id FROM source_series WHERE source_key = 'md-anchored'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let q = format!(r#"{{ canonicalSeries(workId: "{anchored}") {{ title }} }}"#);
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        assert!(
            r.errors.is_empty(),
            "anchored work should resolve: {:?}",
            r.errors
        );
        assert!(data_json(&r).contains("Anchored Work"));
    }

    #[tokio::test]
    async fn library_flags_share_one_row_lookup() {
        // The three per-viewer library flags (`isMarked` / `isFavorite` / `libraryStatus`)
        // are now backed by ONE per-request-cached `user_library` row lookup instead of a
        // SELECT each. Selecting all three in one query must still reflect the same row, and
        // an anonymous viewer must see the empty defaults.
        let (s, pool) = setup_full(100).await;
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-flags",
            &crate::catalog::WorkInput {
                primary_title: Some("Flag Work".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let work_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-flags'")
                .fetch_one(&pool)
                .await
                .unwrap();

        // Favourite + shelf the work as bob (favouriting implies membership).
        let fav = format!(
            r#"mutation {{ setFavorite(seriesId: "{work_id}", favorite: true) {{ id }} }}"#
        );
        let r = exec(&s, &fav, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "setFavorite failed: {:?}", r.errors);
        let status = format!(
            r#"mutation {{ setLibraryStatus(seriesId: "{work_id}", status: "reading") {{ id }} }}"#
        );
        let r = exec(&s, &status, Some("bobtok"), "1.1.1.1").await;
        assert!(
            r.errors.is_empty(),
            "setLibraryStatus failed: {:?}",
            r.errors
        );

        let q = format!(
            r#"{{ canonicalSeries(workId: "{work_id}") {{ isMarked isFavorite libraryStatus }} }}"#
        );
        // Bob: all three reflect the single row.
        let r = exec(&s, &q, Some("bobtok"), "1.1.1.1").await;
        let d = r.data.into_json().unwrap();
        let cs = &d["canonicalSeries"];
        assert_eq!(cs["isMarked"], serde_json::json!(true));
        assert_eq!(cs["isFavorite"], serde_json::json!(true));
        assert_eq!(cs["libraryStatus"], serde_json::json!("reading"));

        // Anonymous viewer: no membership, no favourite, no shelf.
        let r = exec(&s, &q, None, "1.1.1.1").await;
        let d = r.data.into_json().unwrap();
        let cs = &d["canonicalSeries"];
        assert_eq!(cs["isMarked"], serde_json::json!(false));
        assert_eq!(cs["isFavorite"], serde_json::json!(false));
        assert_eq!(cs["libraryStatus"], serde_json::json!(null));
    }

    #[tokio::test]
    async fn canonical_progress_library_and_rating_round_trip() {
        // CR6: a canonical (w_) work gets per-user library + progress state and a
        // reused (reviews-backed) rating aggregate — all keyed on the opaque w_/uuid.
        let (s, pool) = setup_full(100).await;
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-cr6",
            &crate::catalog::WorkInput {
                primary_title: Some("Stateful Work".into()),
                cover_file_name: Some("cover.jpg".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let ssid = crate::catalog::find_source_series_id(&pool, "mangadex", "mangadex", "md-cr6")
            .await
            .unwrap()
            .unwrap();
        for (ext, num) in [("uuid-a", "1"), ("uuid-b", "2")] {
            crate::catalog::upsert_chapter(
                &pool,
                &ssid,
                &crate::catalog::ChapterInput {
                    external_id: ext.into(),
                    number: Some(num.into()),
                    lang: Some("en".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let work_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-cr6'")
                .fetch_one(&pool)
                .await
                .unwrap();

        // ---- Library: mark persists + reflects, unmark removes ----
        let mark_q = |m: bool| {
            format!(r#"mutation {{ mark(seriesId: "{work_id}", marked: {m}) {{ isMarked }} }}"#)
        };
        // Anonymous cannot persist.
        let r = exec(&s, &mark_q(true), None, "1.1.1.1").await;
        assert_eq!(first_error(&r), "Not authenticated");

        let r = exec(&s, &mark_q(true), Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "mark failed: {:?}", r.errors);
        assert!(data_json(&r).contains("\"isMarked\":true"));
        let rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM user_library WHERE user_id = 'bob-id' AND series_id = ?",
        )
        .bind(&work_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 1);
        // canonicalSeries reflects the mark for bob but not for an anonymous viewer.
        let series_q = format!(r#"{{ canonicalSeries(workId: "{work_id}") {{ isMarked }} }}"#);
        let r = exec(&s, &series_q, Some("bobtok"), "1.1.1.1").await;
        assert!(data_json(&r).contains("\"isMarked\":true"));
        let r = exec(&s, &series_q, None, "1.1.1.1").await;
        assert!(data_json(&r).contains("\"isMarked\":false"));
        // Unmark removes.
        let r = exec(&s, &mark_q(false), Some("bobtok"), "1.1.1.1").await;
        assert!(data_json(&r).contains("\"isMarked\":false"));
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM user_library WHERE user_id = 'bob-id'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 0, "unmark deletes the row");

        // ---- Progress: setProgress on a uuid persists + drives resume ----
        let prog_q =
            r#"mutation { setProgress(chapterId: "uuid-a", lastPageRead: 12, read: true) }"#;
        // Anonymous cannot persist.
        let r = exec(&s, prog_q, None, "1.1.1.1").await;
        assert_eq!(first_error(&r), "Not authenticated");

        let r = exec(&s, prog_q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "setProgress failed: {:?}", r.errors);
        // canonicalChapters surfaces the per-user read state (and only for that user).
        let chapters_q =
            format!(r#"{{ canonicalChapters(workId: "{work_id}") {{ id read lastPageRead }} }}"#);
        let r = exec(&s, &chapters_q, Some("bobtok"), "1.1.1.1").await;
        let data = r.data.into_json().unwrap();
        let chs = data["canonicalChapters"].as_array().unwrap();
        let a = chs.iter().find(|c| c["id"] == "uuid-a").unwrap();
        assert_eq!(a["read"], serde_json::json!(true));
        assert_eq!(a["lastPageRead"], serde_json::json!(12));
        // The owning work_id was resolved from the chapter uuid.
        let stored_work: String = sqlx::query_scalar(
            "SELECT work_id FROM canonical_progress WHERE user_id = 'bob-id' AND chapter_id = 'uuid-a'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored_work, work_id);
        // A second call updates in place — no duplicate row.
        let r = exec(
            &s,
            r#"mutation { setProgress(chapterId: "uuid-a", lastPageRead: 30, read: false) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let (cnt, lpr, rd): (i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), MAX(last_page_read), MAX(read) FROM canonical_progress \
             WHERE user_id = 'bob-id' AND chapter_id = 'uuid-a'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((cnt, lpr, rd), (1, 30, 0), "upsert in place");
        // An anonymous viewer sees the chapter as unread.
        let r = exec(&s, &chapters_q, None, "1.1.1.1").await;
        let data = r.data.into_json().unwrap();
        let a = data["canonicalChapters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "uuid-a")
            .unwrap()
            .clone();
        assert_eq!(a["read"], serde_json::json!(false));
        assert_eq!(a["lastPageRead"], serde_json::json!(0));

        // ---- Rating: postReview on the w_ id aggregates via reused reviews ----
        let r = exec(
            &s,
            &format!(
                r#"mutation {{ postReview(input: {{ seriesId: "{work_id}", score: 9, body: "", hasSpoiler: false }}) {{ score }} }}"#
            ),
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "postReview failed: {:?}", r.errors);
        let r = exec(
            &s,
            &format!(
                r#"{{ canonicalSeries(workId: "{work_id}") {{ rating {{ average count }} }} }}"#
            ),
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let data = r.data.into_json().unwrap();
        assert_eq!(
            data["canonicalSeries"]["rating"]["average"],
            serde_json::json!(9.0)
        );
        assert_eq!(
            data["canonicalSeries"]["rating"]["count"],
            serde_json::json!(1)
        );
    }

    #[tokio::test]
    async fn suwayomi_progress_is_per_user() {
        // CR6: numeric Suwayomi series get per-user read state + resume, mirroring the
        // canonical path — two signed-in users are independent, anonymous is all-unread
        // and cannot persist, and library_progress counts only the viewer's own reads.
        let (s, pool) = setup_full(100).await;
        // Seed a cached numeric series (id 500) with two chapters so `chapters()` reads
        // from the cache (no live source fetch) and `setProgress` can resolve series_id.
        for (cid, num) in [(9001_i64, 1.0_f64), (9002, 2.0)] {
            sqlx::query(
                "INSERT INTO suwayomi_chapter \
                   (id, manga_id, name, chapter_number, is_read, last_page_read, page_count, updated_at) \
                 VALUES (?, 500, ?, ?, 1, 99, 20, '2020-01-01T00:00:00Z')",
            )
            .bind(cid)
            .bind(format!("Chapter {num}"))
            .bind(num)
            .execute(&pool)
            .await
            .unwrap();
        }
        // Both users add series 500 to their library.
        for uid in ["bob-id", "admin-id"] {
            sqlx::query(
                "INSERT INTO user_library (user_id, series_id, created_at) \
                 VALUES (?, '500', '2020-01-01T00:00:00Z')",
            )
            .bind(uid)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Anonymous cannot persist progress.
        let prog_q = r#"mutation { setProgress(chapterId: "9001", lastPageRead: 12, read: true) }"#;
        let r = exec(&s, prog_q, None, "1.1.1.1").await;
        assert_eq!(first_error(&r), "Not authenticated");

        // Bob reads chapter 9001; admin reads nothing.
        let r = exec(&s, prog_q, Some("bobtok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "setProgress failed: {:?}", r.errors);
        // series_id was resolved from the chapter's cached manga_id.
        let stored: (String, i64, i64) = sqlx::query_as(
            "SELECT series_id, last_page_read, read FROM suwayomi_progress \
             WHERE user_id = 'bob-id' AND chapter_id = '9001'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, ("500".into(), 12, 1));

        // chapters() overlays only the VIEWER's state — despite the cached global
        // is_read=1/last_page_read=99, bob sees his own values and admin sees unread.
        let chapters_q = r#"{ chapters(seriesId: "500") { id read lastPageRead } }"#;
        let read_state = |r: async_graphql::Response, id: &str| {
            r.data.into_json().unwrap()["chapters"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == id)
                .unwrap()
                .clone()
        };
        let bob = read_state(
            exec(&s, chapters_q, Some("bobtok"), "1.1.1.1").await,
            "9001",
        );
        assert_eq!(bob["read"], serde_json::json!(true));
        assert_eq!(bob["lastPageRead"], serde_json::json!(12));
        let admin = read_state(
            exec(&s, chapters_q, Some("admintok"), "1.1.1.1").await,
            "9001",
        );
        assert_eq!(admin["read"], serde_json::json!(false), "admin independent");
        assert_eq!(admin["lastPageRead"], serde_json::json!(0));
        // Anonymous also sees everything unread (cached global flag ignored).
        let anon = read_state(exec(&s, chapters_q, None, "1.1.1.1").await, "9001");
        assert_eq!(anon["read"], serde_json::json!(false));
        assert_eq!(anon["lastPageRead"], serde_json::json!(0));

        // libraryProgress reflects each viewer's own read count; total is 0 (client
        // falls back to chapterCount).
        let lib_q = r#"{ libraryProgress { id read total } }"#;
        let bob_lib = exec(&s, lib_q, Some("bobtok"), "1.1.1.1").await;
        let lp = bob_lib.data.into_json().unwrap()["libraryProgress"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "500")
            .unwrap()
            .clone();
        assert_eq!(lp["read"], serde_json::json!(1));
        assert_eq!(lp["total"], serde_json::json!(0));
        // Admin has no progress rows → series omitted entirely.
        let admin_lib = exec(&s, lib_q, Some("admintok"), "1.1.1.1").await;
        assert!(admin_lib.data.into_json().unwrap()["libraryProgress"]
            .as_array()
            .unwrap()
            .is_empty());

        // A second write updates in place — no duplicate row.
        let r = exec(
            &s,
            r#"mutation { setProgress(chapterId: "9001", lastPageRead: 5, read: false) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let (cnt, lpr, rd): (i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), MAX(last_page_read), MAX(read) FROM suwayomi_progress \
             WHERE user_id = 'bob-id' AND chapter_id = '9001'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((cnt, lpr, rd), (1, 5, 0), "upsert in place");
    }

    #[tokio::test]
    async fn updates_total_excludes_nsfw_for_opted_out_viewer() {
        // N3: `total`/`has_next` must count only rows the viewer can see, filtered in
        // SQL rather than after the page slice. Seed two series with a new-chapter
        // timestamp — one linked to an NSFW work, one to a SFW work.
        let (s, pool) = setup_full(100).await;
        for (sid, title, nsfw) in [("100", "Safe Feed", false), ("200", "Spicy Feed", true)] {
            let wid = crate::catalog::create_work(
                &pool,
                &crate::catalog::WorkInput {
                    primary_title: Some(title.into()),
                    is_nsfw: nsfw,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            crate::catalog::upsert_source_series(
                &pool, &wid, "suwayomi", "suwayomi", sid, None, nsfw,
            )
            .await
            .unwrap();
            // The feed is driven from `suwayomi_series` now (it carries the release-time
            // sort key), so a scan-state row alone is no longer enough to be counted —
            // the series must also be a library member. See `updates_total_matches_paged_row_count`.
            sqlx::query(
                "INSERT INTO suwayomi_series \
                   (id, title, status, source_id, chapter_count, in_library, latest_chapter_at, updated_at) \
                 VALUES (?, ?, 'ONGOING', 'src', 5, 1, '1751328000000', '2026-07-10T00:00:00Z')",
            )
            .bind(sid)
            .bind(title)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO series_scan_state (series_id, last_new_chapter_at, updated_at) \
                 VALUES (?, '2026-07-10T00:00:00Z', '2026-07-10T00:00:00Z')",
            )
            .bind(sid)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Opted-out (default): total counts only the SFW series (no skew).
        let r = exec(
            &s,
            r#"{ updates { total hasNextPage } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        assert_eq!(
            data["updates"]["total"],
            serde_json::json!(1),
            "N3: nsfw row must not inflate total"
        );
        assert_eq!(data["updates"]["hasNextPage"], serde_json::json!(false));

        // Opted in: both rows count.
        exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let r = exec(&s, r#"{ updates { total } }"#, Some("bobtok"), "1.1.1.1").await;
        let data = r.data.into_json().unwrap();
        assert_eq!(data["updates"]["total"], serde_json::json!(2));
    }

    #[tokio::test]
    async fn auto_merge_ors_source_nsfw_into_existing_work() {
        // N4: an auto-merge onto an existing SFW work must escalate that work to NSFW
        // when the source signals it — every gating read consults `work.is_nsfw`, never
        // `source_series.is_nsfw`, so setting only the latter would leak.
        let pool = migrated_pool().await;
        // A SFW existing work with a title + cover so an exact-title + matching-cover
        // add auto-merges (DD3 requires cover corroboration for auto-merge).
        let existing = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Border Town Tales".into()),
                cover_phash: Some("aabbccddeeff0011".into()),
                is_nsfw: false,
                aliases: vec![crate::catalog::Alias {
                    raw: "Border Town Tales".into(),
                    lang: None,
                }],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Same title, an NSFW genre (source_nsfw = true), identical cover hash.
        let m = suwayomi_manga(55, "Border Town Tales", &["Action", "Hentai"], "src1");
        let r = add_source_series_core(&pool, &m, Some("aabbccddeeff0011".into()))
            .await
            .unwrap();
        assert_eq!(
            r.decision, "auto_merge",
            "expected auto-merge, got {}",
            r.decision
        );
        assert_eq!(r.work_id, existing);
        let nsfw: i64 = sqlx::query_scalar("SELECT is_nsfw FROM work WHERE id = ?")
            .bind(&existing)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            nsfw, 1,
            "N4: source nsfw signal must OR into the merged work"
        );
    }

    #[tokio::test]
    async fn suwayomi_detail_and_reader_paths_gate_nsfw() {
        let (s, pool) = setup_full(100).await;
        // A federated Suwayomi series (id 4242) linked to an NSFW work — the state left
        // by the Tier-2 add flow once it flags a source series as NSFW (3.1). The
        // Suwayomi ids are sequential, so an opted-out viewer could otherwise hand-craft
        // this id to read full detail / chapter list / page images (N2).
        let work_id = crate::catalog::create_work(
            &pool,
            &crate::catalog::WorkInput {
                primary_title: Some("Spicy Suwa".into()),
                is_nsfw: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        crate::catalog::upsert_source_series(
            &pool, &work_id, "suwayomi", "suwayomi", "4242", None, true,
        )
        .await
        .unwrap();

        // Opted-out viewer: detail + chapter list are hidden *before* any source
        // round-trip, so the gate resolves without the (unreachable) Suwayomi server.
        let r = exec(
            &s,
            r#"{ series(id: "4242") { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "No such series");
        let r = exec(
            &s,
            r#"{ chapters(seriesId: "4242") { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "No such series");

        // Opting in passes the gate: the query then fails only because the test
        // Suwayomi server is unreachable, NOT with the NSFW not-found.
        exec(
            &s,
            r#"mutation { setShowNsfw(value: true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let r = exec(
            &s,
            r#"{ series(id: "4242") { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_ne!(
            first_error(&r),
            "No such series",
            "opted-in viewer must pass the gate: {:?}",
            r.errors
        );

        // A safe/uncatalogued series is never over-blocked: even for an opted-out
        // viewer (admin defaults to hidden) the gate passes and the resolver proceeds to
        // the (unreachable) source, so the error is not the NSFW not-found.
        let r = exec(
            &s,
            r#"{ series(id: "999999") { id } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert_ne!(first_error(&r), "No such series");
    }

    #[tokio::test]
    async fn login_succeeds_with_correct_password() {
        let s = setup().await;
        let r = exec(
            &s,
            r#"mutation { login(username:"bob", password:"password123") { token } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
    }

    #[tokio::test]
    async fn login_rejects_wrong_password() {
        let s = setup().await;
        let r = exec(
            &s,
            r#"mutation { login(username:"bob", password:"nope") { token } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "Invalid username or password");
    }

    #[tokio::test]
    async fn login_with_unknown_username_is_rejected_uniformly() {
        let s = setup().await;
        // A3: an unknown username returns the same error as a wrong password
        // (and, in prod, after the same constant-work Argon2 verify).
        let r = exec(
            &s,
            r#"mutation { login(username:"nobody-here", password:"password123") { token } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "Invalid username or password");
    }

    #[tokio::test]
    async fn overlong_passwords_are_rejected() {
        let s = setup().await;
        let long = "x".repeat(MAX_PASSWORD_LEN + 1);
        // A7: register rejects an over-long password.
        let q = format!(
            r#"mutation {{ register(input:{{username:"newbie", email:"n@e.com", password:"{long}"}}) {{ token }} }}"#
        );
        let r = exec(&s, &q, None, "1.1.1.1").await;
        assert_eq!(first_error(&r), "password must be at most 1024 characters");
        // A7: login rejects an over-long password without hashing it.
        let q = format!(r#"mutation {{ login(username:"bob", password:"{long}") {{ token }} }}"#);
        let r = exec(&s, &q, None, "1.1.1.1").await;
        assert_eq!(first_error(&r), "Invalid username or password");
    }

    #[tokio::test]
    async fn banned_user_cannot_login_even_with_correct_password() {
        let s = setup().await;
        let r = exec(
            &s,
            r#"mutation { login(username:"carol", password:"password123") { token } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "This account has been suspended.");
    }

    #[tokio::test]
    async fn rate_limit_keys_on_ip_and_ignores_successes() {
        let s = setup_with_limit(2).await;
        let wrong = r#"mutation { login(username:"bob", password:"nope") { token } }"#;
        let right = r#"mutation { login(username:"bob", password:"password123") { token } }"#;
        // two failures from the attacker IP exhaust its budget
        for _ in 0..2 {
            let r = exec(&s, wrong, None, "9.9.9.9").await;
            assert_eq!(first_error(&r), "Invalid username or password");
        }
        // third attempt from that IP is blocked...
        let blocked = exec(&s, right, None, "9.9.9.9").await;
        assert!(
            first_error(&blocked).contains("Too many login attempts"),
            "got: {}",
            first_error(&blocked)
        );
        // ...but the victim's own IP is unaffected (M1)
        let victim = exec(&s, right, None, "8.8.8.8").await;
        assert!(
            victim.errors.is_empty(),
            "cross-IP lockout: {:?}",
            victim.errors
        );
        // and repeated *successful* logins never trip the limit (M2)
        for _ in 0..5 {
            let r = exec(&s, right, None, "7.7.7.7").await;
            assert!(
                r.errors.is_empty(),
                "success counted against limit: {:?}",
                r.errors
            );
        }
    }

    #[tokio::test]
    async fn admin_only_query_is_gated() {
        let s = setup().await;
        let q = "{ scanStatus { librarySize } }";
        assert_eq!(
            first_error(&exec(&s, q, None, "1.1.1.1").await),
            "Not authenticated"
        );
        assert_eq!(
            first_error(&exec(&s, q, Some("bobtok"), "1.1.1.1").await),
            "Admin access required"
        );
        let ok = exec(&s, q, Some("admintok"), "1.1.1.1").await;
        assert!(ok.errors.is_empty(), "admin blocked: {:?}", ok.errors);
    }

    #[tokio::test]
    async fn delete_comment_requires_admin() {
        let s = setup().await;
        let q = r#"mutation { deleteComment(commentId:"nope") }"#;
        assert_eq!(
            first_error(&exec(&s, q, Some("bobtok"), "1.1.1.1").await),
            "Admin access required"
        );
        // admin: no such comment => false, no error
        let r = exec(&s, q, Some("admintok"), "1.1.1.1").await;
        assert!(r.errors.is_empty());
        assert_eq!(
            r.data.into_json().unwrap()["deleteComment"],
            serde_json::json!(false)
        );
    }

    #[tokio::test]
    async fn ban_user_guards() {
        let s = setup().await;
        // non-admin rejected
        assert_eq!(
            first_error(
                &exec(
                    &s,
                    r#"mutation { banUser(userId:"bob-id", banned:true) { id } }"#,
                    Some("bobtok"),
                    "1.1.1.1"
                )
                .await
            ),
            "Admin access required"
        );
        // admin can't ban self
        assert_eq!(
            first_error(
                &exec(
                    &s,
                    r#"mutation { banUser(userId:"admin-id", banned:true) { id } }"#,
                    Some("admintok"),
                    "1.1.1.1"
                )
                .await
            ),
            "You cannot ban your own account."
        );
        // admin can't ban another admin (bob promoted first would be needed; use nonexistent)
        assert_eq!(
            first_error(
                &exec(
                    &s,
                    r#"mutation { banUser(userId:"ghost", banned:true) { id } }"#,
                    Some("admintok"),
                    "1.1.1.1"
                )
                .await
            ),
            "No such user."
        );
        // admin bans bob -> bob can no longer log in. The mutation now returns
        // the full AdminUser, so `isBanned` is selectable and reflects the write.
        let ban = exec(
            &s,
            r#"mutation { banUser(userId:"bob-id", banned:true) { isBanned username } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(ban.errors.is_empty(), "ban failed: {:?}", ban.errors);
        let ban_data = ban.data.into_json().unwrap();
        assert_eq!(ban_data["banUser"]["isBanned"], serde_json::json!(true));
        assert_eq!(ban_data["banUser"]["username"], serde_json::json!("bob"));
        let login = exec(
            &s,
            r#"mutation { login(username:"bob", password:"password123") { token } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&login), "This account has been suspended.");
    }

    #[tokio::test]
    async fn ban_hides_comments_and_reviews() {
        let s = setup().await;
        // Bob posts a comment on a series thread and a review on that series.
        let posted_comment = exec(
            &s,
            r#"mutation { postComment(input:{ targetType:"series", targetId:"s1", body:"great read", hasSpoiler:false }) { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(
            posted_comment.errors.is_empty(),
            "post comment failed: {:?}",
            posted_comment.errors
        );
        let posted_review = exec(
            &s,
            r#"mutation { postReview(input:{ seriesId:"s1", score:9, body:"loved it", hasSpoiler:false }) { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(
            posted_review.errors.is_empty(),
            "post review failed: {:?}",
            posted_review.errors
        );

        // Before the ban both surface, with total == 1.
        let before = exec(
            &s,
            r#"{ comments(targetType:"series", targetId:"s1") { items { id } total }
                reviews(seriesId:"s1") { items { id } total } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let b = before.data.into_json().unwrap();
        assert_eq!(b["comments"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(b["comments"]["total"], serde_json::json!(1));
        assert_eq!(b["reviews"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(b["reviews"]["total"], serde_json::json!(1));

        // Admin bans bob.
        let ban = exec(
            &s,
            r#"mutation { banUser(userId:"bob-id", banned:true) { id } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(ban.errors.is_empty(), "ban failed: {:?}", ban.errors);

        // After the ban bob's comment and review are hidden server-side and the
        // totals decrement, so the admin's "removed" feedback is truthful on reload.
        let after = exec(
            &s,
            r#"{ comments(targetType:"series", targetId:"s1") { items { id } total }
                reviews(seriesId:"s1") { items { id } total } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        let a = after.data.into_json().unwrap();
        assert!(a["comments"]["items"].as_array().unwrap().is_empty());
        assert_eq!(a["comments"]["total"], serde_json::json!(0));
        assert!(a["reviews"]["items"].as_array().unwrap().is_empty());
        assert_eq!(a["reviews"]["total"], serde_json::json!(0));
    }

    #[tokio::test]
    async fn threaded_replies_nest_and_paginate_by_root() {
        let s = setup().await;
        // Root comment on the series thread.
        let root = exec(
            &s,
            r#"mutation { postComment(input:{ targetType:"series", targetId:"s1", body:"root", hasSpoiler:false }) { id parentId } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(
            root.errors.is_empty(),
            "root post failed: {:?}",
            root.errors
        );
        let root_json = root.data.into_json().unwrap();
        assert_eq!(
            root_json["postComment"]["parentId"],
            serde_json::json!(null)
        );
        let root_id = root_json["postComment"]["id"].as_str().unwrap().to_string();

        // Reply to the root, and a nested reply to that reply (arbitrary depth).
        let reply = exec(
            &s,
            &format!(
                r#"mutation {{ postComment(input:{{ targetType:"series", targetId:"s1", parentId:"{root_id}", body:"reply", hasSpoiler:false }}) {{ id parentId }} }}"#
            ),
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(reply.errors.is_empty(), "reply failed: {:?}", reply.errors);
        let reply_json = reply.data.into_json().unwrap();
        assert_eq!(
            reply_json["postComment"]["parentId"],
            serde_json::json!(root_id)
        );
        let reply_id = reply_json["postComment"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let nested = exec(
            &s,
            &format!(
                r#"mutation {{ postComment(input:{{ targetType:"series", targetId:"s1", parentId:"{reply_id}", body:"nested", hasSpoiler:false }}) {{ id }} }}"#
            ),
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(
            nested.errors.is_empty(),
            "nested failed: {:?}",
            nested.errors
        );

        // The thread query returns all three (flat, ascending) but counts ONE root.
        let list = exec(
            &s,
            r#"{ comments(targetType:"series", targetId:"s1") { items { id parentId body } total hasNextPage } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let lj = list.data.into_json().unwrap();
        assert_eq!(lj["comments"]["items"].as_array().unwrap().len(), 3);
        assert_eq!(
            lj["comments"]["total"],
            serde_json::json!(1),
            "one root thread"
        );
        assert_eq!(lj["comments"]["hasNextPage"], serde_json::json!(false));

        // A reply whose parent lives on a different target is rejected.
        let cross = exec(
            &s,
            &format!(
                r#"mutation {{ postComment(input:{{ targetType:"series", targetId:"s2", parentId:"{root_id}", body:"x", hasSpoiler:false }}) {{ id }} }}"#
            ),
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&cross), "reply target not found on this thread");

        // Deleting the root removes the whole subtree (admin moderation).
        let del = exec(
            &s,
            &format!(r#"mutation {{ deleteComment(commentId:"{root_id}") }}"#),
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(del.errors.is_empty(), "delete failed: {:?}", del.errors);
        assert_eq!(
            del.data.into_json().unwrap()["deleteComment"],
            serde_json::json!(true)
        );
        let after = exec(
            &s,
            r#"{ comments(targetType:"series", targetId:"s1") { items { id } total } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let aj = after.data.into_json().unwrap();
        assert!(
            aj["comments"]["items"].as_array().unwrap().is_empty(),
            "subtree gone"
        );
        assert_eq!(aj["comments"]["total"], serde_json::json!(0));
    }

    #[tokio::test]
    async fn comment_media_links_once_and_rejects_bad_ids() {
        let (s, pool) = setup_full(100).await;
        // Stage an uploaded image row for bob (as POST /comment-media would).
        sqlx::query(
            "INSERT INTO comment_media (id, comment_id, user_id, webp, width, height, created_at) \
             VALUES ('m1', NULL, 'bob-id', X'00', 800, 600, '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Empty body AND no media → rejected.
        let empty = exec(
            &s,
            r#"mutation { postComment(input:{ targetType:"series", targetId:"s1", body:"   ", hasSpoiler:false }) { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&empty), "comment must have text or an image");

        // Someone else's / unlinked media id owned by bob: another user can't claim it.
        let steal = exec(
            &s,
            r#"mutation { postComment(input:{ targetType:"series", targetId:"s1", body:"mine now", hasSpoiler:false, mediaId:"m1" }) { id } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&steal), "attached image not found");

        // Bob attaches his image (image-only comment, empty body allowed).
        let posted = exec(
            &s,
            r#"mutation { postComment(input:{ targetType:"series", targetId:"s1", body:"", hasSpoiler:false, mediaId:"m1" }) { id mediaUrl mediaWidth mediaHeight } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(
            posted.errors.is_empty(),
            "attach failed: {:?}",
            posted.errors
        );
        let pj = posted.data.into_json().unwrap();
        assert_eq!(
            pj["postComment"]["mediaUrl"],
            serde_json::json!("/comment-media/m1.webp")
        );
        assert_eq!(pj["postComment"]["mediaWidth"], serde_json::json!(800));
        assert_eq!(pj["postComment"]["mediaHeight"], serde_json::json!(600));

        // The image is now linked, so it can't be attached again.
        let reuse = exec(
            &s,
            r#"mutation { postComment(input:{ targetType:"series", targetId:"s1", body:"again", hasSpoiler:false, mediaId:"m1" }) { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&reuse), "attached image not found");

        // The thread query surfaces the media on the stored comment.
        let list = exec(
            &s,
            r#"{ comments(targetType:"series", targetId:"s1") { items { mediaUrl mediaWidth } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let lj = list.data.into_json().unwrap();
        assert_eq!(
            lj["comments"]["items"][0]["mediaUrl"],
            serde_json::json!("/comment-media/m1.webp")
        );
        assert_eq!(
            lj["comments"]["items"][0]["mediaWidth"],
            serde_json::json!(800)
        );
    }

    #[tokio::test]
    async fn my_review_survives_pagination_and_is_null_when_signed_out() {
        let (s, pool) = setup_full(100).await;
        // Bob reviews s1 early (created_at = now, e.g. 2026).
        let posted = exec(
            &s,
            r#"mutation { postReview(input:{ seriesId:"s1", score:7, body:"my early take", hasSpoiler:false }) { id } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(
            posted.errors.is_empty(),
            "post review failed: {:?}",
            posted.errors
        );
        let my_id = posted.data.into_json().unwrap()["postReview"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // 20 other users each post a NEWER review on s1, so a page-1 query
        // (LIMIT PAGE_SIZE, `created_at DESC`) no longer includes bob's earlier one.
        for i in 0..20 {
            let uid = format!("filler-{i}");
            seed_user(&pool, &uid, &format!("filler{i}"), 0, 0).await;
            sqlx::query(
                "INSERT INTO reviews (id, series_id, user_id, score, body, has_spoiler, created_at, updated_at) \
                 VALUES (?, 's1', ?, 8, 'filler', 0, ?, ?)",
            )
            .bind(format!("rev-{i}"))
            .bind(&uid)
            .bind(format!("2099-01-01T00:00:{i:02}Z"))
            .bind(format!("2099-01-01T00:00:{i:02}Z"))
            .execute(&pool)
            .await
            .unwrap();
        }

        // Page 1 of the public reviews list drops bob's earlier review.
        let page1 = exec(
            &s,
            r#"{ reviews(seriesId:"s1") { items { id } } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        let ids: Vec<String> = page1.data.into_json().unwrap()["reviews"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), 20, "page 1 is capped at PAGE_SIZE");
        assert!(
            !ids.contains(&my_id),
            "bob's early review should have fallen off page 1"
        );

        // But myReview retrieves bob's own review by identity, regardless of paging.
        let mine = exec(
            &s,
            r#"{ myReview(seriesId:"s1") { id score body } }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(mine.errors.is_empty(), "myReview failed: {:?}", mine.errors);
        let m = mine.data.into_json().unwrap();
        assert_eq!(m["myReview"]["id"], serde_json::json!(my_id));
        assert_eq!(m["myReview"]["score"], serde_json::json!(7));
        assert_eq!(m["myReview"]["body"], serde_json::json!("my early take"));

        // A signed-out viewer gets null (and no error) — not `require_user`.
        let anon = exec(&s, r#"{ myReview(seriesId:"s1") { id } }"#, None, "1.1.1.1").await;
        assert!(
            anon.errors.is_empty(),
            "anon myReview should not error: {:?}",
            anon.errors
        );
        assert_eq!(
            anon.data.into_json().unwrap()["myReview"],
            serde_json::Value::Null
        );

        // A signed-in viewer with no review on this series also gets null.
        let admin_none = exec(
            &s,
            r#"{ myReview(seriesId:"s1") { id } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(admin_none.errors.is_empty());
        assert_eq!(
            admin_none.data.into_json().unwrap()["myReview"],
            serde_json::Value::Null
        );
    }

    #[tokio::test]
    async fn set_user_admin_cannot_demote_self() {
        let s = setup().await;
        assert_eq!(
            first_error(
                &exec(
                    &s,
                    r#"mutation { setUserAdmin(userId:"admin-id", isAdmin:false) { isAdmin } }"#,
                    Some("admintok"),
                    "1.1.1.1"
                )
                .await
            ),
            "You cannot remove your own admin access."
        );
        // promoting bob works
        let r = exec(
            &s,
            r#"mutation { setUserAdmin(userId:"bob-id", isAdmin:true) { isAdmin } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "promote failed: {:?}", r.errors);
        assert_eq!(
            r.data.into_json().unwrap()["setUserAdmin"]["isAdmin"],
            serde_json::json!(true)
        );
    }

    #[tokio::test]
    async fn updates_counts_dated_scan_state_rows() {
        // Build state directly so we can seed series_scan_state, then query updates.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        for (sid, ts) in [
            ("10", Some("2026-02-01T00:00:00Z")),
            ("11", Some("2026-03-01T00:00:00Z")),
            ("12", None),
        ] {
            // Every scan-state row needs its library `suwayomi_series` counterpart: the
            // feed reads the release-time sort key from there, so `in_library = 1` is
            // now part of what `total` counts.
            sqlx::query(
                "INSERT INTO suwayomi_series \
                   (id, title, status, source_id, chapter_count, in_library, latest_chapter_at, updated_at) \
                 VALUES (?, ?, 'ONGOING', 'src', 0, 1, '1751328000000', '2026-01-01T00:00:00Z')",
            )
            .bind(sid)
            .bind(format!("Series {sid}"))
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO series_scan_state \
                   (series_id, avg_interval_hours, known_chapter_count, last_new_chapter_at, updated_at) \
                 VALUES (?, 0, 0, ?, '2026-01-01T00:00:00Z')",
            )
            .bind(sid)
            .bind(ts)
            .execute(&pool)
            .await
            .unwrap();
        }
        let state = std::sync::Arc::new(AppState {
            pool: pool.clone(),
            cover_pool: pool.clone(),
            suwayomi: crate::suwayomi::SuwayomiClient::new("http://127.0.0.1:1".into(), None, None),
            mangadex: std::sync::Arc::new(crate::mangadex::MangaDexClient::new(
                "test-ua", 5.0, 40.0,
            )),
            admin_users: vec![],
            scan_health: Mutex::new(ScanHealth::default()),
            auth_limiter: RateLimiter::new(100, 60),
            federated_limiter: RateLimiter::new(100, 60),
            session_ttl_secs: 30 * 24 * 60 * 60,
            series_inflight: KeyedLocks::default(),
            chapters_inflight: KeyedLocks::default(),
            cover_crawl_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            catalogue_cover_phash: false,
            ext_icons_dir: std::path::PathBuf::new(),
        });
        let s = build_schema(state, false);
        let r = exec(
            &s,
            r#"{ updates { total hasNextPage items { id } } }"#,
            None,
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "updates errored: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        // Two rows carry a last_new_chapter_at; the null one is excluded.
        assert_eq!(data["updates"]["total"], serde_json::json!(2));
        assert_eq!(data["updates"]["hasNextPage"], serde_json::json!(false));
        // Suwayomi is unreachable here, but hydration is DB-first, so the two
        // `suwayomi_series` rows resolve from cache. They share one `latest_chapter_at`,
        // so the `id DESC` tiebreaker decides the order.
        let ids: Vec<&str> = data["updates"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["11", "10"],
            "items must be exactly the counted rows, tie broken by id DESC"
        );
        assert_eq!(
            ids.len() as i64,
            data["updates"]["total"].as_i64().unwrap(),
            "total must equal what the single page returns"
        );
    }

    #[tokio::test]
    async fn set_progress_rejects_negative_page() {
        let s = setup().await;
        let r = exec(
            &s,
            r#"mutation { setProgress(chapterId:"1", lastPageRead:-5, read:false) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "lastPageRead must be non-negative");
    }

    #[tokio::test]
    async fn update_series_admin_rejects_out_of_range() {
        let s = setup().await;
        assert!(first_error(
            &exec(
                &s,
                r#"mutation { updateSeriesAdmin(input:{seriesId:"3", pollEveryMinutes:0}) { id } }"#,
                Some("admintok"),
                "1.1.1.1"
            )
            .await
        )
        .contains("pollEveryMinutes"));
        assert!(first_error(
            &exec(
                &s,
                r#"mutation { updateSeriesAdmin(input:{seriesId:"3", overrideIntervalHours:1000000000}) { id } }"#,
                Some("admintok"),
                "1.1.1.1"
            )
            .await
        )
        .contains("overrideIntervalHours"));
    }

    #[tokio::test]
    async fn mark_source_nsfw_flags_a_whole_source_and_requires_admin() {
        let (s, pool) = setup_full(100).await;
        // Two works ingested from suwayomi source "1534", one from another source.
        for (wid, ssid, src) in [
            ("w_omega1", "sso1", "1534"),
            ("w_omega2", "sso2", "1534"),
            ("w_other", "ssx", "9999"),
        ] {
            sqlx::query(
                "INSERT INTO work (id, is_nsfw, created_at, updated_at) \
                 VALUES (?, 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(wid)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
                 VALUES (?, ?, 'suwayomi', ?, ?, '2026-01-01T00:00:00Z')",
            )
            .bind(ssid)
            .bind(wid)
            .bind(src)
            .bind(wid)
            .execute(&pool)
            .await
            .unwrap();
        }
        // Non-admin is refused.
        let r = exec(
            &s,
            r#"mutation { markSourceNsfw(sourceId:"1534", isNsfw:true) }"#,
            Some("bobtok"),
            "1.1.1.1",
        )
        .await;
        assert!(!r.errors.is_empty(), "non-admin must be refused");
        // Admin marks source "1534" → its two works updated, the other untouched.
        let r = exec(
            &s,
            r#"mutation { markSourceNsfw(sourceId:"1534", isNsfw:true) }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(
            data_json(&r).contains("\"markSourceNsfw\":2"),
            "{}",
            data_json(&r)
        );
        async fn override_of(pool: &SqlitePool, id: &str) -> Option<i64> {
            sqlx::query_scalar::<_, Option<i64>>("SELECT is_nsfw_override FROM work WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap()
        }
        assert_eq!(override_of(&pool, "w_omega1").await, Some(1));
        assert_eq!(override_of(&pool, "w_omega2").await, Some(1));
        assert_eq!(
            override_of(&pool, "w_other").await,
            None,
            "other source untouched"
        );
        // Undo: false clears the override back to NULL (source-derived).
        exec(
            &s,
            r#"mutation { markSourceNsfw(sourceId:"1534", isNsfw:false) }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(
            override_of(&pool, "w_omega1").await,
            None,
            "false clears the override"
        );
    }

    #[tokio::test]
    async fn rederive_suwayomi_nsfw_flips_adult_sources_and_genres() {
        let (s, pool) = setup_full(100).await;
        // Three suwayomi works, all stored SFW (the leak state).
        for wid in ["w_genre", "w_srcflag", "w_sfw"] {
            sqlx::query(
                "INSERT INTO work (id, is_nsfw, created_at, updated_at) \
                 VALUES (?, 0, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .bind(wid)
            .execute(&pool)
            .await
            .unwrap();
        }
        // (work, suwayomi id, source). w_genre+w_sfw on srcA (SFW source), w_srcflag on srcB.
        for (wid, key, src) in [
            ("w_genre", "111", "srcA"),
            ("w_srcflag", "222", "srcB"),
            ("w_sfw", "333", "srcA"),
        ] {
            sqlx::query(
                "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
                 VALUES (?, ?, 'suwayomi', ?, ?, '2026-01-01T00:00:00Z')",
            )
            .bind(format!("ss_{key}"))
            .bind(wid)
            .bind(src)
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        }
        // Cached genres: w_genre is tagged "Mature" (adult genre); the others aren't.
        for (key, genre) in [
            ("111", r#"["Romance","Mature"]"#),
            ("222", r#"["Action"]"#),
            ("333", r#"["Action"]"#),
        ] {
            sqlx::query(
                "INSERT INTO suwayomi_series (id, title, status, source_id, updated_at, genre) \
                 VALUES (?, 'T', 'ONGOING', 'x', '2026-01-01T00:00:00Z', ?)",
            )
            .bind(key.parse::<i64>().unwrap())
            .bind(genre)
            .execute(&pool)
            .await
            .unwrap();
        }
        // srcB is an adult SOURCE (its per-series genres don't say so → the leak vector).
        sqlx::query(
            "INSERT INTO source_extension (source_id, pkg_name, repo_url, is_nsfw, updated_at) \
             VALUES ('srcB', 'pkg', 'repo', 1, '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let r = exec(
            &s,
            r#"mutation { rederiveSuwayomiNsfw }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(
            data_json(&r).contains("\"rederiveSuwayomiNsfw\":2"),
            "{}",
            data_json(&r)
        );
        async fn nsfw_of(pool: &SqlitePool, id: &str) -> i64 {
            sqlx::query_scalar("SELECT is_nsfw FROM work WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap()
        }
        assert_eq!(nsfw_of(&pool, "w_genre").await, 1, "'Mature' genre → NSFW");
        assert_eq!(nsfw_of(&pool, "w_srcflag").await, 1, "adult source → NSFW");
        assert_eq!(nsfw_of(&pool, "w_sfw").await, 0, "SFW work stays SFW");
        // Non-admin refused.
        assert!(!exec(
            &s,
            r#"mutation { rederiveSuwayomiNsfw }"#,
            Some("bobtok"),
            "1.1.1.1"
        )
        .await
        .errors
        .is_empty());
    }

    /// Build a minimal `AppState` around a migrated pool (Suwayomi points at a
    /// dead port; `map_series` never dials it, so read-shape tests stay offline).
    fn state_with_pool(pool: SqlitePool) -> std::sync::Arc<AppState> {
        std::sync::Arc::new(AppState {
            pool: pool.clone(),
            cover_pool: pool.clone(),
            suwayomi: crate::suwayomi::SuwayomiClient::new("http://127.0.0.1:1".into(), None, None),
            mangadex: std::sync::Arc::new(crate::mangadex::MangaDexClient::new(
                "test-ua", 5.0, 40.0,
            )),
            admin_users: vec![],
            scan_health: Mutex::new(ScanHealth::default()),
            auth_limiter: RateLimiter::new(100, 60),
            federated_limiter: RateLimiter::new(100, 60),
            session_ttl_secs: 30 * 24 * 60 * 60,
            series_inflight: KeyedLocks::default(),
            chapters_inflight: KeyedLocks::default(),
            cover_crawl_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            catalogue_cover_phash: false,
            ext_icons_dir: std::path::PathBuf::new(),
        })
    }

    /// REGRESSION — "the feed says 1 hour ago but the chapter is really days old".
    ///
    /// `latestChapterAt` is what the reader labels "released N ago". `latest_chapter_at`
    /// is NOT part of Suwayomi's wire shape, so a live-fetched manga always carries
    /// `None`, while `last_fetched_at` is Suwayomi's `lastFetchedAt` — stamped to NOW by
    /// our own `fetchManga: true` poll. The display path fell back from the first to the
    /// second, i.e. it published a POLL time as a RELEASE time. The stored column was
    /// never wrong (0 of 13,802 live rows held a clock value); only this mapping was.
    #[tokio::test]
    async fn latest_chapter_at_never_falls_back_to_the_poll_clock() {
        let pool = migrated_pool().await;
        let st = state_with_pool(pool.clone());
        let now_secs = chrono::Utc::now().timestamp();

        // A live-fetched manga: no latestChapterAt, lastFetchedAt = the poll we just made.
        let mut m = suwayomi_manga(500, "Poll Clock Fixture", &["Action"], "src1");
        m.last_fetched_at = Some(now_secs.to_string());
        let s = map_series(&st, m.clone()).await;
        assert!(
            !s.updated_at.is_empty(),
            "updatedAt still carries the poll time — that field genuinely means 'polled'"
        );
        assert_eq!(
            s.latest_chapter_at, "",
            "with no known newest-chapter time the server must say NOTHING, not 'now'; \
             the reader's firstDated() chain falls through to updatedAt on its own"
        );

        // Once the cache holds a real newest-chapter time, the page fill supplies it —
        // and it is the CHAPTER's time, days old, not the poll time.
        let three_days_ago_ms = (now_secs - 3 * 86_400) * 1000;
        sqlx::query(
            "INSERT INTO suwayomi_series (id, title, status, source_id, lang, in_library, \
               latest_chapter_at, updated_at) \
             VALUES (500, 'Poll Clock Fixture', 'ONGOING', 'src1', 'en', 1, ?, '2026-01-01T00:00:00Z')",
        )
        .bind(three_days_ago_ms.to_string())
        .execute(&pool)
        .await
        .unwrap();
        let s = map_series(&st, m).await;
        let got = chrono::DateTime::parse_from_rfc3339(&s.latest_chapter_at)
            .expect("a live-fetched series is hydrated from the cache, not left blank");
        assert_eq!(got.timestamp(), now_secs - 3 * 86_400);
        assert!(
            (now_secs - got.timestamp()) > 86_400,
            "the released time must be the chapter's, not this second's poll"
        );

        // A manga that already carries a value (i.e. read out of the cache) is never
        // overwritten by the page fill.
        let mut cached = suwayomi_manga(500, "Poll Clock Fixture", &["Action"], "src1");
        cached.latest_chapter_at = Some(((now_secs - 9 * 86_400) * 1000).to_string());
        let s = map_series(&st, cached).await;
        let got = chrono::DateTime::parse_from_rfc3339(&s.latest_chapter_at).unwrap();
        assert_eq!(got.timestamp(), now_secs - 9 * 86_400);
    }

    /// AD1: the raw poll override is exposed nullable, distinct from the folded
    /// effective value. With no admin row `pollEveryMinutesOverride` is null while
    /// `pollEveryMinutes` still reports the folded default (30); once an override
    /// is set the raw field echoes it. A row whose poll column is NULL (only a
    /// sibling override set) must keep the raw poll override null.
    #[tokio::test]
    async fn scan_policy_exposes_raw_poll_override_nullable() {
        let pool = migrated_pool().await;
        let st = state_with_pool(pool.clone());
        let m = suwayomi_manga(3, "AD1 Fixture", &["Action"], "src1");

        // No admin row: override is null, effective folds to the default.
        let scan = map_series(&st, m.clone()).await.scan;
        assert_eq!(scan.poll_every_minutes, 30, "effective folds to default");
        assert_eq!(
            scan.poll_every_minutes_override, None,
            "no admin row => raw poll override is null"
        );

        // A sibling-only override (poll column left NULL) must NOT pin poll.
        sqlx::query(
            "INSERT INTO series_admin \
               (series_id, override_interval_hours, poll_every_minutes, paused_override, status_override, updated_at) \
             VALUES ('3', 12.0, NULL, NULL, NULL, '2026-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let scan = map_series(&st, m.clone()).await.scan;
        assert_eq!(
            scan.poll_every_minutes, 30,
            "NULL poll still folds to default"
        );
        assert_eq!(
            scan.poll_every_minutes_override, None,
            "a NULL poll column stays an unset raw override"
        );

        // An explicit poll override echoes the raw value on both fields.
        sqlx::query("UPDATE series_admin SET poll_every_minutes = 45 WHERE series_id = '3'")
            .execute(&pool)
            .await
            .unwrap();
        let scan = map_series(&st, m).await.scan;
        assert_eq!(
            scan.poll_every_minutes, 45,
            "effective reflects the override"
        );
        assert_eq!(
            scan.poll_every_minutes_override,
            Some(45),
            "raw poll override echoes the explicit value"
        );
    }

    /// `updateSeriesAdmin` must VALIDATE before it writes: the id parse and the
    /// Suwayomi resolution both have to succeed before the `series_admin` upsert runs.
    /// Previously the upsert went first, so a `w_`-prefixed id persisted a junk
    /// `series_admin` row and then returned a masked "Internal error" — the admin was
    /// told nothing happened while a row had in fact been written.
    ///
    /// (The read-side null-poll semantics this test used to piggyback on are covered
    /// directly by `scan_policy_exposes_raw_poll_override_nullable`.)
    #[tokio::test]
    async fn update_series_admin_validates_before_writing() {
        let (s, pool) = setup_full(100).await;

        // A canonical `w_` id is not a Suwayomi series id: rejected with a real message.
        let r = exec(
            &s,
            r#"mutation { updateSeriesAdmin(input:{seriesId:"w_s1", overrideIntervalHours:12}) { id } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(
            first_error(&r).contains("numeric Suwayomi series id"),
            "expected a validation error, got {:?}",
            r.errors
        );

        // A numeric id whose Suwayomi lookup fails (no server in tests) also writes
        // nothing — resolution gates the upsert.
        let _ = exec(
            &s,
            r#"mutation { updateSeriesAdmin(input:{seriesId:"3", overrideIntervalHours:12}) { id } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM series_admin")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            rows, 0,
            "no series_admin row may be written before the id is validated"
        );
    }

    /// AD2: resolving a merge candidate must be an atomic claim, not a
    /// check-then-write TOCTOU. Once a candidate is accepted, a second resolve of
    /// the same id must fail with the already-resolved error and must NOT re-run
    /// the source_series repoint or the provisional-work delete — the guarded
    /// `WHERE ... AND status='pending'` UPDATE turns the second attempt into a
    /// no-op, so `resolved_at` stays exactly as the first (winning) call left it.
    #[tokio::test]
    async fn resolve_merge_candidate_is_an_atomic_claim() {
        let (s, pool) = setup_full(100).await;

        // Two works: the provisional one the source currently points at, and the
        // canonical target the candidate proposes merging onto.
        for wid in ["w_prov", "w_canon"] {
            sqlx::query(
                "INSERT INTO work (id, is_nsfw, created_at, updated_at) \
                 VALUES (?, 0, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            )
            .bind(wid)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('ss1', 'w_prov', 'suwayomi', 'k1', '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO merge_candidate \
             (id, source_series_id, candidate_work_id, score, method, status, created_at) \
             VALUES ('mc1', 'ss1', 'w_canon', 0.8, 'title_exact', 'pending', '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // First resolve (accept): repoints ss1 onto w_canon and drops the now-orphan
        // provisional work.
        let r = exec(
            &s,
            r#"mutation { resolveMergeCandidate(id: "mc1", accept: true) }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "first resolve failed: {:?}", r.errors);
        assert!(data_json(&r).contains("\"resolveMergeCandidate\":true"));

        let work_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = 'ss1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            work_id, "w_canon",
            "accept repoints the source onto the canonical work"
        );
        let prov_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work WHERE id = 'w_prov'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(prov_count, 0, "the orphaned provisional work is deleted");
        let (status, resolved_at): (String, Option<String>) =
            sqlx::query_as("SELECT status, resolved_at FROM merge_candidate WHERE id = 'mc1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "confirmed");
        let resolved_at = resolved_at.expect("resolved_at set on first resolve");

        // Second resolve of the SAME id: the guarded UPDATE matches zero pending
        // rows, so this is rejected and mutates nothing further.
        let r2 = exec(
            &s,
            r#"mutation { resolveMergeCandidate(id: "mc1", accept: true) }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(
            first_error(&r2),
            "This merge candidate is already resolved.",
            "the second resolve is refused"
        );

        // The claim made the second attempt a no-op: work_id unchanged, provisional
        // work still gone, and resolved_at identical to the winning call (a blind
        // re-UPDATE would have stamped a fresh timestamp here).
        let work_id2: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = 'ss1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            work_id2, "w_canon",
            "second resolve does not re-repoint the source"
        );
        let resolved_at2: Option<String> =
            sqlx::query_scalar("SELECT resolved_at FROM merge_candidate WHERE id = 'mc1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            resolved_at2.as_deref(),
            Some(resolved_at.as_str()),
            "resolved_at is untouched — the guarded UPDATE claimed nothing the second time"
        );
    }

    /// The review queue has to be reviewable: each row carries the WORK behind the
    /// source series (`sourceSeriesId` is a mapping id and cannot be looked up as a
    /// work) plus a cover for each side, and clicking either side must expand — even
    /// for a Suwayomi-only work, which is what most of this queue is and which
    /// `canonicalSeries` refuses outright.
    #[tokio::test]
    async fn merge_queue_rows_carry_covers_and_expand_for_suwayomi_only_works() {
        let (s, pool) = setup_full(100).await;

        // The source side: Suwayomi-only, no cached cover and no MangaDex anchor, so
        // there is nothing to point a cover URL at.
        sqlx::query(
            "INSERT INTO work (id, primary_title, is_nsfw, created_at, updated_at) \
             VALUES ('w_prov', 'Provisional Title', 0, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // The candidate side: a cached cover, so it resolves to the versioned path.
        sqlx::query(
            "INSERT INTO work (id, primary_title, author, is_nsfw, cover_cached_version, \
                               created_at, updated_at) \
             VALUES ('w_canon', 'Canonical Title', 'Some Author', 0, 3, \
                     '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO source_series (id, work_id, source_type, source_key, created_at) \
             VALUES ('ss1', 'w_prov', 'suwayomi', 'k1', '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO merge_candidate \
             (id, source_series_id, candidate_work_id, score, method, status, created_at) \
             VALUES ('mc1', 'ss1', 'w_canon', 0.8, 'title_exact', 'pending', '2020-01-01T00:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let r = exec(
            &s,
            r#"{ mergeQueue { items { sourceSeriesId sourceWorkId candidateWorkId
                 sourceCoverUrl candidateCoverUrl } total } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(r.errors.is_empty(), "mergeQueue failed: {:?}", r.errors);
        let json = data_json(&r);
        assert!(
            json.contains("\"sourceWorkId\":\"w_prov\""),
            "the row must expose the source's WORK, not just the mapping id: {json}"
        );
        assert!(
            json.contains("\"sourceSeriesId\":\"ss1\""),
            "the mapping id stays available: {json}"
        );
        assert!(
            json.contains("\"candidateCoverUrl\":\"/covers/w_canon.webp?v=3\""),
            "a cached cover resolves to its versioned path: {json}"
        );
        assert!(
            json.contains("\"sourceCoverUrl\":\"\""),
            "a work with no cached cover and no MangaDex anchor has no cover to show: {json}"
        );

        // Clicking the source side expands it. This is the whole reason the panel does
        // not reuse `canonicalSeries` — asserted directly below.
        let d = exec(
            &s,
            r#"{ workReviewDetail(workId: "w_prov") { workId title coverUrl chapterCount
                 sources { sourceType sourceKey } } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert!(
            d.errors.is_empty(),
            "workReviewDetail failed: {:?}",
            d.errors
        );
        let dj = data_json(&d);
        assert!(
            dj.contains("\"title\":\"Provisional Title\"") && dj.contains("\"sourceKey\":\"k1\""),
            "a Suwayomi-only work expands with its title and source mapping: {dj}"
        );
        assert!(
            dj.contains("\"coverUrl\":\"\"") && dj.contains("\"chapterCount\":0"),
            "nothing is invented for a work with no cover and no mirrored chapters: {dj}"
        );

        let refused = exec(
            &s,
            r#"{ canonicalSeries(workId: "w_prov") { id } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(
            first_error(&refused),
            "No such work",
            "the premise: canonicalSeries refuses the un-anchored work the panel must show"
        );
    }

    // ---- Sources & Extensions admin surface (EXT-1) ------------------------

    fn ext(
        pkg: &str,
        lang: &str,
        installed: bool,
        nsfw: bool,
    ) -> crate::suwayomi::ExtensionListEntry {
        crate::suwayomi::ExtensionListEntry {
            pkg_name: pkg.into(),
            name: pkg.into(),
            lang: lang.into(),
            version_name: "1.0".into(),
            is_installed: installed,
            has_update: false,
            is_nsfw: nsfw,
            icon_url: None,
            repo: None,
        }
    }

    #[test]
    fn filter_extensions_show_nsfw_posture_wins_over_explicit_filter() {
        let list = vec![ext("a", "en", false, false), ext("b", "en", false, true)];
        // Opted-out viewer never sees NSFW — even asking for nsfw:true yields none.
        let out = filter_extensions(list.clone(), false, None, Some(true), false);
        assert!(out.is_empty(), "opted-out viewer sees no NSFW extensions");
        // Opted-out default listing drops the NSFW entry.
        let out = filter_extensions(list.clone(), false, None, None, false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pkg_name, "a");
        // Opted-in viewer can filter to NSFW-only.
        let out = filter_extensions(list, false, None, Some(true), true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pkg_name, "b");
    }

    #[test]
    fn filter_extensions_installed_and_lang() {
        let list = vec![
            ext("a", "en", true, false),
            ext("b", "en", false, false),
            ext("c", "ja", true, false),
        ];
        let out = filter_extensions(list.clone(), true, None, None, true);
        assert_eq!(
            out.iter().map(|e| e.pkg_name.as_str()).collect::<Vec<_>>(),
            vec!["a", "c"],
            "installedOnly keeps only installed"
        );
        let out = filter_extensions(list, false, Some("ja"), None, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pkg_name, "c");
    }

    #[test]
    fn summarize_bulk_counts_decisions_and_failures() {
        let mk = |decision: &str| MatchResult {
            decision: decision.into(),
            work_id: "w_x".into(),
            matched_work_id: None,
            score: None,
            method: None,
            source_series_id: "ss_x".into(),
        };
        let entries = vec![
            BulkAddEntry {
                suwayomi_manga_id: ID("1".into()),
                result: Some(mk("new")),
                error: None,
            },
            BulkAddEntry {
                suwayomi_manga_id: ID("2".into()),
                result: Some(mk("auto_merge")),
                error: None,
            },
            BulkAddEntry {
                suwayomi_manga_id: ID("3".into()),
                result: Some(mk("review")),
                error: None,
            },
            BulkAddEntry {
                suwayomi_manga_id: ID("4".into()),
                result: Some(mk("existing")),
                error: None,
            },
            BulkAddEntry {
                suwayomi_manga_id: ID("5".into()),
                result: None,
                error: Some("boom".into()),
            },
        ];
        let r = summarize_bulk(entries);
        assert_eq!(r.total, 5);
        assert_eq!(r.succeeded, 4);
        assert_eq!(r.failed, 1);
        assert_eq!(r.new_works, 1);
        assert_eq!(r.auto_merged, 1);
        assert_eq!(r.queued_for_review, 1);
        assert_eq!(r.already_existing, 1);
    }

    #[test]
    fn federated_source_selection_dedupes_by_pkg_and_gates_nsfw() {
        use crate::suwayomi::SuwayomiSource;
        let src = |id: &str, lang: &str, nsfw: bool, pkg: Option<&str>| SuwayomiSource {
            id: id.into(),
            name: format!("src-{id}"),
            lang: lang.into(),
            is_nsfw: nsfw,
            icon_url: None,
            pkg_name: pkg.map(Into::into),
        };
        // Two per-language MangaDex sources (same pkg) + a separate SFW extension +
        // an NSFW extension + the local source (id "0").
        let sources = vec![
            src("0", "und", false, Some("local")),
            src("md-ja", "ja", true, Some("pkg.mangadex")),
            src("md-en", "en", true, Some("pkg.mangadex")),
            src("pill", "en", false, Some("pkg.pill")),
            src("adult", "en", true, Some("pkg.adult")),
        ];

        // Opted-out viewer: NSFW sources dropped, local dropped, one per pkg.
        let sfw = select_federated_sources(sources.clone(), false);
        let ids: Vec<&str> = sfw.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["pill"],
            "only the SFW non-local extension survives"
        );

        // Opted-in viewer: NSFW allowed, but MangaDex still deduped to ONE source,
        // and the English one wins the per-pkg pick.
        let all = select_federated_sources(sources, true);
        let ids: std::collections::HashSet<&str> = all.iter().map(|s| s.id.as_str()).collect();
        assert!(
            ids.contains("md-en"),
            "English MangaDex source is the pkg pick"
        );
        assert!(
            !ids.contains("md-ja"),
            "the other MangaDex language is deduped out"
        );
        assert!(ids.contains("pill") && ids.contains("adult"));
        assert!(!ids.contains("0"), "the local source is never fanned out");
        assert_eq!(all.len(), 3, "one source per extension pkg");
    }

    #[test]
    fn apply_search_filters_by_genre_and_rating() {
        let mk = |title: &str, genres: &[&str], avg: f64, count: i32| Series {
            id: ID(title.into()),
            title: title.into(),
            alt_titles: vec![],
            author: None,
            artist: None,
            description: None,
            genres: genres.iter().map(|g| g.to_string()).collect(),
            r#type: ComicType::Manga,
            status: SeriesStatus::Ongoing,
            cover_url: String::new(),
            source_id: "s".into(),
            chapter_count: 0,
            is_nsfw: false,
            rating: RatingSummary {
                average: avg,
                count,
                distribution: vec![0; 10],
            },
            scan: ScanPolicy {
                avg_interval_hours: 0.0,
                override_interval_hours: None,
                poll_every_minutes: 30,
                paused: false,
                status_override: None,
                paused_override: None,
                poll_every_minutes_override: None,
                last_scanned_at: None,
                next_scan_at: None,
            },
            created_at: String::new(),
            updated_at: String::new(),
            latest_chapter_at: String::new(),
            detected_at: None,
        };
        let items = vec![
            mk("Action8", &["Action", "Comedy"], 8.0, 3),
            mk("Drama5", &["Drama"], 5.0, 2),
            mk("Action3", &["Action"], 3.0, 1),
            mk("Unrated", &["Action"], 0.0, 0),
        ];

        // Genre filter (case-insensitive, ANY): only Action titles.
        let out = apply_search_filters(items.clone(), Some(&["action".into()]), None, None);
        let ids: Vec<&str> = out.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(ids, vec!["Action8", "Action3", "Unrated"]);

        // Rating range [4,10] excludes the 3.0 and the unrated (0.0).
        let out = apply_search_filters(items.clone(), None, Some(4.0), Some(10.0));
        let ids: Vec<&str> = out.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(ids, vec!["Action8", "Drama5"]);

        // Combined: Action AND rating >= 4 → only Action8.
        let out = apply_search_filters(items.clone(), Some(&["Action".into()]), Some(4.0), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Action8");

        // No filters → unchanged.
        assert_eq!(
            apply_search_filters(items.clone(), None, None, None).len(),
            4
        );
    }

    #[test]
    fn group_aggregated_chapters_dedupes_by_number_keeps_sources() {
        // S2: one entry per number, ascending, each keeping every source (translator)
        // that provides it.
        let row = |num: f64, st: &str, sid: &str, mid: Option<&str>, cid: &str| {
            crate::catalog::WorkChapterRow {
                number: num,
                title: Some(format!("Ch {num}")),
                source_type: st.into(),
                source_id: sid.into(),
                suwayomi_manga_id: mid.map(Into::into),
                chapter_id: cid.into(),
                scanlator: None,
            }
        };
        // Number 1 from two sources; 2 from one; 10.5 distinct from 10; out of order.
        let rows = vec![
            row(2.0, "suwayomi", "a", Some("333"), "c2"),
            row(1.0, "suwayomi", "a", Some("333"), "c1"),
            row(1.0, "mangadex", "mangadex", None, "md-uuid"),
            row(10.5, "suwayomi", "a", Some("333"), "c105"),
            row(10.0, "suwayomi", "a", Some("333"), "c10"),
        ];
        let out = group_aggregated_chapters(rows);
        let nums: Vec<f64> = out.iter().map(|c| c.number).collect();
        assert_eq!(nums, vec![1.0, 2.0, 10.0, 10.5], "deduped + ascending");
        // Number 1 kept both sources.
        let first = &out[0];
        assert_eq!(first.sources.len(), 2);
        assert!(first.sources.iter().any(|s| s.source_type == "mangadex"));
        assert!(first
            .sources
            .iter()
            .any(|s| s.suwayomi_manga_id.as_ref().map(|i| i.0.as_str()) == Some("333")));
    }

    #[test]
    fn rank_federated_hits_exact_title_first_stable() {
        // X4: exact-title matches sort first; ties keep incoming (source) order, so
        // the ranking is deterministic regardless of fan-out completion order.
        let mk = |id: i64, title: &str| crate::suwayomi::SuwayomiManga {
            id,
            title: title.into(),
            url: None,
            thumbnail_url: None,
            author: None,
            artist: None,
            description: None,
            genre: vec![],
            status: "ONGOING".into(),
            in_library: false,
            in_library_at: None,
            last_fetched_at: None,
            latest_chapter_at: None,
            source_id: "s".into(),
            source: None,
            chapters: None,
        };
        // Incoming already in source order: a fuzzy, then two exact, then fuzzy.
        let mut hits = vec![
            mk(1, "Naruto Gaiden"),
            mk(2, "Naruto"),
            mk(3, "NARUTO"), // normalizes equal to "naruto"
            mk(4, "Boruto"),
        ];
        rank_federated_hits(&mut hits, "naruto");
        let ids: Vec<i64> = hits.iter().map(|m| m.id).collect();
        // The two exact matches (2,3) come first in their original relative order,
        // then the non-exact ones (1,4) in original order.
        assert_eq!(ids, vec![2, 3, 1, 4]);
    }

    #[test]
    fn store_icon_url_derives_store_hosted_icons() {
        // Keiyoushi's canonicalized repo URL → OUR origin-relative icon path
        // (their `repo/icon/` dir is now empty; we serve the vendored snapshot).
        // The caller absolutizes against the public image base.
        assert_eq!(
            store_icon_url(
                "https://github.com/keiyoushi/extensions/raw/repo/index.pb",
                "eu.kanade.tachiyomi.extension.en.foo"
            )
            .as_deref(),
            Some("/ext-icons/eu.kanade.tachiyomi.extension.en.foo.png")
        );
        // The pre-canonicalization min.json form resolves to the same base.
        assert_eq!(
            store_icon_url(
                "https://raw.githubusercontent.com/keiyoushi/extensions/repo/index.min.json",
                "eu.kanade.tachiyomi.extension.all.mangadex"
            )
            .as_deref(),
            Some("/ext-icons/eu.kanade.tachiyomi.extension.all.mangadex.png")
        );
        // A Keiyoushi package that doesn't split into `{lang}.{dir}` → None, so the
        // caller falls back to the engine icon rather than emitting a bogus path.
        assert_eq!(
            store_icon_url(
                "https://github.com/keiyoushi/extensions/raw/repo/index.pb",
                "com.example.custom"
            ),
            None
        );
        assert_eq!(keiyoushi_icon_url("eu.kanade.tachiyomi.extension.en"), None);
        assert_eq!(
            keiyoushi_icon_url("eu.kanade.tachiyomi.extension.en.foo.bar"),
            None
        );
        // A non-Keiyoushi Mihon-format repo keeps the icons-next-to-index layout.
        assert_eq!(
            store_icon_url("https://example.com/repo/index.min.json", "pkg.x").as_deref(),
            Some("https://example.com/repo/icon/pkg.x.png")
        );
        // A repo URL that doesn't end in an index file → None (fall back to engine).
        assert_eq!(store_icon_url("https://example.com/store", "pkg.x"), None);
        assert_eq!(store_icon_url("", "pkg.x"), None);
    }

    #[tokio::test]
    async fn series_sources_batch_returns_provenance_in_input_order() {
        let (s, pool) = setup_full(100).await;
        // A canonical work with a MangaDex mapping + a Suwayomi mapping whose
        // source_key is the Suwayomi series id the console holds ("42").
        crate::catalog::upsert_work_from_mangadex(
            &pool,
            "md-y",
            &crate::catalog::WorkInput {
                primary_title: Some("Prov Work".into()),
                is_nsfw: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let work_id: String =
            sqlx::query_scalar("SELECT work_id FROM source_series WHERE source_key = 'md-y'")
                .fetch_one(&pool)
                .await
                .unwrap();
        crate::catalog::upsert_source_series(
            &pool, &work_id, "suwayomi", "2048", "42", None, false,
        )
        .await
        .unwrap();
        crate::catalog::upsert_source_extension(
            &pool,
            "2048",
            &crate::catalog::SourceExtensionInput {
                pkg_name: "pkg.prov".into(),
                repo_url: "https://r".into(),
                apk_name: None,
                version_code: None,
                lang: Some("en".into()),
                is_nsfw: false,
            },
        )
        .await
        .unwrap();

        let q = r#"{ seriesSourcesBatch(seriesIds: ["42", "999"]) {
            seriesId workId sources { sourceType sourceId extension { pkgName } } } }"#;
        let r = exec(&s, q, Some("admintok"), "1.1.1.1").await;
        assert!(r.errors.is_empty(), "unexpected: {:?}", r.errors);
        let data = r.data.into_json().unwrap();
        let groups = data["seriesSourcesBatch"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "one group per requested id, in order");
        // Catalogued series: linked work + both mappings, extension joined.
        assert_eq!(groups[0]["seriesId"], serde_json::json!("42"));
        assert_eq!(groups[0]["workId"], serde_json::json!(work_id));
        let sources = groups[0]["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0]["sourceType"], serde_json::json!("mangadex"));
        assert_eq!(sources[1]["sourceType"], serde_json::json!("suwayomi"));
        assert_eq!(
            sources[1]["extension"]["pkgName"],
            serde_json::json!("pkg.prov")
        );
        // Uncatalogued series: null work, empty sources.
        assert_eq!(groups[1]["seriesId"], serde_json::json!("999"));
        assert_eq!(groups[1]["workId"], serde_json::Value::Null);
        assert!(groups[1]["sources"].as_array().unwrap().is_empty());

        // Admin-gated: a non-admin viewer is refused.
        let r = exec(&s, q, Some("bobtok"), "1.1.1.1").await;
        assert_eq!(first_error(&r), "Admin access required");
    }

    #[tokio::test]
    async fn extension_surface_requires_admin() {
        // Every EXT-1 query/mutation is admin-gated — a signed-in non-admin (and
        // an anonymous caller) is refused before any Suwayomi round-trip (the
        // test client points at a dead port, so passing the gate would error
        // differently).
        let s = setup().await;
        for (q, who) in [
            (r#"{ extensions { pkgName } }"#, Some("bobtok")),
            (r#"{ extensions { pkgName } }"#, None),
            (r#"{ sources { id } }"#, Some("bobtok")),
            (r#"{ sources { id } }"#, None),
            (
                r#"{ sourceBrowse(sourceId: "1", type: POPULAR) { page } }"#,
                Some("bobtok"),
            ),
            (
                r#"mutation { addExtensionRepo(indexUrl: "https://example.com/index.json") }"#,
                Some("bobtok"),
            ),
            (
                r#"mutation { installExtension(pkgName: "x") { pkgName } }"#,
                Some("bobtok"),
            ),
            (
                r#"mutation { uninstallExtension(pkgName: "x") { pkgName } }"#,
                Some("bobtok"),
            ),
            (
                r#"mutation { updateExtension(pkgName: "x") { pkgName } }"#,
                Some("bobtok"),
            ),
            (
                r#"mutation { bulkAddSourceSeries(suwayomiMangaIds: ["1"]) { total } }"#,
                Some("bobtok"),
            ),
            (
                r#"mutation { setSeriesPaused(seriesId: "1", paused: false) { id } }"#,
                Some("bobtok"),
            ),
        ] {
            let r = exec(&s, q, who, "1.1.1.1").await;
            let msg = first_error(&r);
            assert!(
                msg == "Admin access required" || msg == "Not authenticated",
                "expected auth refusal for {q}, got: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn bulk_add_source_series_validates_input_size() {
        let s = setup().await;
        let r = exec(
            &s,
            r#"mutation { bulkAddSourceSeries(suwayomiMangaIds: []) { total } }"#,
            Some("admintok"),
            "1.1.1.1",
        )
        .await;
        assert_eq!(first_error(&r), "suwayomiMangaIds must not be empty");

        let ids = (0..101)
            .map(|i| format!("\"{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let q =
            format!(r#"mutation {{ bulkAddSourceSeries(suwayomiMangaIds: [{ids}]) {{ total }} }}"#);
        let r = exec(&s, &q, Some("admintok"), "1.1.1.1").await;
        assert_eq!(
            first_error(&r),
            "At most 100 ids per bulkAddSourceSeries call"
        );
    }
}
