use std::env;

/// Absolute legacy main database path for first-start import. Unset searches
/// beside the SQLite covers file. An empty or relative value is rejected at boot.
pub fn sqlite_import_path() -> Option<std::path::PathBuf> {
    env::var_os("KOMIKA_SQLITE_IMPORT_PATH").map(std::path::PathBuf::from)
}

/// A secret string that never prints its value in `Debug` output (Config is
/// logged wholesale at startup via `?cfg`).
#[derive(Clone, Default)]
pub struct Secret(pub Option<String>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "Some(<redacted>)"
        } else {
            "None"
        })
    }
}

/// Aggregate outbound scan concurrency across ALL of Phase E3's per-source loops
/// (`SCAN_GLOBAL_CONCURRENCY`, default 12).
///
/// **Why this is an env var and not a constant.** E3 replaced the single global scan loop
/// with one loop per source, each running `SCAN_CONCURRENCY = 3`. Per-source concurrency is
/// what protects an individual scanlator site from being hammered, and it is unchanged; the
/// *aggregate* is what this bounds. A release build of this server is ~13 minutes, so if
/// outbound load ever has to come down NOW — a source complaining, a ban warning, an
/// upstream incident — that must be a restart-level knob, not a rebuild. Setting it to 3
/// reproduces the pre-E3 aggregate exactly: one loop's worth of in-flight fetches.
///
/// Clamped to at least 1. A `Semaphore` with 0 permits never hands one out, so every source
/// loop would await forever and the scanner would silently stop — indistinguishable, from
/// the outside, from a healthy idle library.
///
/// Read once, when the supervisor starts, and logged there (`global_permits`) so the
/// effective ceiling is answerable from the logs alone.
///
/// A free function rather than a [`Config`] field on purpose: `scanner::supervisor` is handed
/// an `AppState`, which does not carry `Config`, so a field would have to be threaded through
/// `main` and `AppState` to reach the only consumer — and a `Config` field nothing reads is
/// dead weight that the compiler flags. Parsing still lives in this module so every env var
/// the server reads is documented in one place.
pub fn scan_global_concurrency() -> usize {
    env::var("SCAN_GLOBAL_CONCURRENCY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_SCAN_GLOBAL_CONCURRENCY)
        .max(1)
}

/// Default for [`scan_global_concurrency`]. Matches `deploy/docker-compose.yml`'s
/// `SCAN_GLOBAL_CONCURRENCY: ${SCAN_GLOBAL_CONCURRENCY:-12}`; the two must agree, or the
/// documented default and the effective one differ depending on how the server was started.
pub const DEFAULT_SCAN_GLOBAL_CONCURRENCY: usize = 12;

/// One entry of [`scan_source_rate_limits`]: a hard cap on the outbound HTTP requests the
/// scanner may send to ONE source in a rolling window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRateLimit {
    /// The Suwayomi `source_id` (the same value carried on `source_series.source_id` and
    /// `series_scan_state.source_id`), never a package or display name.
    pub source_id: String,
    /// Upstream HTTP requests allowed per `window_secs`.
    pub requests: u32,
    /// Rolling window length, seconds.
    pub window_secs: u64,
    /// How many upstream requests ONE series scan costs. A scan is Suwayomi's
    /// `fetchMangaAndChapters(fetchManga: true, fetchChapters: true)`, which the extension
    /// serves as a details request plus a chapter-list request — two site hits, even when
    /// both land on the same URL. Overridable per entry for a source known to answer both
    /// from one page.
    pub requests_per_scan: u32,
}

impl SourceRateLimit {
    /// Series scans that fit in one window, never below 1 (a cap smaller than a single scan
    /// still lets one scan through per window — the loop must not deadlock on a typo).
    pub fn scans_per_window(&self) -> u32 {
        (self.requests / self.requests_per_scan.max(1)).max(1)
    }
}

/// Default [`SourceRateLimit::requests_per_scan`] when an entry does not spell it out.
pub const DEFAULT_REQUESTS_PER_SCAN: u32 = 2;

