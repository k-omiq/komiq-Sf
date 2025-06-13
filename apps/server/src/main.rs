mod auth;
mod avatar;
mod browse;
mod catalog;
mod chapter_label;
mod config;
mod cover;
mod db;
mod dedup;
mod discovery;
mod ext_icon;
mod gc;
mod graphql;
mod ingest;
mod mangadex;
mod media;
mod notify;
mod phase_f;
mod phash;
mod scanner;
mod series_cache;
mod suwayomi;
mod sync;
mod task;
mod views;

use std::sync::Arc;

use std::net::{IpAddr, SocketAddr};

use async_graphql::http::GraphiQLSource;
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, FromRef, Multipart, Path as UrlPath, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

use config::Config;
use graphql::{
    build_schema, ApiSchema, AppState, ClientIp, KeyedLocks, RateLimiter, RequestAuth,
    RequestLibraryCache, RequestUserCache, ScanHealth,
};
use suwayomi::SuwayomiClient;

/// Extract a bearer token from the `Authorization` header.
fn bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// A CIDR block (IPv4 or IPv6) used to recognize trusted reverse proxies.
#[derive(Clone, Copy, Debug)]
struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parse `a.b.c.d/nn`, `a:b::/nn`, or a bare IP (treated as a /32 or /128
    /// host route). Returns `None` for malformed input or an out-of-range prefix.
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p.trim().parse::<u8>().ok()?)),
            None => (s, None),
        };
        let network: IpAddr = addr.trim().parse().ok()?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        if prefix > max {
            return None;
        }
        Some(Cidr { network, prefix })
    }

    /// Whether `ip` falls inside this block. A v4 block never matches a v6
    /// address (and vice versa); callers should canonicalize v4-mapped v6 first.
    fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(net), IpAddr::V4(addr)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                u32::from(net) & mask == u32::from(addr) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(addr)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                u128::from(net) & mask == u128::from(addr) & mask
            }
            _ => false,
        }
    }
}

/// Parse a list of CIDR strings, logging (and dropping) any that don't parse.
fn parse_trusted_proxies(raw: &[String]) -> Vec<Cidr> {
    raw.iter()
        .filter_map(|s| match Cidr::parse(s) {
            Some(c) => Some(c),
            None => {
                tracing::warn!(cidr = %s, "ignoring malformed TRUSTED_PROXY_CIDRS entry");
                None
            }
        })
        .collect()
}

/// Resolve the client IP for rate-limiting. Forwarding headers are honored ONLY
/// when the direct socket peer is a configured trusted proxy
/// (`TRUSTED_PROXY_CIDRS`); otherwise they are trivially spoofable, so we key on
/// the socket peer. The default (empty allowlist) always uses the peer.
///
/// Header precedence, and why it is NOT the leftmost `X-Forwarded-For` hop:
/// Cloudflare *appends* the true client address to whatever `X-Forwarded-For` the
/// client sent, so a request carrying `X-Forwarded-For: 1.2.3.4` reaches this
/// origin as `1.2.3.4, <real-ip>`. Trusting the leftmost hop would therefore let
/// any caller pick their own rate-limit bucket and evade the auth limiter
/// entirely. We prefer `CF-Connecting-IP`, which Cloudflare unconditionally
/// overwrites with the true client address, then fall back to the *rightmost*
/// `X-Forwarded-For` hop (the one our own edge appended, i.e. the only hop a
/// client cannot forge), then `X-Real-IP`.
fn resolve_client_ip(headers: &HeaderMap, peer: SocketAddr, trusted: &[Cidr]) -> String {
    // Canonicalize v4-mapped v6 (e.g. `::ffff:127.0.0.1`) so a v4 CIDR matches a
    // dual-stack socket peer.
    let peer_ip = peer.ip().to_canonical();
    if trusted.iter().any(|c| c.contains(peer_ip)) {
        if let Some(cf) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
        {
            let ip = cf.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(last) = xff.split(',').next_back() {
                let ip = last.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
        if let Some(rip) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            let ip = rip.trim();
            if !ip.is_empty() {
                return ip.to_string();
            }
        }
    }
    peer_ip.to_string()
}

/// Combined router state: the GraphQL schema (for `/graphql`) and the DB pool
/// (for the readiness probe). Each handler extracts just the piece it needs.
#[derive(Clone)]
struct RouterState {
    schema: ApiSchema,
    pool: sqlx::SqlitePool,
    /// Un-replicated cover-cache DB (`work_cover_blob`); served by `serve_cover`.
    /// A newtype (`CoverDb`) distinguishes it from the main `pool` in `FromRef`.
    cover_pool: sqlx::SqlitePool,
    trusted_proxies: Arc<Vec<Cidr>>,
    /// Per-user sliding-window limiter for the CPU-bound upload routes
    /// (`/avatar`, `/comment-media`), so a single account can't flood the
    /// decode/resize/encode blocking pool.
    upload_limiter: Arc<RateLimiter>,
    /// Shared app state — for the Suwayomi image proxy (`serve_suwayomi_image`),
    /// which streams cover/page bytes from the internal loopback engine.
    app_state: Arc<graphql::AppState>,
}
impl FromRef<RouterState> for ApiSchema {
    fn from_ref(s: &RouterState) -> Self {
        s.schema.clone()
    }
}
impl FromRef<RouterState> for sqlx::SqlitePool {
    fn from_ref(s: &RouterState) -> Self {
        s.pool.clone()
    }
}
/// Newtype for the cover-cache pool so a handler can extract it via `State`
/// without colliding with the main-pool `FromRef<RouterState> for SqlitePool`.
#[derive(Clone)]
struct CoverDb(sqlx::SqlitePool);
impl FromRef<RouterState> for CoverDb {
    fn from_ref(s: &RouterState) -> Self {
        CoverDb(s.cover_pool.clone())
    }
}
impl FromRef<RouterState> for Arc<Vec<Cidr>> {
    fn from_ref(s: &RouterState) -> Self {
        s.trusted_proxies.clone()
    }
}
impl FromRef<RouterState> for Arc<RateLimiter> {
    fn from_ref(s: &RouterState) -> Self {
        s.upload_limiter.clone()
    }
}
impl FromRef<RouterState> for Arc<graphql::AppState> {
    fn from_ref(s: &RouterState) -> Self {
        s.app_state.clone()
    }
}

async fn graphql_handler(
    State(schema): State<ApiSchema>,
    State(trusted): State<Arc<Vec<Cidr>>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    req: GraphQLRequest,
) -> GraphQLResponse {
    let auth = RequestAuth(bearer(&headers));
    let ip = ClientIp(Some(resolve_client_ip(&headers, peer, &trusted)));
    // Fresh per-request caches so `current_user` does at most one session lookup and each
    // series' `user_library` row is read at most once, even when many resolvers ask (one
    // per feed item).
    let user_cache = RequestUserCache::default();
    let library_cache = RequestLibraryCache::default();
    schema
        .execute(
            req.into_inner()
                .data(auth)
                .data(ip)
                .data(user_cache)
                .data(library_cache),
        )
        .await
        .into()
}

async fn graphiql() -> impl IntoResponse {
    Html(GraphiQLSource::build().endpoint("/graphql").finish())
}

/// A GraphQL-shaped JSON error body for the REST avatar routes, so the reader's
/// error handling reads the same `message` field it does from `/graphql`.
fn avatar_error(status: StatusCode, message: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "message": message }))).into_response()
}

