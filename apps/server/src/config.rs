use std::env;

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

/// Runtime configuration, sourced from env vars (see `.env.example`).
#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    /// SQLite URL for the SEPARATE cover-cache DB. Kept out of `database_url` on
    /// purpose: cover blobs are large and re-derivable from MangaDex, so this DB is
    /// NOT Litestream-replicated (only `database_url` is). Defaults to a
    /// `covers.sqlite3` sibling of the main DB; override with `COVERS_DATABASE_URL`.
    pub covers_database_url: String,
    /// Directory holding the vendored extension icons served at
    /// `/ext-icons/{pkgName}.png`. Populated from `assets/ext-icons/` in the repo
    /// (see `scripts/fetch-ext-icons.mjs`) and copied into the image by
    /// `deploy/server.Dockerfile`. Override with `EXT_ICONS_DIR`; a missing
    /// directory is not fatal — the route falls back to its lazy Keiyoushi fetch.
    pub ext_icons_dir: String,
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
    /// CIDR blocks of trusted reverse proxies. `X-Forwarded-For`/`X-Real-IP` are
    /// honored for the rate-limit key ONLY when the direct socket peer is inside
    /// one of these; empty (default) means always use the socket peer, matching
    /// the shipped compose that publishes the server port directly.
    pub trusted_proxy_cidrs: Vec<String>,
    /// Usernames that are granted admin (for the "manga DB" console). These names
    /// are reserved from open registration and provisioned/promoted at startup.
    pub admin_users: Vec<String>,
    /// Password used to CREATE a configured admin account at startup when it does
    /// not yet exist (existing accounts are only promoted, never re-passworded).
    /// None disables account creation — admins must already exist to be promoted.
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
    /// Enable the direct-MangaDex catalogue sync (CATALOGUE.md §5). Off by default —
    /// nothing hits MangaDex unless `CATALOGUE_SYNC=on`.
    pub catalogue_sync_enabled: bool,
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
    /// it's opt-in on top of `CATALOGUE_SYNC` via `COVER_PHASH=on`.
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
    /// (`/manga?ids[]` + one `/cover` per work), so it's opt-in via `METADATA_BACKFILL=on`.
    pub metadata_backfill_enabled: bool,
    /// Interval between auto-enrichment ticks (seconds). Default 5 min.
    pub metadata_backfill_interval_secs: u64,
    /// Works enriched per auto-enrichment tick. Kept small (one `/cover` request
    /// each) so a tick stays polite. Default 25.
    pub metadata_backfill_batch: i64,
    /// Automatic cover-cache drainer: fetch each catalogued work's cover once and
    /// store it as a WebP BLOB so the web reader serves covers off the CF Worker.
    /// Defaults ON (no manual trigger needed); `COVER_CACHE=off` disables it.
    pub cover_cache_enabled: bool,
    /// Interval between cover-drainer ticks (seconds). First tick fires on startup.
    /// Default 5 min.
    pub cover_cache_interval_secs: u64,
    /// Covers fetched + stored per drainer tick. Bounded so a tick stays polite
    /// (each is one MangaDex-CDN fetch under the shared limiter). Default 500.
    pub cover_cache_batch: i64,
}

impl Config {
    pub fn from_env() -> Self {
        let port = env::var("PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        let database_url =
            env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://komika.sqlite3".to_string());
        // Cover-cache DB lives next to the main DB by default (a `covers.sqlite3`
        // sibling), but is a distinct file so Litestream (which replicates only
        // `database_url`) never ships cover bytes to R2.
        let covers_database_url =
            env::var("COVERS_DATABASE_URL").unwrap_or_else(|_| derive_covers_url(&database_url));
        let ext_icons_dir =
            env::var("EXT_ICONS_DIR").unwrap_or_else(|_| "/srv/ext-icons".to_string());
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
        let trusted_proxy_cidrs = env::var("TRUSTED_PROXY_CIDRS")
            .unwrap_or_default()
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
        let catalogue_sync_enabled = env::var("CATALOGUE_SYNC")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "on" || v == "1" || v == "true"
            })
            .unwrap_or(false);
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
        Self {
            port,
            database_url,
            suwayomi_url,
            suwayomi_public_url,
            source_id,
            cors_origins,
            trusted_proxy_cidrs,
            admin_users,
            admin_password,
            admin_email,
            scan_tick_seconds,
            source_sync_interval_seconds,
            source_sync_max_pages,
            auth_rate_limit_max,
            auth_rate_limit_window_secs,
            federated_rate_limit_max,
            federated_rate_limit_window_secs,
            session_ttl_secs,
            graphiql_enabled,
            catalogue_sync_enabled,
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
            covers_database_url,
            ext_icons_dir,
        }
    }
}

/// Place `covers.sqlite3` next to the main DB file, preserving the URL's scheme
/// prefix. Handles `sqlite:///abs/dir/komika.sqlite3`, `sqlite://rel.sqlite3`, and
/// `sqlite:rel.sqlite3`. A `:memory:` URL (tests only — prod is always a file) maps
/// to its own in-memory DB.
fn derive_covers_url(database_url: &str) -> String {
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