/// Per-source outbound scan rate limits (`SCAN_SOURCE_RATE_LIMITS`).
///
/// Format: comma-separated `source_id=requests/window_seconds[@requests_per_scan]`, e.g.
/// `8358420357808595754=10/300` — at most 10 HTTP requests to that source per rolling
/// 5 minutes, i.e. 5 series scans, which `scanner::SourceThrottle` additionally spaces
/// evenly (one scan every 60 s) rather than bunching at the start of each window.
///
/// This exists because a WAF rule at a scanlator site counts requests per window, and the
/// per-source concurrency cap (`SCAN_CONCURRENCY = 3`) bounds parallelism, not rate: three
/// back-to-back 1 s scans are 180 requests a minute. Like `SCAN_GLOBAL_CONCURRENCY` it is a
/// restart-level knob, not a rebuild, because a ban warning cannot wait 13 minutes.
///
/// Malformed entries are skipped (and logged by the supervisor), never fatal: a typo must
/// degrade to "that one source is unthrottled", not "the server does not boot".
pub fn scan_source_rate_limits() -> Vec<SourceRateLimit> {
    env::var("SCAN_SOURCE_RATE_LIMITS")
        .ok()
        .map(|v| parse_scan_source_rate_limits(&v))
        .unwrap_or_default()
}

/// Pure parser behind [`scan_source_rate_limits`], split out so it is testable without
/// racing sibling tests over a process-global env var.
pub fn parse_scan_source_rate_limits(raw: &str) -> Vec<SourceRateLimit> {
    raw.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .filter_map(|entry| {
            let (source_id, rest) = entry.split_once('=')?;
            let source_id = source_id.trim();
            if source_id.is_empty() {
                return None;
            }
            let (rate, cost) = match rest.split_once('@') {
                Some((r, c)) => (r, Some(c)),
                None => (rest, None),
            };
            let (requests, window) = rate.split_once('/')?;
            let requests: u32 = requests.trim().parse().ok().filter(|r| *r > 0)?;
            let window_secs: u64 = window.trim().parse().ok().filter(|w| *w > 0)?;
            let requests_per_scan: u32 = match cost {
                Some(c) => c.trim().parse().ok().filter(|c| *c > 0)?,
                None => DEFAULT_REQUESTS_PER_SCAN,
            };
            Some(SourceRateLimit {
                source_id: source_id.to_string(),
                requests,
                window_secs,
                requests_per_scan,
            })
        })
        .collect()
}