/// `POST /avatar` — authenticated multipart avatar upload. The bytes are decoded,
/// squared, and re-encoded as budgeted lossless WebP (`avatar::process_avatar`),
/// stored as a BLOB in `user_avatars`, and the resulting path stored on the user
/// row. Returns `{ "avatarUrl": "/avatars/<id>.webp?v=<ts>" }`.
async fn upload_avatar(
    State(pool): State<sqlx::SqlitePool>,
    State(limiter): State<Arc<RateLimiter>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> axum::response::Response {
    let Some(tok) = bearer(&headers) else {
        return avatar_error(StatusCode::UNAUTHORIZED, "Not authenticated");
    };
    let user = match auth::user_for_token(&pool, &tok).await {
        Ok(Some(u)) => u,
        Ok(None) => return avatar_error(StatusCode::UNAUTHORIZED, "Not authenticated"),
        Err(e) => {
            tracing::warn!(error = %e, "avatar upload: token lookup failed");
            return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
        }
    };
    if limiter.check(&format!("upload:{}", user.id)).is_err() {
        return avatar_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many uploads — please slow down",
        );
    }

    // Take the first file part (the reader sends a single `avatar` field).
    let mut data: Option<Vec<u8>> = None;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let is_file = field.name() == Some("avatar") || field.file_name().is_some();
                if is_file {
                    match field.bytes().await {
                        Ok(b) => {
                            data = Some(b.to_vec());
                            break;
                        }
                        Err(_) => {
                            return avatar_error(
                                StatusCode::BAD_REQUEST,
                                "Upload too large or could not be read",
                            )
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(_) => return avatar_error(StatusCode::BAD_REQUEST, "Malformed upload"),
        }
    }
    let Some(bytes) = data else {
        return avatar_error(StatusCode::BAD_REQUEST, "No image file provided");
    };

    // Decoding + resizing + encoding is CPU-bound: keep it off the async runtime.
    let webp = match tokio::task::spawn_blocking(move || avatar::process_avatar(&bytes)).await {
        Ok(Ok(w)) => w,
        Ok(Err(e)) => return avatar_error(StatusCode::BAD_REQUEST, &e.to_string()),
        Err(e) => {
            tracing::error!(error = %e, "avatar processing task panicked");
            return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not process image");
        }
    };

    // Millisecond granularity so two uploads within the same second still produce
    // distinct `?v=` values (the served avatar is immutable/1-year-cached).
    let version = chrono::Utc::now().timestamp_millis();
    let now = chrono::Utc::now().to_rfc3339();
    let url = avatar::avatar_url(&user.id, version);
    // Upsert the BLOB and repoint the user row in one transaction so the stored
    // `avatar_url` version can never disagree with the bytes on record.
    let stored = async {
        let mut tx = pool.begin().await?;
        sqlx::query(
            "INSERT INTO user_avatars (user_id, webp, version, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET \
               webp = excluded.webp, version = excluded.version, updated_at = excluded.updated_at",
        )
        .bind(&user.id)
        .bind(&webp)
        .bind(version)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE users SET avatar_url = ? WHERE id = ?")
            .bind(&url)
            .bind(&user.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }
    .await;
    if let Err(e) = stored {
        tracing::error!(error = %e, "avatar save failed");
        return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not save avatar");
    }
    Json(serde_json::json!({ "avatarUrl": url })).into_response()
}

/// `POST /admin/cover/{work_id}` — ADMIN multipart cover upload. Decodes the image,
/// re-encodes it as a budgeted lossy WebP (`cover::process_cover`), stores it in
/// `work_cover_blob`, flips `work.cover_cached_version`, and clears any recorded
/// `work_cover_issue`. The manual-recovery path for covers the crawl can't process.
/// Returns `{ "coverUrl": "/covers/<work_id>.webp?v=<version>" }`.
async fn upload_cover(
    State(app): State<Arc<graphql::AppState>>,
    State(limiter): State<Arc<RateLimiter>>,
    UrlPath(work_id): UrlPath<String>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> axum::response::Response {
    let Some(tok) = bearer(&headers) else {
        return avatar_error(StatusCode::UNAUTHORIZED, "Not authenticated");
    };
    let user = match auth::user_for_token(&app.pool, &tok).await {
        Ok(Some(u)) => u,
        Ok(None) => return avatar_error(StatusCode::UNAUTHORIZED, "Not authenticated"),
        Err(e) => {
            tracing::warn!(error = %e, "cover upload: token lookup failed");
            return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
        }
    };
    if user.is_admin == 0 {
        return avatar_error(StatusCode::FORBIDDEN, "Admin only");
    }
    if limiter.check(&format!("upload:{}", user.id)).is_err() {
        return avatar_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many uploads — please slow down",
        );
    }
    // Guard against an unknown work id (the FK would also reject, but a clear 404 is
    // friendlier and avoids processing an image we can't attach).
    match sqlx::query_scalar::<_, i64>("SELECT 1 FROM work WHERE id = ?")
        .bind(&work_id)
        .fetch_optional(&app.pool)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return avatar_error(StatusCode::NOT_FOUND, "No such work"),
        Err(e) => {
            tracing::warn!(error = %e, "cover upload: work lookup failed");
            return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
        }
    }

    let mut data: Option<Vec<u8>> = None;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let is_file = field.name() == Some("cover") || field.file_name().is_some();
                if is_file {
                    match field.bytes().await {
                        Ok(b) => {
                            data = Some(b.to_vec());
                            break;
                        }
                        Err(_) => {
                            return avatar_error(
                                StatusCode::BAD_REQUEST,
                                "Upload too large or could not be read",
                            )
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(_) => return avatar_error(StatusCode::BAD_REQUEST, "Malformed upload"),
        }
    }
    let Some(bytes) = data else {
        return avatar_error(StatusCode::BAD_REQUEST, "No image file provided");
    };

    let webp = match tokio::task::spawn_blocking(move || cover::process_cover(&bytes)).await {
        Ok(Ok(w)) => w,
        Ok(Err(e)) => return avatar_error(StatusCode::BAD_REQUEST, &e.to_string()),
        Err(e) => {
            tracing::error!(error = %e, "cover processing task panicked");
            return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not process image");
        }
    };

    if let Err(e) = cover::put_work_cover(&app.pool, &app.cover_pool, &work_id, &webp).await {
        tracing::error!(error = %e, work_id = %work_id, "cover upload: store failed");
        return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not save cover");
    }
    cover::clear_cover_issue(&app.pool, &work_id).await;

    // Report the freshly-versioned URL so the admin UI can show the new cover
    // immediately (cache-busted by the stored version).
    let version =
        sqlx::query_scalar::<_, Option<i64>>("SELECT cover_cached_version FROM work WHERE id = ?")
            .bind(&work_id)
            .fetch_optional(&app.pool)
            .await
            .ok()
            .flatten()
            .flatten();
    let url = match version {
        Some(v) => cover::cover_path(&work_id, v),
        None => format!("/covers/{work_id}.webp"),
    };
    Json(serde_json::json!({ "coverUrl": url })).into_response()
}

/// `GET /ext-icons/{file}` — serve an extension's icon from our own origin.
/// `{file}` is `<pkgName>.png`.
///
/// Three tiers, cheapest first (see `ext_icon` for why we host these at all):
///   1. the vendored snapshot baked into the image (`assets/ext-icons/`) — this
///      answers essentially every request, with no DB or network work;
///   2. `extension_icon_blob`, holding icons for extensions Keiyoushi added
///      since that snapshot, plus tombstones for the ~41 that have no icon;
///   3. a one-time fetch from Keiyoushi, cached into (2) for next time.
///
/// The package name is validated before it is joined onto a path or a URL
/// (`ext_icon::is_valid_pkg` rejects `/`, `\` and `..`), so a bad shape 404s
/// rather than escaping the icons directory.
async fn serve_ext_icon(
    State(app): State<Arc<graphql::AppState>>,
    State(CoverDb(pool)): State<CoverDb>,
    UrlPath(file): UrlPath<String>,
) -> axum::response::Response {
    let Some(pkg) = file
        .strip_suffix(".png")
        .filter(|p| ext_icon::is_valid_pkg(p))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Tier 1: the vendored snapshot.
    if let Ok(bytes) = tokio::fs::read(app.ext_icons_dir.join(format!("{pkg}.png"))).await {
        return png_icon_response(bytes);
    }
    // Tier 2: previously fetched, or a live "no icon exists" tombstone.
    match ext_icon::cached(&pool, pkg).await {
        Ok(Some(bytes)) => return png_icon_response(bytes),
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(()) => {}
    }
    // Tier 3: ask Keiyoushi once, then serve from tier 2 forever after.
    match ext_icon::fetch_and_cache(&pool, pkg).await {
        Some(bytes) => png_icon_response(bytes),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Extension icons are content-addressed by package name and effectively never
/// change, so they carry the same immutable year-long cache as avatars/covers —
/// the browser and any edge cache in front of us should ask exactly once.
fn png_icon_response(bytes: Vec<u8>) -> axum::response::Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

/// `GET /avatars/{file}` — serve a stored avatar from `user_avatars`. Immutable +
/// long-cache; the stored `avatar_url` carries a `?v=<ts>` so a new upload busts
/// the cache. `{file}` is `<user_id>.webp`; the id is looked up as a bind param
/// (no path/SQL injection surface), returning 404 for a bad shape or unknown id.
async fn serve_avatar(
    State(pool): State<sqlx::SqlitePool>,
    UrlPath(file): UrlPath<String>,
) -> axum::response::Response {
    let Some(user_id) = file.strip_suffix(".webp") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let webp: Option<Vec<u8>> =
        match sqlx::query_scalar("SELECT webp FROM user_avatars WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&pool)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "avatar read failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
    match webp {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "image/webp"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /covers/{work_id}.webp` — public serve of a work cover from `work_cover_blob`.
/// Same BLOB-in-SQLite + immutable-cache model as avatars. On a cache MISS for a
/// MangaDex-anchored work, lazily fetch its MangaDex cover, downscale to a bounded
/// WebP, store it (so it's on our origin from now on), and serve it — "save covers as
/// we fetch". If the lazy fetch/resize fails (or the work has a version but a missing
/// blob), 302-redirect to the MangaDex CDN so the cover still renders; a genuinely
/// coverless work 404s.
async fn serve_cover(
    State(app): State<Arc<graphql::AppState>>,
    State(CoverDb(pool)): State<CoverDb>,
    UrlPath(file): UrlPath<String>,
) -> axum::response::Response {
    let Some(work_id) = file.strip_suffix(".webp") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Fast path: already-cached blob.
    match sqlx::query_scalar::<_, Vec<u8>>("SELECT webp FROM work_cover_blob WHERE work_id = ?")
        .bind(work_id)
        .fetch_optional(&pool)
        .await
    {
        Ok(Some(bytes)) => return webp_cover_response(bytes),
        Ok(None) => {} // fall through to lazy fetch
        Err(e) => {
            tracing::warn!(error = %e, "cover read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    // Lazy cache: this URL is only built for MangaDex-anchored works, so look up the
    // anchor and materialize the cover on first request.
    let Some((mangadex_id, file_name)) = cover::mangadex_cover_anchor(&app.pool, work_id).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cdn_fallback = crate::mangadex::cover_thumb_url(&mangadex_id, &file_name);
    let Some(bytes) = app
        .mangadex
        .cover_thumb_bytes(&mangadex_id, &file_name)
        .await
    else {
        return axum::response::Redirect::temporary(&cdn_fallback).into_response();
    };
    // Decode + resize + WebP encode is CPU-bound — keep it off the async runtime.
    let resized = tokio::task::spawn_blocking(move || cover::process_cover(&bytes)).await;
    match resized {
        Ok(Ok(webp)) => {
            if let Err(e) = cover::put_work_cover(&app.pool, &pool, work_id, &webp).await {
                tracing::warn!(error = %e, work_id, "lazy cover cache store failed");
            } else {
                // This lazy materialization is an "auto success": if the crawl had
                // recorded a cover issue for this work, clear it so it drops out of the
                // admin Bugs panel and stops being excluded from the drainer's SELECTs.
                cover::clear_cover_issue(&app.pool, work_id).await;
            }
            webp_cover_response(webp)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, work_id, "lazy cover resize failed; redirecting to CDN");
            axum::response::Redirect::temporary(&cdn_fallback).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, work_id, "lazy cover resize task panicked");
            axum::response::Redirect::temporary(&cdn_fallback).into_response()
        }
    }
}

/// Whether `rest` — the path tail after `/api/v1/manga/` — is one of the exactly two
/// Suwayomi image endpoints we proxy: `{id}/thumbnail` (a cover) or
/// `{mangaId}/chapter/{chapterIndex}/page/{pageIndex}` (a page). Every id segment must
/// be numeric. This keeps `serve_suwayomi_image` from becoming a general Suwayomi REST
/// proxy — the GraphQL/library/mutation surface on `:4567` stays unreachable.
fn is_suwayomi_image_path(rest: &str) -> bool {
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match rest.split('/').collect::<Vec<_>>().as_slice() {
        [id, "thumbnail"] => numeric(id),
        [mid, "chapter", ci, "page", pi] => numeric(mid) && numeric(ci) && numeric(pi),
        _ => false,
    }
}

/// `GET /api/v1/manga/{id}/thumbnail` and
/// `GET /api/v1/manga/{mangaId}/chapter/{chapterIndex}/page/{pageIndex}` — public proxy
/// for a Suwayomi-source cover or chapter page.
///
/// Suwayomi (`localhost:4567`) is not publicly exposed, so the browser can't reach its
/// image endpoints. Cover/page URLs are built against THIS origin (set
/// `SUWAYOMI_PUBLIC_URL=https://api.komiq.cc`) and we fetch the bytes internally over the
/// loopback host (`fetch_cover_now` / `fetch_page_now`, each bounded by its own pool) and
/// re-serve them with an immutable cache. Both covers and pages load DIRECTLY from this
/// origin in the reader — the image Worker is for foreign CDNs (MangaDex) only, never our
/// own origin. Only the two numeric image paths above are honoured
/// (`is_suwayomi_image_path`); anything else 404s.
async fn serve_suwayomi_image(
    State(app): State<Arc<graphql::AppState>>,
    State(CoverDb(covers)): State<CoverDb>,
    UrlPath(rest): UrlPath<String>,
) -> axum::response::Response {
    if !is_suwayomi_image_path(&rest) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Covers (`{id}/thumbnail`) are downscaled to a bounded WebP and origin-cached —
    // the feeds serve these full-resolution (avg ~0.8 MB, up to several MB) at card
    // size, so resizing is a ~40x transfer win. Chapter PAGES are served raw: the
    // reader needs full-resolution page images.
    if let Some(manga_id) = thumbnail_manga_id(&rest) {
        return serve_suwayomi_cover(&app, &covers, manga_id).await;
    }
    let path = format!("/api/v1/manga/{rest}");
    // Bounded + timed page fetch (own pool, page timeout, byte cap). The old path called
    // the unbounded `fetch_image`, which shared the 30 s scan timeout and no concurrency
    // bound, so a burst of page requests (a chapter is tens of pages) could pile onto the
    // engine behind the scanner and each pay the full 30 s ceiling.
    match app.suwayomi.fetch_page_now(&path).await {
        Ok((bytes, content_type)) => raw_image_response(content_type, bytes),
        Err(crate::suwayomi::PageFetchError::Busy) => {
            // Saturated past the short wait — shed with a retryable 503 rather than queue.
            // `info`, not `warn`: a steady stream is the operator's signal that page demand
            // is outrunning the engine, not an error per request.
            tracing::info!(path = %path, "suwayomi page: pool saturated, 503");
            page_busy_response()
        }
        Err(crate::suwayomi::PageFetchError::Upstream(e)) => {
            tracing::warn!(error = %e, path = %path, "suwayomi image proxy failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// Retryable, uncacheable 503 for a page whose fetch pool is saturated. Recovery is the
/// reader's per-page "failed — tap to retry" affordance, which fires on any non-image
/// response (browsers do NOT honour `Retry-After` for an `<img>` load, so there is no
/// automatic re-request storm); `Retry-After` stays advisory for non-browser clients, and
/// `no-store` keeps the transient failure out of every cache.
fn page_busy_response() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store, must-revalidate"),
            ),
            (header::RETRY_AFTER, HeaderValue::from_static("2")),
        ],
    )
        .into_response()
}

/// Numeric manga id if `rest` (the tail after `/api/v1/manga/`) is a cover-thumbnail
/// path (`{id}/thumbnail`), else `None` (a chapter page). `is_suwayomi_image_path`
/// has already validated the shape + numeric id, so the parse is infallible here.
fn thumbnail_manga_id(rest: &str) -> Option<i64> {
    match rest.split('/').collect::<Vec<_>>().as_slice() {
        [id, "thumbnail"] => id.parse::<i64>().ok(),
        _ => None,
    }
}

/// Serve a Suwayomi source cover as a bounded WebP: the origin-cached blob when
/// present, else a live fetch + downscale (`cover::process_cover`) that is cached for
/// next time. If the source can't be resized (too large / undecodable) the raw bytes
/// are served so the cover still renders — just unoptimized, and not cached so the
/// resize is retried on a later request.
async fn serve_suwayomi_cover(
    app: &graphql::AppState,
    covers: &sqlx::SqlitePool,
    manga_id: i64,
) -> axum::response::Response {
    if let Some(webp) = cover::get_suwayomi_cover(covers, manga_id).await {
        return webp_cover_response(webp);
    }
    let path = format!("/api/v1/manga/{manga_id}/thumbnail");
    // On a MISS the fetch goes through the BOUNDED cover pool (`fetch_cover_now`), which
    // refuses rather than queues when saturated and uses an 8 s timeout instead of the
    // scanner's 30 s. Before this, a cold cover grid opened ~30 unbounded fetches that
    // queued inside Suwayomi behind the scanner's own traffic and each paid the full 30 s
    // ceiling — measured p50 5.6 s / p90 22.0 s / max 29.0 s, with a 502 past the
    // timeout. The browser renders a 20 s progressive JPEG as a half-loaded image, which
    // is what "half broken covers" actually was.
    let fetched = match app.suwayomi.fetch_cover_now(&path).await {
        Ok(v) => v,
        Err(crate::suwayomi::CoverFetchError::Busy) => {
            // Saturated. Return IMMEDIATELY with a non-cacheable placeholder and
            // materialize out-of-band, instead of adding another 20 s queued request to
            // the pile that caused the saturation. The next load of this page gets the
            // real bytes from the blob cache (and the background drainer's warmer is
            // converging on full coverage regardless).
            spawn_suwayomi_cover_materialize(app, covers, manga_id);
            // `info`, not `debug`: once the warmer has converged this should be near-zero,
            // so a stream of these lines is the operator's signal that cover demand is
            // outrunning the cache (see the deploy note). It cannot be noisy in the steady
            // state because a warm cover never reaches this branch at all.
            tracing::info!(manga_id, "suwayomi cover: pool saturated, 503 warming");
            return cover_warming_response();
        }
        Err(crate::suwayomi::CoverFetchError::Upstream(e)) => {
            tracing::warn!(error = %e, path = %path, "suwayomi cover proxy failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let (bytes, content_type) = fetched;
    // Decode + up to 5x Lanczos3 resize + lossless WebP encode is CPU-bound
    // (~130ms even at opt-level=3): keep it off the async runtime like the
    // avatar/comment handlers do. On failure the closure hands the original
    // bytes back so the cover still renders raw.
    let resized = tokio::task::spawn_blocking(move || {
        cover::process_cover(&bytes).map_err(|e| (e.to_string(), bytes))
    })
    .await;
    match resized {
        Ok(Ok(webp)) => {
            if let Err(e) = cover::put_suwayomi_cover(covers, manga_id, &webp).await {
                tracing::warn!(error = %e, manga_id, "suwayomi cover cache store failed");
            }
            webp_cover_response(webp)
        }
        Ok(Err((err, bytes))) => {
            tracing::warn!(error = %err, manga_id, "suwayomi cover resize failed; serving raw");
            raw_image_response(content_type, bytes)
        }
        Err(e) => {
            tracing::error!(error = %e, manga_id, "suwayomi cover resize task panicked");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Materialize one Suwayomi cover into `suwayomi_cover_blob` OUT OF BAND, after the
/// request that discovered the miss has already returned a placeholder.
///
/// Bounded twice over: it takes a slot from the client's small background pool (and does
/// nothing if that pool is full), and the fetch itself waits on the shared cover-fetch
/// semaphore. So a burst of misses spawns at most `BG_MATERIALIZE_CONCURRENCY` tasks, and
/// those plus the warmer's share are what the on-demand headroom is computed against —
/// see the accounting on `suwayomi::COVER_FETCH_CONCURRENCY`.
///
/// Deliberately un-deduplicated: N concurrent misses for the SAME cover can each spawn a
/// task and fetch the same bytes. With the pool at 4 the waste is bounded and a
/// single-flight map would need its own eviction; the second writer simply overwrites an
/// identical blob.
fn spawn_suwayomi_cover_materialize(
    app: &graphql::AppState,
    covers: &sqlx::SqlitePool,
    manga_id: i64,
) {
    let Some(slot) = app.suwayomi.try_background_slot() else {
        return; // background pool full — the drainer's warmer will get to it
    };
    let suwayomi = app.suwayomi.clone();
    let covers = covers.clone();
    tokio::spawn(async move {
        let _slot = slot; // held for the task's lifetime: this is the fan-out bound
        let path = format!("/api/v1/manga/{manga_id}/thumbnail");
        let Ok((bytes, _ct)) = suwayomi.fetch_cover_background(&path).await else {
            return;
        };
        if let Ok(Ok(webp)) =
            tokio::task::spawn_blocking(move || cover::process_cover(&bytes)).await
        {
            if let Err(e) = cover::put_suwayomi_cover(&covers, manga_id, &webp).await {
                tracing::warn!(error = %e, manga_id, "background suwayomi cover store failed");
            }
        }
    });
}

/// Response for a cover that isn't materialized yet and can't be fetched right now
/// because the bounded cover pool is saturated. Returns in milliseconds instead of
/// queueing for up to 30 s; the bytes arrive out of band via
/// [`spawn_suwayomi_cover_materialize`].
///
/// **A fast 5xx, not a 200 placeholder.** The obvious alternative — a transparent 1x1
/// WebP with `no-store` — is worse on both counts:
///
/// * The reader is built for this exact signal. `Cover.svelte`'s `onerror` renders the
///   themed `.cover-ph k-cover` block, and it treats a *sub-second* failure as
///   authoritative and does NOT retry ("a saturated cover origin answers in
///   milliseconds"), so there is no retry amplification to avoid. A 200 fires `onload`
///   instead, and `MangaCard`'s `.cover` container has no background of its own, so the
///   transparent pixel leaves a see-through hole where every other unloaded cover shows
///   `var(--k-cover)`.
/// * A 503 is visible as a 503 in the edge/origin status metrics. A 200 of a blank pixel
///   is indistinguishable from a served cover.
///
/// `no-store` + `Retry-After` keep any intermediary from pinning this in place of the
/// real bytes (Cloudflare does not cache `no-store`, and does not cache 503 by default
/// either — belt and braces). `X-Cover-Status` is for operators reading a curl.
fn cover_warming_response() -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store, must-revalidate"),
            ),
            (header::RETRY_AFTER, HeaderValue::from_static("2")),
            (
                header::HeaderName::from_static("x-cover-status"),
                HeaderValue::from_static("warming"),
            ),
        ],
    )
        .into_response()
}

/// Immutable-cached response for a resized WebP cover.
fn webp_cover_response(bytes: Vec<u8>) -> axum::response::Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("image/webp")),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Immutable-cached response for a raw proxied image (chapter pages, un-resizable
/// covers) — passes the upstream `Content-Type` through.
fn raw_image_response(content_type: String, bytes: Vec<u8>) -> axum::response::Response {
    let ct = HeaderValue::from_str(&content_type)
        .unwrap_or_else(|_| HeaderValue::from_static("image/jpeg"));
    (
        [
            (header::CONTENT_TYPE, ct),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// `POST /comment-media` — authenticated multipart upload of one image to attach to
/// a comment. The bytes are decoded, downscaled (aspect-preserving) and re-encoded
/// as budgeted lossless WebP (`media::process_comment_image`), then stored as a
/// staged BLOB in `comment_media` (with `comment_id` NULL, owned by the uploader).
/// Returns `{ "mediaId", "url", "width", "height" }`; the client passes `mediaId`
/// to `postComment`, which links the row to the new comment. Unlinked rows are
/// drafts the user never posted.
async fn upload_comment_media(
    State(pool): State<sqlx::SqlitePool>,
    State(limiter): State<Arc<RateLimiter>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> axum::response::Response {
    let Some(tok) = bearer(&headers) else {
        return avatar_error(StatusCode::UNAUTHORIZED, "Not authenticated");
    };
    let user = match auth::user_for_token(&pool, &tok).await {
        Ok(Some(u)) => u,
        Ok(None) => return avatar_error(StatusCode::UNAUTHORIZED, "Not authenticated"),
        Err(e) => {
            tracing::warn!(error = %e, "comment media upload: token lookup failed");
            return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
        }
    };
    if limiter.check(&format!("upload:{}", user.id)).is_err() {
        return avatar_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many uploads — please slow down",
        );
    }
    // Cap the number of staged (unattached) uploads a user can accumulate, so the
    // GC-eligible backlog can't be inflated faster than it's swept. 20 pending
    // drafts is far more than any real compose flow needs.
    let staged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM comment_media WHERE user_id = ? AND comment_id IS NULL",
    )
    .bind(&user.id)
    .fetch_one(&pool)
    .await
    .unwrap_or(0);
    if staged >= 20 {
        return avatar_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many pending image uploads — post or discard some first",
        );
    }

    // Take the first file part (the reader sends a single `image` field).
    let mut data: Option<Vec<u8>> = None;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                let is_file = field.name() == Some("image") || field.file_name().is_some();
                if is_file {
                    match field.bytes().await {
                        Ok(b) => {
                            data = Some(b.to_vec());
                            break;
                        }
                        Err(_) => {
                            return avatar_error(
                                StatusCode::BAD_REQUEST,
                                "Upload too large or could not be read",
                            )
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(_) => return avatar_error(StatusCode::BAD_REQUEST, "Malformed upload"),
        }
    }
    let Some(bytes) = data else {
        return avatar_error(StatusCode::BAD_REQUEST, "No image file provided");
    };

    // Decoding + resizing + encoding is CPU-bound: keep it off the async runtime.
    let processed =
        match tokio::task::spawn_blocking(move || media::process_comment_image(&bytes)).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => return avatar_error(StatusCode::BAD_REQUEST, &e.to_string()),
            Err(e) => {
                tracing::error!(error = %e, "comment media processing task panicked");
                return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not process image");
            }
        };

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let stored = sqlx::query(
        "INSERT INTO comment_media (id, comment_id, user_id, webp, width, height, created_at) \
         VALUES (?, NULL, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&user.id)
    .bind(&processed.webp)
    .bind(processed.width as i64)
    .bind(processed.height as i64)
    .bind(&now)
    .execute(&pool)
    .await;
    if let Err(e) = stored {
        tracing::error!(error = %e, "comment media save failed");
        return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not save image");
    }
    Json(serde_json::json!({
        "mediaId": id,
        "url": media::comment_media_url(&id),
        "width": processed.width,
        "height": processed.height,
    }))
    .into_response()
}

/// `GET /comment-media/{file}` — serve a stored comment image from `comment_media`.
/// Immutable + long-cache: the id is a random uuid, so the bytes never change.
/// `{file}` is `<id>.webp`; the id is looked up as a bind param (no injection
/// surface), returning 404 for a bad shape or unknown id.
async fn serve_comment_media(
    State(pool): State<sqlx::SqlitePool>,
    UrlPath(file): UrlPath<String>,
) -> axum::response::Response {
    let Some(id) = file.strip_suffix(".webp") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let webp: Option<Vec<u8>> =
        match sqlx::query_scalar("SELECT webp FROM comment_media WHERE id = ?")
            .bind(id)
            .fetch_optional(&pool)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "comment media read failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
    match webp {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "image/webp"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Liveness: cheap, dependency-free — used by the container HEALTHCHECK. A blip
/// in the DB must not flap this (that would trigger restart loops).
async fn health() -> &'static str {
    "ok"
}

/// Prometheus-format operational gauges, scraped on demand.
///
/// A first cut: these are all DERIVED FROM DB STATE at scrape time, so they need no
/// request-path instrumentation. That deliberately omits request-latency histograms
/// and error-rate counters, which would require threading a metric registry through
/// the resolvers — a separate change. What's here is what the plan's verification
/// steps actually need to read without grepping `docker logs`: scan-scheduler health
/// (the herd/backoff signals), the subscription circuit breaker, and updates-feed
/// freshness. NOTE: this shares the public 0.0.0.0:{port} router, so the cloudflared
/// ingress MUST exclude `/metrics` (deploy step) to keep it off api.komiq.cc — it is
/// not loopback-bound. Even leaked it discloses only aggregate counts, not user data.
async fn metrics(State(pool): State<sqlx::SqlitePool>) -> impl IntoResponse {
    // One helper so a single failed query degrades to a `-1` gauge rather than 500ing
    // the whole scrape (a metrics endpoint that fails is worse than one with a hole).
    async fn scalar(pool: &sqlx::SqlitePool, sql: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .unwrap_or(-1)
    }

    let now = chrono::Utc::now().to_rfc3339();
    let scan_total = scalar(&pool, "SELECT COUNT(*) FROM series_scan_state").await;
    let scan_due = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM series_scan_state WHERE next_scan_at <= ?",
    )
    .bind(&now)
    .fetch_one(&pool)
    .await
    .unwrap_or(-1);
    let scan_failing = scalar(
        &pool,
        "SELECT COUNT(*) FROM series_scan_state WHERE consecutive_failures > 0",
    )
    .await;
    let scan_awaiting = scalar(
        &pool,
        "SELECT COUNT(*) FROM series_scan_state WHERE awaiting_since IS NOT NULL",
    )
    .await;
    // Herd detector: how many rows are bunched into the single busiest upcoming minute.
    // A healthy, jittered schedule keeps this small; the pre-fix herd put ~740 here.
    let scan_max_minute_cluster = scalar(
        &pool,
        "SELECT COALESCE(MAX(c), 0) FROM (SELECT COUNT(*) c FROM series_scan_state \
         WHERE next_scan_at IS NOT NULL GROUP BY substr(next_scan_at, 1, 16))",
    )
    .await;
    let subs_total = scalar(&pool, "SELECT COUNT(*) FROM extension_subscription").await;
    let subs_disabled = scalar(
        &pool,
        "SELECT COUNT(*) FROM extension_subscription WHERE disabled_at IS NOT NULL",
    )
    .await;
    let feed_rows = scalar(&pool, "SELECT COUNT(*) FROM feed_updates").await;
    let feed_age_secs = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT CAST((julianday('now') - julianday(MAX(latest_at))) * 86400 AS INTEGER) \
         FROM feed_updates",
    )
    .fetch_one(&pool)
    .await
    .ok()
    .flatten()
    .unwrap_or(-1);

    let body = format!(
        "# HELP komika_scan_state_total Series tracked by the scan scheduler\n\
         # TYPE komika_scan_state_total gauge\n\
         komika_scan_state_total {scan_total}\n\
         # HELP komika_scan_due Series currently overdue for a scan\n\
         # TYPE komika_scan_due gauge\n\
         komika_scan_due {scan_due}\n\
         # HELP komika_scan_failing Series with a non-zero consecutive-failure count\n\
         # TYPE komika_scan_failing gauge\n\
         komika_scan_failing {scan_failing}\n\
         # HELP komika_scan_awaiting Series in the accelerated awaiting-chapter poll\n\
         # TYPE komika_scan_awaiting gauge\n\
         komika_scan_awaiting {scan_awaiting}\n\
         # HELP komika_scan_max_minute_cluster Largest number of series scheduled in one minute (herd detector)\n\
         # TYPE komika_scan_max_minute_cluster gauge\n\
         komika_scan_max_minute_cluster {scan_max_minute_cluster}\n\
         # HELP komika_subscriptions_total Extension sync subscriptions\n\
         # TYPE komika_subscriptions_total gauge\n\
         komika_subscriptions_total {subs_total}\n\
         # HELP komika_subscriptions_disabled Subscriptions auto-disabled by the circuit breaker\n\
         # TYPE komika_subscriptions_disabled gauge\n\
         komika_subscriptions_disabled {subs_disabled}\n\
         # HELP komika_feed_updates_rows Materialized updates-feed row count\n\
         # TYPE komika_feed_updates_rows gauge\n\
         komika_feed_updates_rows {feed_rows}\n\
         # HELP komika_feed_updates_newest_age_seconds Age of the newest chapter in the feed\n\
         # TYPE komika_feed_updates_newest_age_seconds gauge\n\
         komika_feed_updates_newest_age_seconds {feed_age_secs}\n"
    );
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

/// Readiness: verifies the process can actually serve — i.e. the DB answers.
/// For a load balancer / orchestrator readiness gate, not the liveness probe.
async fn ready(State(pool): State<sqlx::SqlitePool>) -> impl IntoResponse {
    match sqlx::query("SELECT 1").execute(&pool).await {
        Ok(_) => (axum::http::StatusCode::OK, "ready"),
        Err(e) => {
            tracing::warn!(error = %e, "readiness check failed");
            (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not ready")
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "komika_server=info,tower_http=info".into());
    // LOG_FORMAT=json emits structured JSON logs for prod log aggregation;
    // anything else keeps the human-readable formatter for local dev.
    let json_logs = std::env::var("LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    if json_logs {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    let cfg = Config::from_env();

    // `komika-server phase-f <stage> [--apply] …` — the Phase F (`all.mangadex`
    // retirement) tool, handled BEFORE any server state is built so it can never
    // start a listener, a scanner or a sync loop alongside itself. Every stage is a
    // dry run unless `--apply` is passed, and the destructive one additionally needs
    // `--confirm <token>`; see `phase_f`. It exits when the stage is done.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("phase-f") {
        let pool = db::init(&cfg.database_url).await?;
        let cover_pool = db::init_covers(&cfg.covers_database_url).await?;
        let result =
            phase_f::run_cli(&pool, Some(&cover_pool), &cfg.suwayomi_url, &argv[1..]).await;
        // Close both pools explicitly: a `merge` stage holds a WAL that a still-open
        // pool would keep pinned, and this process runs alongside the live server.
        pool.close().await;
        cover_pool.close().await;
        return result;
    }

    tracing::info!(?cfg, "starting komika-server");

    let pool = db::init(&cfg.database_url).await?;
    // Separate, un-replicated DB for cover blobs — see `db::init_covers`. Kept out
    // of the main DB so Litestream ships only accounts/social/catalogue to R2, never
    // the large (re-derivable) cover thumbnails.
    let cover_pool = db::init_covers(&cfg.covers_database_url).await?;

    // DR reconciliation: if the covers DB is empty (e.g. a fresh host that restored
    // the main DB from R2 but has no covers.sqlite3), clear any surviving
    // `cover_cached_version` pointers so cover URLs fall back to the Worker proxy
    // until the drainer re-materializes them — otherwise those pointers would 404.
    // Cheap no-op in steady state (covers DB non-empty → skip the UPDATE).
    let cover_blob_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_cover_blob")
        .fetch_one(&cover_pool)
        .await?;
    if cover_blob_count == 0 {
        let cleared = sqlx::query(
            "UPDATE work SET cover_cached_version = NULL WHERE cover_cached_version IS NOT NULL",
        )
        .execute(&pool)
        .await?;
        if cleared.rows_affected() > 0 {
            tracing::warn!(
                cleared = cleared.rows_affected(),
                "covers DB empty — cleared stale cover version pointers; covers will re-cache"
            );
        }
    }

    // Provision/promote configured admin accounts (idempotent). This is the only
    // path to admin: `register` reserves these names and never grants admin.
    graphql::provision_admins(
        &pool,
        &cfg.admin_users,
        cfg.admin_password.0.as_deref(),
        cfg.admin_email.as_deref(),
    )
    .await?;

    let suwayomi = SuwayomiClient::new(
        cfg.suwayomi_url.clone(),
        cfg.suwayomi_public_url.clone(),
        cfg.source_id.clone(),
    );
    let mangadex = Arc::new(mangadex::MangaDexClient::new(
        &cfg.mangadex_user_agent,
        cfg.mangadex_rate_per_sec,
        cfg.mangadex_athome_per_min,
    ));
    let state = Arc::new(AppState {
        pool: pool.clone(),
        cover_pool: cover_pool.clone(),
        suwayomi,
        mangadex: mangadex.clone(),
        admin_users: cfg.admin_users.clone(),
        scan_health: std::sync::Mutex::new(ScanHealth::default()),
        auth_limiter: RateLimiter::new(cfg.auth_rate_limit_max, cfg.auth_rate_limit_window_secs),
        federated_limiter: RateLimiter::new(
            cfg.federated_rate_limit_max,
            cfg.federated_rate_limit_window_secs,
        ),
        session_ttl_secs: cfg.session_ttl_secs,
        series_inflight: KeyedLocks::default(),
        chapters_inflight: KeyedLocks::default(),
        cover_crawl_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        catalogue_cover_phash: cfg.catalogue_cover_phash,
        ext_icons_dir: std::path::PathBuf::from(&cfg.ext_icons_dir),
    });

    // Startup recovery: an ingest job still `running` was interrupted by the
    // previous shutdown — its task is gone, so mark it failed (otherwise the
    // partial unique index blocks new jobs for that source forever).
    match ingest::mark_interrupted_jobs(&pool).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(n, "marked interrupted source-ingest jobs as failed"),
        Err(e) => tracing::warn!(error = %e, "failed to sweep interrupted ingest jobs"),
    }

    // Populate the materialized updates feed at boot (migration 0051), off the request
    // path, so `canonicalUpdates` serves real rows immediately — even before the first
    // catalogue-sync cycle, and even when CATALOGUE_SYNC is off. Spawned so a slow
    // rebuild doesn't delay the listener coming up: measured on production data it is a
    // ~13 s transaction for `feed_series_updates` (48.5k rows) followed by a ~6-7 s one for
    // `browse_catalogue` (115.5k rows, migration 0069), which `refresh_feed_updates` chains.
    // Delayed ~20s so this heavy writer doesn't land in the same instant as the
    // scanner's immediate first tick — the scanner would absorb the contention via its
    // lock-retry, but there's no reason to create it (mirrors the 2C boot stagger).
    //
    // Browse serves correct rows for that whole window regardless: migration 0069 backfills
    // `browse_catalogue` itself, so the first request does not depend on this task.
    {
        let pool = pool.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            match crate::catalog::refresh_feed_updates(&pool).await {
                Ok(n) => tracing::info!(works = n, "feed_updates: initial build complete"),
                Err(e) => tracing::warn!(error = %e, "feed_updates: initial build failed"),
            }
            // AD-5's `refresh_work_fts` used to be a second call here. It is now the last
            // link of the `refresh_feed_updates` chain (with `feed_series_updates` and
            // `browse_catalogue`), so this slot still builds the search index — search and
            // Browse just can no longer be wired to different cadences by accident.
        });
    }

    // Background adaptive scan scheduler, sharing the same AppState.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    scanner::spawn(state.clone(), cfg.scan_tick_seconds, shutdown_rx);

    // Extension-level source-sync scheduler: re-walks *subscribed* extensions (LATEST)
    // to auto-discover newly-added series, and (independently of subscriptions) reconciles
    // library membership + backfills scan-state rows once per interval. Discovery is a
    // no-op with nothing subscribed, but the reconcile still runs — throttled to once per
    // interval so restarts don't re-trigger it (see `sync::run_loop`).
    sync::spawn(
        state.clone(),
        cfg.source_sync_interval_seconds,
        cfg.source_sync_max_pages,
        shutdown_tx.subscribe(),
    );

    // Phase E5: LATEST-diff discovery. A tight (~15 min) poll of page 1 of each subscribed
    // source's LATEST, diffed against a per-source snapshot; series that moved up / entered
    // are enqueued for a targeted scan via the same `trigger_due_now` sink E2 uses. This is
    // the fast push signal that makes ~98.8%-wasted timer polling unnecessary (§8e). The
    // per-series baseline poll still runs underneath it until Phase E slows it to a safety
    // net.
    discovery::spawn(
        state.clone(),
        cfg.discovery_interval_secs,
        shutdown_tx.subscribe(),
    );

    // Hourly garbage collection: orphaned staged comment-media uploads (rows the
    // uploader never attached to a comment) plus cover blobs in the separate covers DB
    // whose owning work/series no longer exists — nothing in the main DB can cascade
    // across the file boundary, so 8,868 work blobs (1,464 MiB) had accumulated with no
    // reclaimer. See `gc::sweep_cover_blobs` for the race-safety argument.
    gc::spawn(pool.clone(), cover_pool.clone(), shutdown_tx.subscribe());

    // Keep `sqlite_stat1` current so the planner keeps choosing migration 0058's
    // indexes. Background, not boot-blocking. See `db::spawn_analyze` for why this is
    // an exact per-table ANALYZE rather than `PRAGMA optimize`.
    db::spawn_analyze(pool.clone(), shutdown_tx.subscribe());

    // Chapter-spine drains (Phase B3). Migration 0090 adds `chapter.chapter_key` and
    // `chapter.scanlator` and moves nothing; this is what backfills the 877,824 existing
    // rows' keys and materialises the 563,095 Suwayomi chapters that have never had a
    // canonical `chapter` row at all. Unconditional — the spine is the substrate the
    // release ledger and the incremental feed are built on, not an opt-in feature — but
    // paced, staggered behind boot, and self-terminating once drained.
    catalog::spine::spawn(pool.clone(), shutdown_tx.subscribe());

    // Direct-MangaDex catalogue sync — opt-in (CATALOGUE.md §5). Off unless
    // CATALOGUE_SYNC is set, so the default deployment never hits MangaDex. Recurring:
    // seeds on startup, then incrementally refreshes (updatedAtSince) every interval.
    //
    // FLEET CONSTRAINT (M4, CATALOGUE.md §9): the MangaDex rate limiter is
    // in-process, so it only bounds THIS process. Enable CATALOGUE_SYNC on exactly
    // one replica — N replicas syncing = N× the shared-IP budget → 429/ban. Moving
    // to a shared (DB/Redis) limiter is the prerequisite for multi-replica sync.
    if cfg.catalogue_sync_enabled {
        mangadex::spawn_recurring(
            pool.clone(),
            mangadex.clone(),
            cfg.catalogue_cover_phash,
            cfg.catalogue_sync_interval_secs,
            cfg.chapter_sync_interval_secs,
            cfg.feed_reconcile_interval_secs,
            shutdown_tx.subscribe(),
        );
        // One-time top-up: fill catalogue series the original seed missed (the
        // forward-only incremental never revisits them). Runs once (marker in
        // maintenance_flag), only after the seed is complete, and holds the same
        // single-flight lock as the recurring sync so it can't race it.
        // The covers pool goes along so the dedup this backfill runs on completion can
        // reclaim the merged-away works' cover blobs; without it that one-time fold
        // leaked a blob per merge into the un-cascaded covers DB.
        // The shutdown watch goes along because the pass schedule spans up to ~20h; a
        // redeploy must be able to interrupt it rather than leave it sweeping MangaDex
        // (and taking the catalogue single-flight lock) through the drain.
        mangadex::spawn_backfill_if_needed(
            pool.clone(),
            cover_pool.clone(),
            mangadex.clone(),
            cfg.catalogue_cover_phash,
            shutdown_tx.subscribe(),
        );
        // A1b — the backfill migration 0073 promised and nobody had written. Fills
        // `chapter.readable_at` / `chapter.external_url` on the 877,868 mirrored MangaDex
        // rows that have neither, by re-asking `/chapter?ids[]=` 100 at a time. That is what
        // returns the 46 works whose every chapter carries MangaDex's 2037 `publishAt`
        // sentinel to /updates, and what makes the ~35,000 off-site chapters identifiable
        // (`external_url IS NOT NULL`) instead of silently unopenable.
        //
        // Inside the CATALOGUE_SYNC gate deliberately: it hits MangaDex under the same
        // in-process rate limiter, so the one-replica rule above applies to it verbatim.
        // Resumable with no cursor and no completion marker — its work-list is
        // `readable_at IS NULL`, so an interrupted drain resumes itself and a finished one
        // costs an hourly scan of an empty partial index.
        mangadex::spawn_readable_backfill(pool.clone(), mangadex.clone(), shutdown_tx.subscribe());
    } else {
        tracing::info!("catalogue sync disabled (set CATALOGUE_SYNC=on to enable)");
    }

    // Recurring auto-enrichment drainer (X1) — opt-in. Keeps newly-ingested
    // MangaDex-anchored works self-enriching (S2 metadata + F2 covers) in small
    // polite batches; shares the MangaDex rate limiter.
    if cfg.metadata_backfill_enabled {
        graphql::spawn_metadata_backfill(
            state.clone(),
            cfg.metadata_backfill_interval_secs,
            cfg.metadata_backfill_batch,
            shutdown_tx.subscribe(),
        );
    } else {
        tracing::info!("metadata auto-enrichment disabled (set METADATA_BACKFILL=on to enable)");
    }

    // Automatic cover-cache drainer — populates the DB cover cache (`/covers/…`,
    // off the CF image Worker) with NO manual trigger. Defaults ON; hits MangaDex
    // under the shared limiter, so keep it to one replica (COVER_CACHE=off elsewhere).
    if cfg.cover_cache_enabled {
        cover::spawn(
            pool.clone(),
            cover_pool.clone(),
            mangadex.clone(),
            state.suwayomi.clone(),
            state.cover_crawl_running.clone(),
            cfg.cover_cache_interval_secs,
            cfg.cover_cache_batch,
            shutdown_tx.subscribe(),
        );
    } else {
        tracing::info!("cover cache drainer disabled (COVER_CACHE=off)");
    }

    let schema = build_schema(state.clone(), !cfg.graphiql_enabled);

    let origins: Vec<_> = cfg
        .cors_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

    // GraphiQL (GET /graphql) is served only when explicitly enabled; otherwise a
    // GET is a 405 and only the POST endpoint exists. Introspection is disabled in
    // the same off state (see build_schema above).
    let graphql_route = if cfg.graphiql_enabled {
        get(graphiql).post(graphql_handler)
    } else {
        post(graphql_handler)
    };

    let app = Router::new()
        .route("/graphql", graphql_route)
        .route("/health", get(health))
        .route("/health/ready", get(ready))
        // Operational gauges for scraping (aggregate counts only — no series content,
        // no user data). Intended to be scraped on the VPS via localhost:8080/metrics;
        // the cloudflared ingress must exclude `/metrics` so it is not reachable at
        // api.komiq.cc/metrics (deploy step). Even if leaked it discloses only row
        // counts and ages, but it is not meant to be public.
        .route("/metrics", get(metrics))
        // Authenticated avatar upload + public serve (VM data volume). The upload
        // route raises the body limit above the raw-image cap enforced in
        // `avatar::process_avatar` (axum's default is 2 MB).
        .route(
            "/avatar",
            post(upload_avatar).layer(DefaultBodyLimit::max(
                avatar::MAX_UPLOAD_BYTES + 1024 * 1024,
            )),
        )
        .route("/avatars/{file}", get(serve_avatar))
        // Public serve of DB-backed work covers (WebP BLOB in `work_cover_blob`).
        // Lets the web reader load covers from our own origin instead of the CF
        // image Worker; the bytes are materialized by `cover::crawl_uncached_covers`
        // (admin `materializeCatalogueCovers`).
        .route("/covers/{file}", get(serve_cover))
        // Public serve of extension icons from the vendored snapshot baked into
        // this image, with a lazy Keiyoushi backfill for extensions added since.
        // Hosted here because Keiyoushi's own icon directory was emptied in their
        // `index.pb` migration (see the `ext_icon` module).
        .route("/ext-icons/{file}", get(serve_ext_icon))
        // Admin: replace ONE work's cover from an uploaded image (the "Bugs" panel's
        // manual-upload action for covers the crawl can't process). Same multipart
        // model as avatars; body limit raised above the raw-image cap.
        .route(
            "/admin/cover/{work_id}",
            post(upload_cover).layer(DefaultBodyLimit::max(cover::MAX_SOURCE_BYTES + 1024 * 1024)),
        )
        // Public proxy for Suwayomi-source cover thumbnails + chapter pages. Suwayomi
        // is loopback-only, so its image endpoints are unreachable from the browser;
        // these URLs point HERE (SUWAYOMI_PUBLIC_URL=https://api.komiq.cc) and we fetch
        // the bytes internally. Restricted to the two numeric image paths
        // (`is_suwayomi_image_path`) so it is not a general Suwayomi REST proxy.
        .route("/api/v1/manga/{*rest}", get(serve_suwayomi_image))
        // Authenticated comment-image upload + public serve, same BLOB-in-SQLite
        // model as avatars. The upload route raises the body limit above the raw
        // image cap enforced in `media::process_comment_image`.
        .route(
            "/comment-media",
            post(upload_comment_media)
                .layer(DefaultBodyLimit::max(media::MAX_UPLOAD_BYTES + 1024 * 1024)),
        )
        .route("/comment-media/{file}", get(serve_comment_media))
        // Request-id + access-log span. SetRequestId runs first (generates an
        // x-request-id when the client didn't send one), TraceLayer's span picks
        // it up, and PropagateRequestId echoes it back on the response.
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(|request: &axum::http::Request<axum::body::Body>| {
                            let request_id = request
                                .headers()
                                .get("x-request-id")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("-");
                            tracing::info_span!(
                                "request",
                                method = %request.method(),
                                uri = %request.uri(),
                                request_id = %request_id,
                            )
                        })
                        .on_response(DefaultOnResponse::new().level(Level::INFO)),
                )
                .layer(PropagateRequestIdLayer::x_request_id()),
        )
        .layer(cors)
        .layer(SetResponseHeaderLayer::overriding(
            header::HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static(
                "geolocation=(), camera=(), microphone=(), payment=(), usb=()",
            ),
        ))
        // Outermost: a panicking resolver/handler becomes a 500 (logged) instead
        // of dropping the connection or killing the worker task.
        .layer(CatchPanicLayer::custom(handle_panic))
        .with_state(RouterState {
            schema,
            pool,
            cover_pool,
            trusted_proxies: Arc::new(parse_trusted_proxies(&cfg.trusted_proxy_cidrs)),
            // ~20 uploads/min per user across the avatar + comment-media routes.
            upload_limiter: Arc::new(RateLimiter::new(20, 60)),
            app_state: state,
        });

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{addr}/graphql");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown_tx))
    .await?;
    Ok(())
}

/// Turn a caught panic into a JSON 500 (GraphQL-shaped) and log it with the
/// panic payload, so a single bad request can't take the server down.
fn handle_panic(err: Box<dyn std::any::Any + Send + 'static>) -> axum::response::Response {
    let details = if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        (*s).to_string()
    } else {
        "unknown panic".to_string()
    };
    tracing::error!(panic = %details, "caught panic; returning 500");
    let body = r#"{"data":null,"errors":[{"message":"Internal Server Error"}]}"#;
    axum::http::Response::builder()
        .status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .expect("static panic response is valid")
}

async fn shutdown_signal(shutdown_tx: tokio::sync::watch::Sender<bool>) {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("shutting down");
    let _ = shutdown_tx.send(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn peer(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 12345)
    }

    /// The saturated-pool answer must be a FAST 5xx that no cache will keep. A 200
    /// (the transparent-pixel placeholder this replaced) fires `Cover.svelte`'s
    /// `onload`, leaving a see-through hole instead of the themed `.cover-ph` block,
    /// and is invisible in status metrics.
    #[test]
    fn cover_warming_response_is_an_uncacheable_503() {
        let res = cover_warming_response();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        let cc = res.headers().get(header::CACHE_CONTROL).unwrap();
        assert!(
            cc.to_str().unwrap().contains("no-store"),
            "a warming answer must never be cached in place of the real cover"
        );
        assert_eq!(res.headers().get(header::RETRY_AFTER).unwrap(), "2");
        assert_eq!(res.headers().get("x-cover-status").unwrap(), "warming");
    }

    #[test]
    fn suwayomi_image_path_allows_only_cover_and_page() {
        // The two proxied image endpoints (tail after `/api/v1/manga/`).
        assert!(is_suwayomi_image_path("123/thumbnail"));
        assert!(is_suwayomi_image_path("123/chapter/4/page/0"));
        assert!(is_suwayomi_image_path("1/chapter/0/page/25"));
        // Everything else must be rejected so this is not a general Suwayomi REST
        // proxy — the GraphQL/library/mutation surface on :4567 stays unreachable.
        assert!(!is_suwayomi_image_path("123")); // bare manga
        assert!(!is_suwayomi_image_path("123/chapters")); // chapter list
        assert!(!is_suwayomi_image_path("abc/thumbnail")); // non-numeric id
        assert!(!is_suwayomi_image_path("123/thumbnail/../../graphql")); // traversal
        assert!(!is_suwayomi_image_path("123/chapter/4/page")); // missing index
        assert!(!is_suwayomi_image_path("123/chapter/x/page/0")); // non-numeric chapter
        assert!(!is_suwayomi_image_path("")); // empty
        assert!(!is_suwayomi_image_path("123/thumbnail/")); // trailing slash → empty seg
    }

    #[test]
    fn cidr_ipv4_contains() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains("10.1.2.3".parse().unwrap()));
        assert!(c.contains("10.255.255.255".parse().unwrap()));
        assert!(!c.contains("11.0.0.1".parse().unwrap()));
        assert!(!c.contains("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn cidr_bare_ip_is_host_route() {
        let c = Cidr::parse("127.0.0.1").unwrap();
        assert!(c.contains("127.0.0.1".parse().unwrap()));
        assert!(!c.contains("127.0.0.2".parse().unwrap()));
    }

    #[test]
    fn cidr_ipv6_and_cross_family() {
        let c = Cidr::parse("2001:db8::/32").unwrap();
        assert!(c.contains("2001:db8::1".parse().unwrap()));
        assert!(!c.contains("2001:db9::1".parse().unwrap()));
        // a v4 block never matches a v6 address
        let v4 = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(!v4.contains("::1".parse().unwrap()));
    }

    #[test]
    fn cidr_rejects_malformed() {
        assert!(Cidr::parse("not-an-ip").is_none());
        assert!(Cidr::parse("10.0.0.0/33").is_none());
        assert!(Cidr::parse("::1/129").is_none());
    }

    #[test]
    fn untrusted_peer_ignores_forwarded_headers() {
        // No trusted proxies configured (the default): the socket peer wins and a
        // spoofed X-Forwarded-For cannot move the rate-limit key.
        let h = hdrs(&[("x-forwarded-for", "1.2.3.4"), ("x-real-ip", "5.6.7.8")]);
        let ip = resolve_client_ip(&h, peer("203.0.113.9"), &[]);
        assert_eq!(ip, "203.0.113.9");
    }

    #[test]
    fn trusted_peer_honors_xff_rightmost() {
        // The rightmost hop is the one our own edge appended; the leftmost is
        // whatever the client sent and must never be trusted.
        let trusted = vec![Cidr::parse("10.0.0.0/8").unwrap()];
        let h = hdrs(&[("x-forwarded-for", "1.2.3.4, 203.0.113.7")]);
        let ip = resolve_client_ip(&h, peer("10.0.0.5"), &trusted);
        assert_eq!(ip, "203.0.113.7");
    }

    #[test]
    fn cf_connecting_ip_wins_over_forged_xff() {
        // Cloudflare overwrites CF-Connecting-IP with the true client address, so
        // it must beat a client-supplied X-Forwarded-For prefix.
        let trusted = vec![Cidr::parse("10.0.0.0/8").unwrap()];
        let h = hdrs(&[
            ("x-forwarded-for", "1.2.3.4"),
            ("cf-connecting-ip", "203.0.113.7"),
        ]);
        let ip = resolve_client_ip(&h, peer("10.0.0.5"), &trusted);
        assert_eq!(ip, "203.0.113.7");
    }

    #[test]
    fn spoofed_xff_cannot_pick_its_own_bucket() {
        // Regression guard: a caller sending `X-Forwarded-For: <victim>` arrives at
        // the origin as `<victim>, <real>` once the edge appends. The limiter must
        // key on the appended hop, not the forged one.
        let trusted = vec![Cidr::parse("10.0.0.0/8").unwrap()];
        let attacker = hdrs(&[("x-forwarded-for", "9.9.9.9, 198.51.100.4")]);
        let victim = hdrs(&[("x-forwarded-for", "9.9.9.9, 198.51.100.5")]);
        let a = resolve_client_ip(&attacker, peer("10.0.0.5"), &trusted);
        let v = resolve_client_ip(&victim, peer("10.0.0.5"), &trusted);
        assert_ne!(a, v, "forged leftmost hop must not collapse two clients");
        assert_eq!(a, "198.51.100.4");
    }

    #[test]
    fn trusted_peer_falls_back_to_x_real_ip() {
        let trusted = vec![Cidr::parse("10.0.0.0/8").unwrap()];
        let h = hdrs(&[("x-real-ip", "9.9.9.9")]);
        let ip = resolve_client_ip(&h, peer("10.0.0.5"), &trusted);
        assert_eq!(ip, "9.9.9.9");
    }

    #[test]
    fn trusted_peer_without_headers_uses_peer() {
        let trusted = vec![Cidr::parse("10.0.0.0/8").unwrap()];
        let ip = resolve_client_ip(&HeaderMap::new(), peer("10.0.0.5"), &trusted);
        assert_eq!(ip, "10.0.0.5");
    }

    #[test]
    fn v4_mapped_v6_peer_matches_v4_cidr() {
        let trusted = vec![Cidr::parse("127.0.0.0/8").unwrap()];
        let h = hdrs(&[("x-forwarded-for", "1.2.3.4")]);
        // A dual-stack socket may present a v4 peer as `::ffff:127.0.0.1`.
        let ip = resolve_client_ip(&h, peer("::ffff:127.0.0.1"), &trusted);
        assert_eq!(ip, "1.2.3.4");
    }
}