/// Runtime configuration, sourced from env vars (see `.env.example`).
#[derive(Clone)]
pub struct Config {
    /// Exact media manifests are opt-in (`KOMIKA_MEDIA_MANIFESTS=on`).
    pub media_manifests: bool,
    pub media_signing_key_file: std::path::PathBuf,
    /// Address the public listener binds (`BIND_ADDR`, default `0.0.0.0`). An IPv4
    /// or IPv6 literal; `main` refuses to start on anything else rather than
    /// silently widening a loopback-only intent to every interface.
    pub bind_addr: String,
    pub port: u16,
    /// Address the `/metrics` listener binds (`KOMIKA_METRICS_BIND_ADDR`, default
    /// `127.0.0.1`). It is a separate listener, never the public router. Only a
    /// container sets this to `0.0.0.0`: Docker forwards a published port to the
    /// container's network address, not to its loopback, so the compose file
    /// widens the bind inside the container and keeps the HOST publish on
    /// 127.0.0.1. `main` refuses anything but an IP literal, like `bind_addr`.
    pub metrics_bind_addr: String,
    /// Port for the `/metrics` scrape endpoint. Default 9090, the de-facto
    /// Prometheus sidecar convention. Override with `KOMIKA_METRICS_PORT`.
    pub metrics_port: u16,
    pub database_url: String,
    /// SQLite URL for the SEPARATE cover-cache DB. Kept out of `database_url` on
    /// purpose: cover blobs are large and re-derivable from MangaDex, so this DB is
    /// not stored in PostgreSQL. Defaults to `sqlite://covers.sqlite3`; override
    /// with `COVERS_DATABASE_URL`.
    pub covers_database_url: String,
    /// Directory holding the vendored extension icons served at
    /// `/ext-icons/{pkgName}.png`. Populated from `assets/ext-icons/` in the repo
    /// (see `scripts/fetch-ext-icons.mjs`) and copied into the image by
    /// `deploy/server.Dockerfile`. Override with `EXT_ICONS_DIR`; a missing
    /// directory is not fatal — the route falls back to its lazy fetch from the
    /// extension's own repo (a self-hosted install ships no snapshot).
    pub ext_icons_dir: String,
    /// Optional folder holding the built web apps (`KOMIKA_WEB_DIR`): `reader/` is
    /// served at `/` and `admin/` at `/admin`, each with an `index.html` SPA
    /// fallback. Unset (the default) serves no static files at all.
    pub web_dir: Option<String>,
    /// Serve the image proxy at `/img` (`img_proxy`). `KOMIKA_IMG_PROXY=on|off`; when
    /// unset it follows `web_dir` — on for a server that serves the reader itself, off
    /// for the hosted API, which has the image Worker instead.
    pub img_proxy_enabled: bool,
    pub suwayomi_url: String,
    /// Publicly reachable base for Suwayomi image URLs (covers + pages). GraphQL
    /// federation uses the internal `suwayomi_url`, but image URLs are handed to
    /// the browser, so behind Docker/compose they must use a public host, not the
    /// internal `suwayomi:4567`. Falls back to `suwayomi_url` when unset.
    pub suwayomi_public_url: Option<String>,
    /// Pin a specific Suwayomi source id; otherwise an English source is auto-picked.
    pub source_id: Option<String>,
    /// Origins allowed by CORS (the reader dev/preview servers by default).
    pub cors_origins: Vec<String>,
    // Trusted reverse proxies (`TRUSTED_PROXY_CIDRS`) are read through the instance
    // settings instead (`instance::TRUSTED_PROXY_CIDRS`, env still wins), because
    // Cloudflare setup changes them at runtime; see `TrustedProxies` in main.rs.
    /// Usernames that are granted admin (for the "manga DB" console). These names
    /// are reserved from registration; existing non-admin promotion requires password proof.
    pub admin_users: Vec<String>,
    /// Password used to CREATE a configured admin account at startup when it does
    /// not yet exist, or to prove ownership before promoting an existing non-admin.
    /// None disables creation and promotion. Existing admins are never re-passworded.
    pub admin_password: Secret,
    /// Email for the primary provisioned admin account. Others get a synthesized
    /// `<username>@admin.local`. Only used when creating a missing admin account.
    pub admin_email: Option<String>,
    /// How often the background scan scheduler wakes to re-evaluate the library
    /// (default 1h). Per-series cadence is still adaptive via each series' `next_scan_at`.
    pub scan_tick_seconds: u64,
    /// How often the source-sync scheduler re-walks subscribed extensions to discover
    /// new series and reconcile library membership (default 1 day).
    pub source_sync_interval_seconds: u64,
    /// Extension store refresh/update cadence; 0 disables updates (default 6h).
    pub extension_auto_update_interval_seconds: u64,
    /// Max LATEST pages walked per source per sync pass (bounds a capped/looping walk).
    pub source_sync_max_pages: i64,
    /// Max failed login/register attempts per key within the rate-limit window
    /// before further attempts are rejected.
    pub auth_rate_limit_max: u32,
    /// Sliding window (seconds) over which `auth_rate_limit_max` is counted.
    pub auth_rate_limit_window_secs: u64,
    /// Max `searchAllSources` calls per user within its window before further
    /// calls are rejected (C1). Federated search fans out + writes, so it needs
    /// its own budget separate from auth.
    pub federated_rate_limit_max: u32,
    /// Sliding window (seconds) for `federated_rate_limit_max`.
    pub federated_rate_limit_window_secs: u64,
    /// Absolute lifetime of a session token (seconds). After this, the token
    /// stops resolving and the user must sign in again. Default 30 days.
    pub session_ttl_secs: i64,
    /// Serve the GraphiQL IDE on `GET /graphql` and allow schema introspection.
    /// Off by default so a public deployment exposes neither; enable only in dev.
    pub graphiql_enabled: bool,
    /// Global request budget for the MangaDex crawl (fleet-wide; shared egress IP).
    /// MangaDex's per-IP ceiling is ~5 req/s.
    pub mangadex_rate_per_sec: f64,
    /// Dedicated budget for MangaDex `/at-home` calls (per minute). This endpoint
    /// has its own ~40/min limit, far tighter than the global one. Default 40.
    pub mangadex_athome_per_min: f64,
    /// User-Agent sent to MangaDex (required by their API).
    pub mangadex_user_agent: String,
    /// Compute a cover perceptual-hash per work during the catalogue sync
    /// (CATALOGUE.md §4). Off by default — it adds one cover download per work, so
    /// it's opt-in on top of the catalogue sync via `COVER_PHASH=on` (it only ever
    /// runs inside a sync pass, which only runs with MangaDex usage on).
    pub catalogue_cover_phash: bool,
    /// Interval between recurring incremental catalogue/chapter refresh cycles
    /// (`updatedAtSince`). The first cycle seeds on startup; default 6h.
    pub catalogue_sync_interval_secs: u64,
    /// How often the `/chapter` firehose runs, independent of the catalogue sweep (Phase D).
    pub chapter_sync_interval_secs: u64,
    /// How often the wholesale feed chain runs as a drift reconciler (Phase C3).
    pub feed_reconcile_interval_secs: u64,
    /// How often LATEST-diff discovery (Phase E5) polls page 1 of each subscribed
    /// source and enqueues the series that moved up / entered. Default 15 min.
    pub discovery_interval_secs: u64,
    /// Recurring auto-enrichment drainer (X1). Off by default — it hits MangaDex
    /// (`/manga?ids[]` + one `/cover` per work), so it's opt-in via `METADATA_BACKFILL=on`,
    /// and then still only runs while the MangaDex switch is on (checked every tick).
    pub metadata_backfill_enabled: bool,
    /// Interval between auto-enrichment ticks (seconds). Default 5 min.
    pub metadata_backfill_interval_secs: u64,
    /// Works enriched per auto-enrichment tick. Kept small (one `/cover` request
    /// each) so a tick stays polite. Default 25.
    pub metadata_backfill_batch: i64,
    /// Automatic cover-cache drainer: fetch each catalogued work's cover once and
    /// store it as a WebP BLOB so the web reader serves covers off the CF Worker.
    /// Defaults ON (no manual trigger needed); `COVER_CACHE=off` disables it whole.
    /// Its MangaDex pass additionally follows the MangaDex switch; its Suwayomi
    /// passes do not touch MangaDex and run either way.
    pub cover_cache_enabled: bool,
    /// Interval between cover-drainer ticks (seconds). First tick fires on startup.
    /// Default 5 min.
    pub cover_cache_interval_secs: u64,
    /// Covers fetched + stored per drainer tick. Bounded so a tick stays polite
    /// (each is one MangaDex-CDN fetch under the shared limiter). Default 500.
    pub cover_cache_batch: i64,
    /// One-shot recompression of cover masters still stored in the pre-switch LOSSLESS
    /// encoding (see `cover_recompress`). Defaults ON; it terminates by itself once the
    /// walk completes, so leaving it on costs nothing after the first pass. Set
    /// COVER_RECOMPRESS=off to keep the CPU free (e.g. during an incident).
    pub cover_recompress_enabled: bool,
    /// Interval between recompress ticks (seconds).
    pub cover_recompress_interval_secs: u64,
    /// Masters examined per recompress tick. Sized so a tick's CPU stays a fraction of
    /// its interval: a re-encode is ~60 ms, so 200 per 60 s is ~20% of one core.
    pub cover_recompress_batch: i64,
}

impl Config {
    pub fn from_env() -> Self {
        let bind_addr = addr_or(env::var("BIND_ADDR").ok(), "0.0.0.0");
        let metrics_bind_addr = addr_or(env::var("KOMIKA_METRICS_BIND_ADDR").ok(), "127.0.0.1");
        let port = env::var("PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        let metrics_port = env::var("KOMIKA_METRICS_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(9090);
        let database_url =
            env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://localhost/komika".to_string());
        // The derived cover cache retains a local SQLite file independently of
        // the PostgreSQL database endpoint.
        let covers_database_url =
            env::var("COVERS_DATABASE_URL").unwrap_or_else(|_| derive_covers_url(&database_url));
        let ext_icons_dir =
            env::var("EXT_ICONS_DIR").unwrap_or_else(|_| "/srv/ext-icons".to_string());
        let web_dir = env::var("KOMIKA_WEB_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let img_proxy_enabled = match env::var("KOMIKA_IMG_PROXY")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
        {
            Some(v) => v == "on" || v == "1" || v == "true",
            None => web_dir.is_some(),
        };
        let suwayomi_url =
            env::var("SUWAYOMI_URL").unwrap_or_else(|_| "http://localhost:4567".to_string());
        let suwayomi_public_url = env::var("SUWAYOMI_PUBLIC_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let source_id = env::var("SUWAYOMI_SOURCE_ID")
            .ok()
            .filter(|s| !s.is_empty());
        let cors_origins = env::var("CORS_ORIGINS")
            .unwrap_or_else(|_| {
                "http://localhost:5173,http://localhost:4173,http://tauri.localhost".to_string()
            })
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let admin_users = env::var("KOMIKA_ADMIN_USERS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let admin_password = Secret(
            env::var("KOMIKA_ADMIN_PASSWORD")
                .ok()
                .filter(|s| !s.is_empty()),
        );
        let admin_email = env::var("KOMIKA_ADMIN_EMAIL")
            .ok()
            .filter(|s| !s.is_empty());
        // 1 hour (3600s). The scheduler wakes this often to re-evaluate the library;
        // each series still has its own adaptive `next_scan_at`, so it's only re-fetched
        // when actually due. Hourly (vs the old 5-minute default) keeps the per-tick
        // O(library) sweep light on large libraries (100k+ series).
        let scan_tick_seconds = env::var("SCAN_TICK_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(3600);
        // 1 day (86400s). Discovery of newly-added source series is far less time-
        // sensitive than chapter updates and each pass browses upstream, so a daily
        // cadence keeps it cheap even with many subscribed extensions.
        let source_sync_interval_seconds = env::var("SOURCE_SYNC_INTERVAL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(86400);
        let extension_auto_update_interval_seconds =
            env::var("EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(21600);
        let source_sync_max_pages = env::var("SOURCE_SYNC_MAX_PAGES")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(10);
        let auth_rate_limit_max = env::var("AUTH_RATE_LIMIT_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(10);
        let auth_rate_limit_window_secs = env::var("AUTH_RATE_LIMIT_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(300);
        let federated_rate_limit_max = env::var("FEDERATED_RATE_LIMIT_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(20);
        let federated_rate_limit_window_secs = env::var("FEDERATED_RATE_LIMIT_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(60);
        let session_ttl_secs = env::var("SESSION_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v: &i64| v > 0)
            .unwrap_or(30 * 24 * 60 * 60);
        let graphiql_enabled = env::var("GRAPHIQL_ENABLED")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "on" || v == "1" || v == "true"
            })
            .unwrap_or(false);
        // `CATALOGUE_SYNC` is not read here any more: `instance::catalogue_crawl` decides
        // the crawl from it (same on/off spelling), and `instance::resolve_mangadex` lets
        // `CATALOGUE_SYNC=on` force MangaDex usage on. Both are resolved in `main`.
        let mangadex_rate_per_sec = env::var("MANGADEX_RATE_PER_SEC")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v: &f64| v > 0.0)
            // ~4 req/s leaves headroom under MangaDex's 5 req/s ceiling so a burst
            // (or reader at-home traffic sharing the budget) doesn't sit on the
            // ban threshold (M2).
            .unwrap_or(4.0);
        let mangadex_athome_per_min = env::var("MANGADEX_ATHOME_PER_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v: &f64| v > 0.0)
            .unwrap_or(40.0);
        let mangadex_user_agent = env::var("MANGADEX_USER_AGENT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Komika/0.1 (+https://github.com/komika)".to_string());
        let catalogue_cover_phash = env::var("COVER_PHASH")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "on" || v == "1" || v == "true"
            })
            .unwrap_or(false);
        let catalogue_sync_interval_secs = env::var("CATALOGUE_SYNC_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(6 * 60 * 60);
        // Phase D. The chapter firehose gets its OWN cadence, decoupled from the catalogue
        // sweep's. The two halves have wildly different costs — an incremental chapter pass
        // is ~75 chapters, a catalogue sweep is thousands of records — and tying them
        // together is why a brand-new chapter could surface already labelled "5h ago".
        //
        // 15 MINUTES IS ONLY SAFE BECAUSE OF PHASE C. This tick used to drag the ~20-25 s
        // wholesale refresh chain behind it; firing that 96×/day instead of 4× would push
        // the scanner's writes past the pool's 15 s `busy_timeout` into SQLITE_BUSY. C2 gave
        // both feed halves incremental writers and C3 moved the chain onto its own daily
        // reconcile tick, so what runs here now is the firehose plus a handful of one-row
        // upserts.
        let chapter_sync_interval_secs = env::var("CHAPTER_SYNC_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(15 * 60);
        // Phase C3. The demoted wholesale chain, now a drift REPORTER rather than the
        // mechanism. Daily.
        let feed_reconcile_interval_secs = env::var("FEED_RECONCILE_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(24 * 60 * 60);
        // Phase E5. LATEST-diff discovery cadence. 15 min = the recommended variant
        // (~2,643 upstream fetches/day vs ~22,292 polling; §8e). Detection latency is
        // bounded by this interval.
        let discovery_interval_secs = env::var("DISCOVERY_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(15 * 60);
        let metadata_backfill_enabled = env::var("METADATA_BACKFILL")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "on" || v == "1" || v == "true"
            })
            .unwrap_or(false);
        let metadata_backfill_interval_secs = env::var("METADATA_BACKFILL_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(300);
        let metadata_backfill_batch = env::var("METADATA_BACKFILL_BATCH")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v: &i64| v > 0)
            .unwrap_or(25);
        // Cover cache drainer defaults ON (unlike the other MangaDex-hitting tasks):
        // covers should populate automatically with no manual admin trigger. Set
        // COVER_CACHE=off to disable (e.g. on non-primary replicas).
        let cover_cache_enabled = env::var("COVER_CACHE")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v == "off" || v == "0" || v == "false")
            })
            .unwrap_or(true);
        let cover_cache_interval_secs = env::var("COVER_CACHE_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(300);
        let cover_cache_batch = env::var("COVER_CACHE_BATCH")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v: &i64| v > 0)
            .unwrap_or(500);
        // Cover recompression defaults ON for the same reason as the drainer: it is
        // maintenance nobody should have to remember to trigger. It hits no external
        // service, so unlike the drainer it is safe on every replica.
        let cover_recompress_enabled = env::var("COVER_RECOMPRESS")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v == "off" || v == "0" || v == "false")
            })
            .unwrap_or(true);
        let cover_recompress_interval_secs = env::var("COVER_RECOMPRESS_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(60);
        let cover_recompress_batch = env::var("COVER_RECOMPRESS_BATCH")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v: &i64| v > 0)
            .unwrap_or(200);
        Self {
            media_manifests: env::var("KOMIKA_MEDIA_MANIFESTS").is_ok_and(|v| v == "on"),
            media_signing_key_file: env::var_os("KOMIKA_MEDIA_SIGNING_KEY_FILE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "media-signing.key".into()),
            bind_addr,
            port,
            metrics_bind_addr,
            metrics_port,
            database_url,
            suwayomi_url,
            suwayomi_public_url,
            source_id,
            cors_origins,
            admin_users,
            admin_password,
            admin_email,
            scan_tick_seconds,
            source_sync_interval_seconds,
            extension_auto_update_interval_seconds,
            source_sync_max_pages,
            auth_rate_limit_max,
            auth_rate_limit_window_secs,
            federated_rate_limit_max,
            federated_rate_limit_window_secs,
            session_ttl_secs,
            graphiql_enabled,
            mangadex_rate_per_sec,
            mangadex_athome_per_min,
            mangadex_user_agent,
            catalogue_cover_phash,
            catalogue_sync_interval_secs,
            chapter_sync_interval_secs,
            feed_reconcile_interval_secs,
            discovery_interval_secs,
            metadata_backfill_enabled,
            metadata_backfill_interval_secs,
            metadata_backfill_batch,
            cover_cache_enabled,
            cover_cache_interval_secs,
            cover_cache_batch,
            cover_recompress_enabled,
            cover_recompress_interval_secs,
            cover_recompress_batch,
            covers_database_url,
            ext_icons_dir,
            web_dir,
            img_proxy_enabled,
        }
    }
}

/// PostgreSQL endpoints default to the local `covers.sqlite3` cache. Legacy
/// SQLite URLs still derive a sibling file for offline/source tooling. Handles `sqlite:///abs/dir/komika.sqlite3`, `sqlite://rel.sqlite3`, and
/// `sqlite:rel.sqlite3`. A `:memory:` URL (tests only — prod is always a file) maps
/// to its own in-memory DB.
/// A listener address from an env value: trimmed, with `default` when unset or blank.
fn addr_or(raw: Option<String>, default: &str) -> String {
    raw.map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn derive_covers_url(database_url: &str) -> String {
    if database_url.starts_with("postgres:") || database_url.starts_with("postgresql:") {
        return "sqlite://covers.sqlite3".to_string();
    }
    if database_url.contains(":memory:") {
        return "sqlite::memory:".to_string();
    }
    if let Some(i) = database_url.rfind('/') {
        return format!("{}/covers.sqlite3", &database_url[..i]);
    }
    // No path separator, e.g. `sqlite:komika.sqlite3` — swap after the scheme colon.
    if let Some(i) = database_url.rfind(':') {
        return format!("{}:covers.sqlite3", &database_url[..i]);
    }
    "covers.sqlite3".to_string()
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind_addr", &self.bind_addr)
            .field("port", &self.port)
            .field("metrics_bind_addr", &self.metrics_bind_addr)
            .field("metrics_port", &self.metrics_port)
            .field("database_url", &"<redacted>")
            .field("covers_database_url", &self.covers_database_url)
            .finish_non_exhaustive()
    }
}

/// PostgreSQL session limits. Values are milliseconds; zero/invalid overrides
/// fall back to the bounded default rather than disabling a timeout.
#[derive(Clone, Copy, Debug)]
pub struct DatabaseSessionLimits {
    pub statement_ms: u64,
    pub lock_ms: u64,
    pub idle_transaction_ms: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct DatabaseLimits {
    pub read_connections: u32,
    pub foreground_connections: u32,
    pub readers: DatabaseSessionLimits,
    pub foreground: DatabaseSessionLimits,
    pub background: DatabaseSessionLimits,
}

impl Default for DatabaseLimits {
    fn default() -> Self {
        Self {
            read_connections: 16,
            foreground_connections: 8,
            readers: DatabaseSessionLimits {
                statement_ms: 30_000,
                lock_ms: 5_000,
                idle_transaction_ms: 60_000,
            },
            foreground: DatabaseSessionLimits {
                statement_ms: 15_000,
                lock_ms: 5_000,
                idle_transaction_ms: 30_000,
            },
            background: DatabaseSessionLimits {
                statement_ms: 1_800_000,
                lock_ms: 60_000,
                idle_transaction_ms: 900_000,
            },
        }
    }
}

impl DatabaseLimits {
    pub fn from_env() -> Self {
        Self::parse(|name| env::var(name).ok())
    }

    fn parse(get: impl Fn(&str) -> Option<String>) -> Self {
        fn positive(get: &impl Fn(&str) -> Option<String>, name: &str, default: u64) -> u64 {
            get(name)
                .and_then(|v| v.trim().parse().ok())
                .filter(|v| *v > 0 && *v <= i32::MAX as u64)
                .unwrap_or(default)
        }
        let mut limits = Self {
            read_connections: positive(&get, "DATABASE_READ_POOL_SIZE", 16) as u32,
            foreground_connections: positive(&get, "DATABASE_FOREGROUND_POOL_SIZE", 8) as u32,
            ..Self::default()
        };
        for (lane, session) in [
            ("READ", &mut limits.readers),
            ("FOREGROUND", &mut limits.foreground),
            ("BACKGROUND", &mut limits.background),
        ] {
            session.statement_ms = positive(
                &get,
                &format!("DATABASE_{lane}_STATEMENT_TIMEOUT_MS"),
                session.statement_ms,
            );
            session.lock_ms = positive(
                &get,
                &format!("DATABASE_{lane}_LOCK_TIMEOUT_MS"),
                session.lock_ms,
            );
            session.idle_transaction_ms = positive(
                &get,
                &format!("DATABASE_{lane}_IDLE_TRANSACTION_TIMEOUT_MS"),
                session.idle_transaction_ms,
            );
        }
        limits
    }
}

#[cfg(test)]
mod database_limit_tests {
    use super::*;

    #[test]
    fn database_limits_defaults_and_overrides_are_bounded() {
        let defaults = DatabaseLimits::parse(|_| None);
        assert_eq!(
            defaults.read_connections + defaults.foreground_connections + 1,
            25
        );
        assert_eq!(defaults.foreground.statement_ms, 15_000);
        assert_eq!(defaults.background.idle_transaction_ms, 900_000);
        let overridden = DatabaseLimits::parse(|name| match name {
            "DATABASE_FOREGROUND_POOL_SIZE" => Some("3".into()),
            "DATABASE_FOREGROUND_LOCK_TIMEOUT_MS" => Some("50".into()),
            "DATABASE_READ_STATEMENT_TIMEOUT_MS" => Some("0".into()),
            "DATABASE_BACKGROUND_LOCK_TIMEOUT_MS" => Some("invalid".into()),
            _ => None,
        });
        assert_eq!(overridden.foreground_connections, 3);
        assert_eq!(overridden.foreground.lock_ms, 50);
        assert_eq!(overridden.readers.statement_ms, 30_000);
        assert_eq!(overridden.background.lock_ms, 60_000);
    }
}

#[cfg(test)]
mod listener_addr_tests {
    use super::addr_or;

    #[test]
    fn metrics_bind_addr_defaults_to_loopback_and_takes_an_override() {
        assert_eq!(addr_or(None, "127.0.0.1"), "127.0.0.1");
        assert_eq!(addr_or(Some("  ".into()), "127.0.0.1"), "127.0.0.1");
        assert_eq!(addr_or(Some(" 0.0.0.0 ".into()), "127.0.0.1"), "0.0.0.0");
        assert_eq!(addr_or(Some("::1".into()), "127.0.0.1"), "::1");
    }
}
