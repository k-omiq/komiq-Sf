mod auth;
mod avatar;
mod block;
mod browse;
mod catalog;
mod chapter_label;
mod cloudflare;
mod config;
mod cover;
mod cover_recompress;
mod database;
mod db;
mod dedup;
mod discovery;
mod ext_icon;
mod gc;
mod graphql;
mod img_proxy;
mod ingest;
mod instance;
mod lan;
mod maintenance;
mod mangadex;
mod media;
mod notify;
mod phase_f;
mod phash;
mod rate_limit;
mod request_budget;
mod scanlators;
mod scanner;
mod series_cache;
mod setup_cli;
mod source_health;
mod suwayomi;
mod sync;
mod tags;
mod task;
mod views;
mod web_dir;
mod work_stats;

use std::sync::Arc;

use std::net::{IpAddr, SocketAddr};

use async_graphql::http::GraphiQLSource;
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::{
    extract::{
        ConnectInfo, DefaultBodyLimit, FromRef, Multipart, Path as UrlPath, RawQuery, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
// Response compression (gzip). The home feed is ~64 KB uncompressed / ~22 KB
// gzipped, and the cloudflared tunnel bills every byte — see PERFORMANCE_ROADMAP.md.
// The default predicate skips image/* (covers, avatars) and already-encoded bodies,
// so the binary routes are untouched.
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

use config::Config;
use graphql::{
    ApiSchema, AppState, ClientIp, KeyedLocks, RateLimiter, RequestAuth, RequestBlocks,
    RequestLibraryCache, RequestNsfwPref, RequestUserCache, ScanHealth,
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

/// The trusted-proxy list, readable per request and replaceable at runtime: Cloudflare
/// setup sets it (loopback for a tunnel, Cloudflare's ranges for a public IP) and
/// "Remove" clears it. When `TRUSTED_PROXY_CIDRS` is set it is **pinned**: env wins, as
/// for every instance setting, so the hosted deployment keeps exactly its list.
pub(crate) struct TrustedProxies {
    pinned: bool,
    list: std::sync::RwLock<(Vec<String>, Arc<Vec<Cidr>>)>,
}

impl TrustedProxies {
    fn new(raw: Vec<String>, pinned: bool) -> Self {
        let parsed = Arc::new(parse_trusted_proxies(&raw));
        Self {
            pinned,
            list: std::sync::RwLock::new((raw, parsed)),
        }
    }

    /// Env first, then the stored setting (`instance::TRUSTED_PROXY_CIDRS`).
    async fn load(pool: &crate::db::Database) -> Self {
        let r = instance::resolve(pool, &instance::TRUSTED_PROXY_CIDRS).await;
        let raw = r
            .value
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Self::new(raw, r.source == instance::Source::Env)
    }

    fn current(&self) -> Arc<Vec<Cidr>> {
        self.list
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .1
            .clone()
    }

    pub(crate) fn raw(&self) -> Vec<String> {
        self.list
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .clone()
    }

    pub(crate) fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Replace the list; a no-op while env pins it.
    pub(crate) fn replace(&self, raw: &[String]) {
        if self.pinned {
            return;
        }
        let parsed = Arc::new(parse_trusted_proxies(raw));
        *self.list.write().unwrap_or_else(|p| p.into_inner()) = (raw.to_vec(), parsed);
    }

    #[cfg(test)]
    pub(crate) fn for_tests(raw: Vec<String>, pinned: bool) -> Self {
        Self::new(raw, pinned)
    }
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
            if let Ok(ip) = cf.trim().parse::<IpAddr>() {
                return ip.to_canonical().to_string();
            }
        }
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(last) = xff.split(',').next_back() {
                if let Ok(ip) = last.trim().parse::<IpAddr>() {
                    return ip.to_canonical().to_string();
                }
            }
        }
        if let Some(rip) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
            if let Ok(ip) = rip.trim().parse::<IpAddr>() {
                return ip.to_canonical().to_string();
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
    pool: crate::db::Database,
    ready_pool: ReadyDb,
    request_budgets: Arc<request_budget::RequestBudgets>,
    /// Un-replicated cover-cache DB (`work_cover_blob`); served by `serve_cover`.
    /// A newtype (`CoverDb`) distinguishes it from the main `pool` in `FromRef`.
    cover_pool: crate::db::Database,
    trusted_proxies: Arc<TrustedProxies>,
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
impl FromRef<RouterState> for crate::db::Database {
    fn from_ref(s: &RouterState) -> Self {
        s.pool.clone()
    }
}
/// A reserved query-only connection keeps reader-pool contention from turning a
/// healthy process into an unhealthy readiness probe.
#[derive(Clone)]
struct ReadyDb(sqlx::SqlitePool);
impl ReadyDb {
    async fn connect(pool: &crate::db::Database) -> Result<Self, sqlx::Error> {
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(3))
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .map(Self)
    }
}
impl FromRef<RouterState> for ReadyDb {
    fn from_ref(s: &RouterState) -> Self {
        s.ready_pool.clone()
    }
}
impl FromRef<RouterState> for Arc<request_budget::RequestBudgets> {
    fn from_ref(s: &RouterState) -> Self {
        s.request_budgets.clone()
    }
}
/// Newtype for the cover-cache pool so a handler can extract it via `State`
/// without colliding with the main-pool `FromRef<RouterState> for Database`.
#[derive(Clone)]
struct CoverDb(crate::db::Database);
impl FromRef<RouterState> for CoverDb {
    fn from_ref(s: &RouterState) -> Self {
        CoverDb(s.cover_pool.clone())
    }
}
impl FromRef<RouterState> for Arc<TrustedProxies> {
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

/// Whether a request came in the clear from an address on the internet: not over
/// HTTPS (`OverTls` marks the HTTPS listener), and not through a trusted proxy, which
/// may have ended HTTPS itself. The home network, a VPN and this computer are not
/// "the internet". See `graphql::PlainFromInternet`.
///
/// A public peer is the proof. A private peer proves nothing behind Docker Desktop or
/// rootless Docker, where every connection comes from Docker's own address. So the
/// address the client typed counts too (`cloudflare::TypedHost`), unless HTTPS ended
/// in front of this server (`cloudflare::https_in_front`).
fn plain_from_internet(
    tls: Option<axum::Extension<cloudflare::origin_tls::OverTls>>,
    peer: SocketAddr,
    trusted: &[Cidr],
    headers: &HeaderMap,
) -> graphql::PlainFromInternet {
    use cloudflare::TypedHost;
    use graphql::PlainFromInternet as Plain;
    let ip = peer.ip().to_canonical();
    if tls.is_some() || trusted.iter().any(|c| c.contains(ip)) {
        return Plain::No;
    }
    if cloudflare::from_internet(ip) {
        return Plain::Yes;
    }
    if cloudflare::https_in_front(headers) {
        return Plain::No;
    }
    let typed = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    match typed.map(cloudflare::typed_host) {
        Some(TypedHost::Public) => Plain::Yes,
        Some(TypedHost::Name(name)) => Plain::Named(name),
        _ => Plain::No,
    }
}

async fn graphql_handler(
    State(schema): State<ApiSchema>,
    State(trusted): State<Arc<TrustedProxies>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    tls: Option<axum::Extension<cloudflare::origin_tls::OverTls>>,
    headers: HeaderMap,
    req: request_budget::AdmittedGraphQLRequest,
) -> axum::response::Response {
    let request_budget::AdmittedGraphQLRequest {
        request: req,
        lease,
    } = req;
    lease
        .run(async move {
            let auth = RequestAuth(bearer(&headers));
            let plain = plain_from_internet(tls, peer, &trusted.current(), &headers);
            let ip = ClientIp(Some(resolve_client_ip(&headers, peer, &trusted.current())));
            // Fresh per-request caches so `current_user` does at most one session lookup, the
            // viewer's NSFW preference at most one `users` read, and each series' `user_library`
            // row is read at most once — even when many resolvers ask (one per feed item).
            let user_cache = RequestUserCache::default();
            let nsfw_cache = RequestNsfwPref::default();
            let blocks_cache = RequestBlocks::default();
            let library_cache = RequestLibraryCache::default();
            let response = schema
                .execute(
                    req.into_inner()
                        .data(auth)
                        .data(ip)
                        .data(plain)
                        .data(user_cache)
                        .data(nsfw_cache)
                        .data(blocks_cache)
                        .data(library_cache),
                )
                .await;
            let unavailable = graphql_unavailable(&response);
            let mut response = GraphQLResponse::from(response).into_response();
            if unavailable {
                mark_unavailable(&mut response);
            }
            response
        })
        .await
}

fn graphql_unavailable(response: &async_graphql::Response) -> bool {
    response.errors.iter().any(|error| {
        error
            .extensions
            .as_ref()
            .and_then(|extensions| extensions.get("code"))
            == Some(&async_graphql::Value::String("SERVICE_UNAVAILABLE".into()))
    })
}

fn mark_unavailable(response: &mut axum::response::Response) {
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

fn busy_response() -> axum::response::Response {
    let mut response = Json(serde_json::json!({
        "data": null,
        "errors": [{ "message": "Server busy; retry soon", "extensions": { "code": "SERVICE_UNAVAILABLE" } }]
    })).into_response();
    mark_unavailable(&mut response);
    response
}

fn database_error_response(
    error: impl Into<anyhow::Error>,
    message: &str,
) -> axum::response::Response {
    let error = error.into();
    if error.chain().any(|cause| {
        cause.downcast_ref::<sqlx::Error>().is_some_and(|cause| {
            matches!(cause, sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed)
        })
    }) {
        busy_response()
    } else {
        avatar_error(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

/// Browser cache lifetime for anonymous GET /graphql responses. Matches the lower
/// bound of the in-process feed memo (30–60 s), so an edge/browser hit is never
/// staler than what the origin would have served from its own cache anyway.
const GRAPHQL_GET_MAX_AGE_SECS: u32 = 30;
/// Cloudflare edge lifetime for the same responses (s-maxage).
const GRAPHQL_GET_S_MAXAGE_SECS: u32 = 60;

/// P3.1: the CACHEABLE query endpoint. Same execution pipeline as
/// `graphql_handler`, with three differences:
///
/// - Only query operations are accepted — a GET must be safe per the
///   GraphQL-over-HTTP spec, and edge caches replay GET responses, so a mutation
///   here would be both wrong and dangerous. Rejected with 405.
/// - ANONYMOUS, error-free responses carry `Cache-Control: public` + a weak
///   `ETag` (If-None-Match answered with 304), so Cloudflare can absorb repeat
///   public reads and browsers can revalidate instead of refetching. Anonymous
///   responses are viewer-invariant by construction (one viewer class), which is
///   what makes URL-keyed caching sound.
/// - AUTHENTICATED responses are marked `private, no-store`: edge caches key on
///   the URL alone, and a cached authed body would leak one viewer's
///   `isMarked`/progress fields to the next requester of the same URL.
async fn graphql_get_handler(
    State(schema): State<ApiSchema>,
    State(trusted): State<Arc<TrustedProxies>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    tls: Option<axum::Extension<cloudflare::origin_tls::OverTls>>,
    headers: HeaderMap,
    req: request_budget::AdmittedGraphQLRequest,
) -> axum::response::Response {
    let request_budget::AdmittedGraphQLRequest {
        request: req,
        lease,
    } = req;
    let anonymous = bearer(&headers).is_none();
    let plain = plain_from_internet(tls, peer, &trusted.current(), &headers);
    let mut res = lease
        .run(graphql_get_response(
            schema, trusted, peer, plain, headers, req,
        ))
        .await;
    if anonymous {
        allow_any_origin(res.headers_mut());
    }
    res
}

/// Answer an ANONYMOUS `GET /graphql` with `Access-Control-Allow-Origin: *`, replacing the
/// allowlisted echo `CorsLayer` would otherwise put there.
///
/// WHAT THIS DOES AND DOES NOT WIDEN. It applies to one method on one route, and only when
/// the request carries no bearer token — the same condition that already makes the
/// response publicly cacheable (`cacheable` below). The bytes behind it are the anonymous
/// public catalogue, viewer-invariant by construction and served from a shared edge cache
/// to anyone who asks. `*` also FORBIDS credentialed requests, and this layer never sets
/// `Access-Control-Allow-Credentials`, so no cookie or authenticated read becomes readable
/// by another site: authenticated reads carry a token, take the POST path, and keep the
/// strict allowlist untouched.
///
/// WHY IT IS NEEDED. The reader's edge SSR fetches this endpoint through SvelteKit's
/// `load({ fetch })`, which SIMULATES CORS server-side "for consistency with client-side
/// behaviour": it throws unless the response's ACAO is `*` or EXACTLY the page's origin.
/// Using that fetch is what lets SvelteKit inline the SSR response into the HTML so
/// hydration replays it instead of refetching — the two GraphQL round trips that sat on
/// the home page's critical path at 706 ms and 963 ms.
///
/// An echo cannot satisfy that check reliably, which is why this is `*` and not something
/// narrower. Workers synthesize their OWN `Origin` on a subrequest — measured, it is the
/// worker's route host (`http://komiq.cc`), not the origin of the page being rendered
/// (`https://www.komiq.cc`) — so the echo we would send back is a value SvelteKit rejects
/// as "Incorrect", and the SSR degrades every public page to empty feeds behind a 60 s
/// edge cache. `*` is the only answer that is correct for every host the reader is served
/// on, today and after any future domain change.
fn allow_any_origin(headers: &mut HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
}

/// The GET /graphql pipeline proper. Split out of [`graphql_get_handler`] only so the
/// wrapper can post-process every one of its exit paths in one place.
async fn graphql_get_response(
    schema: ApiSchema,
    trusted: Arc<TrustedProxies>,
    peer: SocketAddr,
    plain: graphql::PlainFromInternet,
    headers: HeaderMap,
    req: GraphQLRequest,
) -> axum::response::Response {
    let inner = req.into_inner();
    // Reject non-query operations BEFORE execution. An unparseable document falls
    // through — execution produces the proper GraphQL error body for it.
    if !get_document_is_query_only(&inner.query) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            Json(serde_json::json!({
                "errors": [{ "message": "Only query operations may use GET; send mutations via POST" }]
            })),
        )
            .into_response();
    }
    let anonymous = bearer(&headers).is_none();
    let auth = RequestAuth(bearer(&headers));
    let ip = ClientIp(Some(resolve_client_ip(&headers, peer, &trusted.current())));
    let user_cache = RequestUserCache::default();
    let nsfw_cache = RequestNsfwPref::default();
    let blocks_cache = RequestBlocks::default();
    let library_cache = RequestLibraryCache::default();
    let resp = schema
        .execute(
            inner
                .data(auth)
                .data(ip)
                .data(plain)
                .data(user_cache)
                .data(nsfw_cache)
                .data(blocks_cache)
                .data(library_cache),
        )
        .await;
    // Only a clean anonymous response is cacheable: errors (rate limits, transient
    // failures) must never be replayed from an edge cache for 60 s.
    //
    let cacheable = get_response_cacheable(anonymous, &resp);
    let unavailable = graphql_unavailable(&resp);
    let body = match serde_json::to_vec(&resp) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "GET /graphql: response serialization failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if !cacheable {
        let mut response = (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CACHE_CONTROL, "private, no-store"),
            ],
            body,
        )
            .into_response();
        if unavailable {
            mark_unavailable(&mut response);
        }
        return response;
    }
    let etag = weak_etag(&body);
    let cache_control =
        format!("public, max-age={GRAPHQL_GET_MAX_AGE_SECS}, s-maxage={GRAPHQL_GET_S_MAXAGE_SECS}");
    let revalidated = if_none_match_matches(
        headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok()),
        &etag,
    );
    if revalidated {
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag), (header::CACHE_CONTROL, cache_control)],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, cache_control),
        ],
        body,
    )
        .into_response()
}

/// May a GET /graphql response be served with public `Cache-Control`? Only a
/// clean anonymous answer, and never one that selected a `no_cache` field
/// (`instance`: setup state and the sign-up switch change at runtime).
fn get_response_cacheable(anonymous: bool, resp: &async_graphql::Response) -> bool {
    anonymous && resp.is_ok() && resp.cache_control.max_age != -1
}

/// Does a GET /graphql document contain ONLY query operations? An unparseable
/// document answers `true` — it falls through to execution, which produces the
/// proper GraphQL error body (a 405 for garbage would be misleading).
fn get_document_is_query_only(query: &str) -> bool {
    use async_graphql::parser::types::{DocumentOperations, OperationType};
    match async_graphql::parser::parse_query(query) {
        Ok(doc) => match &doc.operations {
            DocumentOperations::Single(op) => op.node.ty == OperationType::Query,
            DocumentOperations::Multiple(ops) => {
                ops.iter().all(|(_, op)| op.node.ty == OperationType::Query)
            }
        },
        Err(_) => true,
    }
}

/// Weak ETag from a fast 64-bit hash of the body: a validator, not a security
/// boundary — a collision only risks one spurious 304 on content that changes
/// within its own 30 s browser TTL anyway.
fn weak_etag(body: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hasher::write(&mut h, body);
    format!("W/\"{:016x}\"", std::hash::Hasher::finish(&h))
}

/// RFC 9110 If-None-Match: a comma-separated list of entity tags (or `*`).
fn if_none_match_matches(header: Option<&str>, etag: &str) -> bool {
    header.is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
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
    State(pool): State<crate::db::Database>,
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
            return database_error_response(e, "Internal error");
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
    let decoding = UPLOAD_DECODE_LIMIT.acquire().await;
    let webp = match tokio::task::spawn_blocking(move || {
        let _decoding = decoding;
        avatar::process_avatar(&bytes)
    })
    .await
    {
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
        return database_error_response(e, "Could not save avatar");
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
            return database_error_response(e, "Internal error");
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
            return database_error_response(e, "Internal error");
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

    let decoding = UPLOAD_DECODE_LIMIT.acquire().await;
    let webp = match tokio::task::spawn_blocking(move || {
        let _decoding = decoding;
        cover::process_cover(&bytes)
    })
    .await
    {
        Ok(Ok(w)) => w,
        Ok(Err(e)) => return avatar_error(StatusCode::BAD_REQUEST, &e.to_string()),
        Err(e) => {
            tracing::error!(error = %e, "cover processing task panicked");
            return avatar_error(StatusCode::INTERNAL_SERVER_ERROR, "Could not process image");
        }
    };

    if let Err(e) = cover::put_work_cover(&app.pool, &app.cover_pool, &work_id, &webp).await {
        tracing::error!(error = %e, work_id = %work_id, "cover upload: store failed");
        return database_error_response(e, "Could not save cover");
    }
    cover::clear_cover_issue(&app.pool, &work_id).await;
    graphql::clear_feed_caches();

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
///   2. `extension_icon_blob`, holding icons fetched since that snapshot (new
///      Keiyoushi extensions, and every extension from any other repo), plus
///      tombstones for the ones that have no icon;
///   3. a one-time fetch from the extension's OWN repo (`ext_icon::icon_candidates`),
///      cached into (2) for next time.
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
    // Tier 3: ask the extension's own repo once, then serve from tier 2 forever
    // after. The repo is the one the engine reports for the package — first from the
    // catalogue's record of installed extensions, then the engine's store listing.
    let recorded: Option<String> =
        sqlx::query_scalar("SELECT repo_url FROM source_extension WHERE pkg_name = ? LIMIT 1")
            .bind(pkg)
            .fetch_optional(&app.pool)
            .await
            .ok()
            .flatten();
    let engine = match recorded {
        Some(_) => None,
        None => Some(app.suwayomi.get_extension(pkg).await),
    };
    // An extension the engine knows without a repo (installed before repos were
    // tracked): on a server whose admin added Keiyoushi's repo, treat it as one of
    // Keiyoushi's, as before. A server without that repo asks nobody.
    let keiyoushi_store = match &engine {
        Some(Ok(e)) if e.repo.is_none() => {
            Some(app.suwayomi.list_extension_stores().await.map(|stores| {
                stores.into_iter().map(|s| s.index_url).find(|url| {
                    ext_icon::repo_base(url).as_deref() == Some(ext_icon::KEIYOUSHI_REPO_BASE)
                })
            }))
        }
        _ => None,
    };
    let repo = match (&recorded, &engine, &keiyoushi_store) {
        (Some(r), _, _) => ext_icon::RepoLookup::Repo(r),
        (None, Some(Ok(e)), None) => match e.repo.as_deref() {
            Some(r) => ext_icon::RepoLookup::Repo(r),
            None => ext_icon::RepoLookup::Unknown,
        },
        (None, _, Some(Ok(Some(url)))) => ext_icon::RepoLookup::Repo(url),
        (None, _, Some(Ok(None))) => ext_icon::RepoLookup::Unknown,
        (None, _, _) => ext_icon::RepoLookup::Unavailable,
    };
    match ext_icon::fetch_and_cache(&pool, pkg, repo).await {
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
    State(pool): State<crate::db::Database>,
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
                return database_error_response(e, "Internal error");
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
/// coverless work 404s, and so does every miss while MangaDex is switched off.
async fn serve_cover(
    State(app): State<Arc<graphql::AppState>>,
    State(CoverDb(pool)): State<CoverDb>,
    UrlPath(file): UrlPath<String>,
    RawQuery(query): RawQuery,
) -> axum::response::Response {
    let Some(work_id) = file.strip_suffix(".webp") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // `?v=` (cache bust) is ignored here; only `?w=` changes what we serve.
    let width = cover::variant_width(query_param(&query, "w"));
    // Fast path: already-cached blob.
    match sqlx::query_scalar::<_, Vec<u8>>("SELECT webp FROM work_cover_blob WHERE work_id = ?")
        .bind(work_id)
        .fetch_optional(&pool)
        .await
    {
        Ok(Some(bytes)) => {
            return cover_response_at_width(&pool, work_id, width, bytes).await;
        }
        Ok(None) => {} // fall through to lazy fetch
        Err(e) => {
            tracing::warn!(error = %e, "cover read failed");
            return database_error_response(e, "Internal error");
        }
    }
    // Lazy cache: this URL is only built for MangaDex-anchored works, so look up the
    // anchor and materialize the cover on first request.
    let Some((mangadex_id, file_name)) = cover::mangadex_cover_anchor(&app.pool, work_id).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // MangaDex switched off: no lazy fetch and, just as important, no redirect to the
    // MangaDex CDN (that would only move the MangaDex request into the browser). A
    // plain miss the reader renders as a placeholder; `no-store` so the real cover
    // shows once the drainer (Suwayomi pass, or MangaDex when it is back on) fills it.
    if !app.mangadex.is_enabled() {
        return (StatusCode::NOT_FOUND, [(header::CACHE_CONTROL, "no-store")]).into_response();
    }
    let cdn_fallback = crate::mangadex::cover_thumb_url(&mangadex_id, &file_name);
    let work = cover::master_cover_work().await;
    let Some(bytes) = app
        .mangadex
        .cover_thumb_bytes(&mangadex_id, &file_name)
        .await
    else {
        return axum::response::Redirect::temporary(&cdn_fallback).into_response();
    };
    // Decode + resize + WebP encode is CPU-bound — keep it off the async runtime.
    let resized = work.process(move || cover::process_cover(&bytes)).await;
    match resized {
        Ok(Ok(webp)) => {
            let work = work_id.to_owned();
            let bytes = webp.clone();
            let main = app.pool.clone();
            let covers = pool.clone();
            app.pool.defer(async move {
                cover::put_work_cover(&main, &covers, &work, &bytes).await?;
                cover::clear_cover_issue(&main, &work).await;
                Ok(())
            });
            cover_response_at_width(&pool, work_id, width, webp).await
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
    RawQuery(query): RawQuery,
) -> axum::response::Response {
    if !is_suwayomi_image_path(&rest) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Covers (`{id}/thumbnail`) are downscaled to a bounded WebP and origin-cached —
    // the feeds serve these full-resolution (avg ~0.8 MB, up to several MB) at card
    // size, so resizing is a ~40x transfer win. Chapter PAGES are served raw: the
    // reader needs full-resolution page images.
    if let Some(manga_id) = thumbnail_manga_id(&rest) {
        let width = cover::variant_width(query_param(&query, "w"));
        return serve_suwayomi_cover(&app, &covers, manga_id, width).await;
    }
    // The owning manga (`is_suwayomi_image_path` already proved the first segment numeric).
    let manga_id = rest.split('/').next().and_then(|id| id.parse::<i64>().ok());
    // A chapter page of a series a site-wide rule blocks is not served: `pages` refuses
    // to list its URLs, but the URL is three guessable numbers. Covers stay open, as the
    // admin console still lists a blocked series and its `<img>` carries no session.
    // Fails closed: a rule that cannot be read must not let the page out.
    if let Some(id) = manga_id {
        match graphql::site_blocks_engine_series(&app.pool, id).await {
            Ok(false) => {}
            Ok(true) => return StatusCode::NOT_FOUND.into_response(),
            Err(e) => {
                tracing::warn!(error = %e, "suwayomi page: block rules unreadable");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }
    let path = format!("/api/v1/manga/{rest}");
    // Bounded + timed page fetch (own pool, page timeout, byte cap). The old path called
    // the unbounded `fetch_image`, which shared the 30 s scan timeout and no concurrency
    // bound, so a burst of page requests (a chapter is tens of pages) could pile onto the
    // engine behind the scanner and each pay the full 30 s ceiling.
    // Per-source health buckets, by the owning manga.
    let health = |outcome: Result<(), &anyhow::Error>| {
        if let Some(id) = manga_id {
            source_health::observe(
                source_health::Subject::Manga(id),
                source_health::Surface::Image,
                &outcome,
            );
        }
    };
    match app.suwayomi.fetch_page_now(&path).await {
        Ok((bytes, content_type)) => {
            health(Ok(()));
            raw_image_response(content_type, bytes)
        }
        Err(crate::suwayomi::PageFetchError::Busy) => {
            // Saturated past the short wait — shed with a retryable 503 rather than queue.
            // `info`, not `warn`: a steady stream is the operator's signal that page demand
            // is outrunning the engine, not an error per request.
            tracing::info!(path = %path, "suwayomi page: pool saturated, 503");
            page_busy_response()
        }
        Err(crate::suwayomi::PageFetchError::Upstream(e)) => {
            health(Err(&e));
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
    covers: &crate::db::Database,
    manga_id: i64,
    width: Option<u32>,
) -> axum::response::Response {
    let variant_key = cover::suwayomi_variant_key(manga_id);
    if let Some(webp) = cover::get_suwayomi_cover(covers, manga_id).await {
        return cover_response_at_width(covers, &variant_key, width, webp).await;
    }
    let path = format!("/api/v1/manga/{manga_id}/thumbnail");
    let work = cover::master_cover_work().await;
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
    let resized = work
        .process(move || cover::process_cover(&bytes).map_err(|e| (e.to_string(), bytes)))
        .await;
    match resized {
        Ok(Ok(webp)) => {
            let writer = covers.clone();
            let bytes = webp.clone();
            covers.defer(async move { cover::put_suwayomi_cover(&writer, manga_id, &bytes).await });
            cover_response_at_width(covers, &variant_key, width, webp).await
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
    covers: &crate::db::Database,
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
        let work = cover::master_cover_work().await;
        let Ok((bytes, _ct)) = suwayomi.fetch_cover_background(&path).await else {
            return;
        };
        if let Ok(Ok(webp)) = work.process(move || cover::process_cover(&bytes)).await {
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

/// First value of `key` in a raw query string, or `None`. Deliberately hand-rolled
/// rather than a `Query<T>` extractor: a typed extractor REJECTS the whole request on a
/// value it cannot parse, and a cover URL carrying a stale or hand-edited `?w=` must
/// still render the cover. Values here are plain integers, so no percent-decoding is
/// needed — anything that does not parse falls through to the master.
fn query_param<'a>(raw: &'a Option<String>, key: &str) -> Option<&'a str> {
    raw.as_deref()?
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// Serve `master`, or an allowlisted `?w=` derivative of it, generating and caching that
/// derivative on first request.
///
/// A card renders a cover at 150–200 CSS px; the masters measure 240–364 px wide, so a
/// 1x viewer downloads roughly four times the pixels it can show and a 2x viewer still
/// downloads more than it can show. The derivatives are what the reader puts in the `1x`
/// and `2x` slots of its `srcset` (see `cover::VARIANT_WIDTHS`); only a 3x viewer gets
/// the master, which is the largest cover we hold.
///
/// A master already at or under the asked width yields no derivative — it is served
/// as-is and NOTHING is stored, so the 320 rung does not mint a byte-identical copy of
/// every narrow master.
///
/// Every failure mode falls back to the master: a cover that renders slightly too large
/// is strictly better than a hole in the grid.
async fn cover_response_at_width(
    covers: &crate::db::Database,
    asset_key: &str,
    width: Option<u32>,
    master: Vec<u8>,
) -> axum::response::Response {
    let Some(w) = width else {
        return webp_cover_response(master);
    };
    if let Some(bytes) = cover::get_cover_variant(covers, asset_key, w).await {
        return webp_cover_response(bytes);
    }
    // Bounded: unlike every other resize here, this one has no upstream fetch in front of
    // it to cap concurrency, so a cold grid would otherwise start ~30 at once. See
    // `cover::VARIANT_RESIZE_LIMIT`. The worker retains the permit even if the
    // HTTP route times out while its non-cancellable resize is still running.
    let permit = cover::variant_resize_permit().await;
    // Re-check under the permit: while we waited, another request for this same cover may
    // have generated and stored it. Skips a duplicate resize on exactly the stampede this
    // bound exists for.
    if let Some(bytes) = cover::get_cover_variant(covers, asset_key, w).await {
        return webp_cover_response(bytes);
    }
    // Decode + Lanczos3 resize + WebP encode is CPU-bound; keep it off the async runtime
    // exactly like `process_cover`'s callers do.
    let source = master.clone();
    let made = resize_cover_variant(permit, move || cover::downscale_variant(&source, w)).await;
    match made {
        // The master is already at or under `w` — serve it, store nothing.
        Ok(Ok(None)) => webp_cover_response(master),
        Ok(Ok(Some(bytes))) => {
            let writer = covers.clone();
            let asset = asset_key.to_owned();
            let cached = bytes.clone();
            covers
                .defer(async move { cover::put_cover_variant(&writer, &asset, w, &cached).await });
            webp_cover_response(bytes)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, asset_key, width = w, "cover variant resize failed");
            webp_cover_response(master)
        }
        Err(e) => {
            tracing::error!(error = %e, asset_key, width = w, "cover variant task panicked");
            webp_cover_response(master)
        }
    }
}

/// Move the resize slot into the blocking worker, whose lifetime can outlast the
/// HTTP response when the route's deadline or client cancellation drops its join.
fn resize_cover_variant<T: Send + 'static>(
    permit: tokio::sync::SemaphorePermit<'static>,
    resize: impl FnOnce() -> T + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        resize()
    })
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

/// The most a `POST /graphql` body may be.
const GRAPHQL_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// How many uploaded images are decoded at once, over all accounts. Decoding one large
/// image takes hundreds of megabytes; the per-account upload limit alone would let one
/// account start twenty at the same moment.
static UPLOAD_DECODE_LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

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
            // The bytes come from a source and are not re-encoded, and `image/*`
            // includes SVG, which can carry script. Opened as a page of its own, it
            // must not run with this origin's rights. An `<img>` is not affected.
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("sandbox; default-src 'none'"),
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
    State(pool): State<crate::db::Database>,
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
            return database_error_response(e, "Internal error");
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
    let decoding = UPLOAD_DECODE_LIMIT.acquire().await;
    let processed = match tokio::task::spawn_blocking(move || {
        let _decoding = decoding;
        media::process_comment_image(&bytes)
    })
    .await
    {
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
        return database_error_response(e, "Could not save image");
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
    State(pool): State<crate::db::Database>,
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
                return database_error_response(e, "Internal error");
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
/// Database gauges and small process counters expose writer contention without
/// requiring a metrics registry. The remaining gauges track scan-scheduler health
/// (the herd/backoff signals), the subscription circuit breaker, and updates-feed
/// freshness. Served on its OWN listener bound to 127.0.0.1:{metrics_port}, so it
/// is unreachable from outside the host by construction (no reliance on ingress
/// filtering). Even leaked it discloses only aggregate counts, not user data.
async fn metrics(State(pool): State<crate::db::Database>) -> impl IntoResponse {
    // One helper so a single failed query degrades to a `-1` gauge rather than 500ing
    // the whole scrape (a metrics endpoint that fails is worse than one with a hole).
    async fn scalar(pool: &crate::db::Database, sql: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .unwrap_or(-1)
    }

    let mut wal_path = pool.connect_options().get_filename().as_os_str().to_owned();
    wal_path.push("-wal");
    let wal_bytes = match std::fs::metadata(std::path::Path::new(&wal_path)) {
        Ok(metadata) => metadata.len() as i64,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(_) => -1,
    };
    let pool_size = pool.size();
    let pool_idle = pool.num_idle();
    let write_queue_wait = db::write_queue_wait_seconds();
    let write_queue_acquisitions = db::write_queue_acquisitions();
    let lock_errors = db::lock_error_count();
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
        "# HELP komika_sqlite_wal_bytes Main database WAL file size\n\
         # TYPE komika_sqlite_wal_bytes gauge\n\
         komika_sqlite_wal_bytes {wal_bytes}\n\
         # HELP komika_sqlite_pool_connections Main database read pool connections\n\
         # TYPE komika_sqlite_pool_connections gauge\n\
         komika_sqlite_pool_connections {pool_size}\n\
         # HELP komika_sqlite_pool_idle Main database idle read connections\n\
         # TYPE komika_sqlite_pool_idle gauge\n\
         komika_sqlite_pool_idle {pool_idle}\n\
         # HELP komika_sqlite_write_queue_wait_seconds_total Time waiting for the in-process writer\n\
         # TYPE komika_sqlite_write_queue_wait_seconds_total counter\n\
         komika_sqlite_write_queue_wait_seconds_total {write_queue_wait}\n\
         # HELP komika_sqlite_write_queue_acquisitions_total Writer acquisitions\n\
         # TYPE komika_sqlite_write_queue_acquisitions_total counter\n\
         komika_sqlite_write_queue_acquisitions_total {write_queue_acquisitions}\n\
         # HELP komika_sqlite_lock_errors_total Observed SQLite lock errors\n\
         # TYPE komika_sqlite_lock_errors_total counter\n\
         komika_sqlite_lock_errors_total {lock_errors}\n\
         # HELP komika_scan_state_total Series tracked by the scan scheduler\n\
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
async fn ready(State(ReadyDb(pool)): State<ReadyDb>) -> impl IntoResponse {
    match sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&pool)
        .await
    {
        Ok(_) => (axum::http::StatusCode::OK, "ready"),
        Err(e) => {
            tracing::warn!(error = %e, "readiness check failed");
            (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not ready")
        }
    }
}

/// Assemble the actual public router in one place so admission regressions test
/// the same routes and global middleware served by both HTTP and HTTPS.
fn public_router(
    cfg: &Config,
    router_state: RouterState,
    cloudflare: Arc<cloudflare::Manager>,
) -> Router {
    let mangadex = router_state.app_state.mangadex.clone();
    let request_budgets = router_state.request_budgets.clone();
    let origins: Vec<_> = cfg
        .cors_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

    // P3.1: GET /graphql is the CACHEABLE query endpoint (anonymous public reads get
    // Cache-Control + ETag so Cloudflare and browsers can serve/revalidate them —
    // POST is uncacheable by every CDN). Mutations over GET are rejected in the
    // handler. GraphiQL moved to its own GET /graphiql when enabled; introspection
    // stays disabled in the same off state (see build_schema above).
    let mut app = Router::new().route(
        "/graphql",
        get(graphql_get_handler)
            .post(graphql_handler)
            // The GraphQL extractor reads the whole body, and a multipart one into a
            // temporary file, before any resolver checks who is asking. Nothing the
            // apps send comes near this size; files have their own upload routes.
            .layer(RequestBodyLimitLayer::new(GRAPHQL_BODY_LIMIT)),
    );
    if cfg.graphiql_enabled {
        app = app.route("/graphiql", get(graphiql));
    }
    let app = app
        .route("/health", get(health))
        .route("/health/ready", get(ready))
        // Operational gauges for scraping are NOT on this router: `/metrics` is
        // served on a separate 127.0.0.1-bound listener below, so the scrape
        // endpoint is unreachable from outside the host by construction
        // (aggregate counts only — no series content, no user data).
        // Authenticated avatar upload + public serve (VM data volume). The upload
        // route raises the body limit above the raw-image cap enforced in
        // `avatar::process_avatar` (axum's default is 2 MB).
        .route(
            "/avatar",
            post(upload_avatar)
                .layer::<_, std::convert::Infallible>(axum::middleware::from_fn_with_state(
                    request_budgets.clone(),
                    request_budget::limit_uploads,
                ))
                .layer(DefaultBodyLimit::max(
                    avatar::MAX_UPLOAD_BYTES + 1024 * 1024,
                )),
        )
        .route("/avatars/{file}", get(serve_avatar))
        // Public serve of DB-backed work covers (WebP BLOB in `work_cover_blob`).
        // Lets the web reader load covers from our own origin instead of the CF
        // image Worker; the bytes are materialized by `cover::crawl_uncached_covers`
        // (admin `materializeCatalogueCovers`).
        .route(
            "/covers/{file}",
            get(serve_cover).layer(axum::middleware::from_fn_with_state(
                std::time::Duration::from_secs(120),
                request_budget::limit_image_time,
            )),
        )
        // Public serve of extension icons from the vendored snapshot baked into
        // this image, with a lazy fetch from each extension's own repo for the rest.
        // Hosted here because Keiyoushi's own icon directory was emptied in their
        // `index.pb` migration (see the `ext_icon` module).
        .route(
            "/ext-icons/{file}",
            get(serve_ext_icon).layer(axum::middleware::from_fn_with_state(
                std::time::Duration::from_secs(120),
                request_budget::limit_image_time,
            )),
        )
        // Admin: replace ONE work's cover from an uploaded image (the "Bugs" panel's
        // manual-upload action for covers the crawl can't process). Same multipart
        // model as avatars; body limit raised above the raw-image cap.
        .route(
            "/admin/cover/{work_id}",
            post(upload_cover)
                .layer::<_, std::convert::Infallible>(axum::middleware::from_fn_with_state(
                    request_budgets.clone(),
                    request_budget::limit_uploads,
                ))
                .layer(DefaultBodyLimit::max(cover::MAX_SOURCE_BYTES + 1024 * 1024)),
        )
        // Public proxy for Suwayomi-source cover thumbnails + chapter pages. Suwayomi
        // is loopback-only, so its image endpoints are unreachable from the browser;
        // these URLs point HERE (SUWAYOMI_PUBLIC_URL=https://api.komiq.cc) and we fetch
        // the bytes internally. Restricted to the two numeric image paths
        // (`is_suwayomi_image_path`) so it is not a general Suwayomi REST proxy.
        .route(
            "/api/v1/manga/{*rest}",
            get(serve_suwayomi_image).layer(axum::middleware::from_fn_with_state(
                std::time::Duration::from_secs(60),
                request_budget::limit_image_time,
            )),
        )
        // Authenticated comment-image upload + public serve, same BLOB-in-SQLite
        // model as avatars. The upload route raises the body limit above the raw
        // image cap enforced in `media::process_comment_image`.
        .route(
            "/comment-media",
            post(upload_comment_media)
                .layer::<_, std::convert::Infallible>(axum::middleware::from_fn_with_state(
                    request_budgets.clone(),
                    request_budget::limit_uploads,
                ))
                .layer(DefaultBodyLimit::max(media::MAX_UPLOAD_BYTES + 1024 * 1024)),
        )
        .route("/comment-media/{file}", get(serve_comment_media));
    // Self-host: the server's own image proxy (`img_proxy`), the stand-in for the
    // `img.komiq.cc` Worker. Mounted only where it is wanted — a server that serves
    // the web apps (`KOMIKA_WEB_DIR`) or one told to (`KOMIKA_IMG_PROXY=on`) — so the
    // hosted API, which has the Worker, exposes no new route.
    let app = if cfg.img_proxy_enabled {
        tracing::info!("image proxy enabled at /img");
        app.route(
            "/img",
            get(img_proxy::serve)
                .with_state(Arc::new(img_proxy::ImgProxy::new(mangadex.clone())))
                .layer(axum::middleware::from_fn_with_state(
                    std::time::Duration::from_secs(180),
                    request_budget::limit_image_time,
                )),
        )
    } else {
        app
    };
    // Self-host: the built reader at `/` and admin at `/admin`, below every API
    // route above. Unset (the default, and the hosted deployment) serves nothing.
    let app = match &cfg.web_dir {
        Some(dir) => {
            tracing::info!(dir, "serving web apps from KOMIKA_WEB_DIR");
            web_dir::mount(app, std::path::Path::new(dir))
        }
        None => app,
    };
    let app = app
        // Compress compressible responses (JSON payloads — the home feed alone drops
        // 64 KB → ~22 KB). Innermost of the middleware so the request-id/trace span
        // and every outer layer observe the final (encoded) body. The default
        // predicate leaves image/* and already-encoded responses alone.
        .layer(CompressionLayer::new())
        // Self-host "public without a domain" with HTTPS: plain HTTP from the internet
        // is sent to the HTTPS address instead of being handled. A no-op otherwise.
        .layer(axum::middleware::from_fn_with_state(
            cloudflare.clone(),
            cloudflare::plain_http_guard,
        ))
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
        .with_state(router_state);
    app
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
    // `komika-server setup` — the first-run wizard in a terminal (`setup_cli`). It is a
    // client of the server that is already running, so it builds no server state.
    if argv.first().map(String::as_str) == Some("setup") {
        return setup_cli::run(cfg.port, &argv[1..]).await;
    }
    if argv.first().map(String::as_str) == Some("phase-f") {
        let pool = db::connect_existing(&cfg.database_url).await?;
        let cover_pool = db::connect_existing_covers(&cfg.covers_database_url).await?;
        let result =
            phase_f::run_cli(&pool, Some(&cover_pool), &cfg.suwayomi_url, &argv[1..]).await;
        // Close both pools explicitly: a `merge` stage holds a WAL that a still-open
        // pool would keep pinned, and this process runs alongside the live server.
        pool.close().await;
        cover_pool.close().await;
        return result;
    }

    tracing::info!(?cfg, "starting komika-server");

    let _data_directory_lock = db::DataDirectoryLock::acquire(&cfg.database_url)?;
    let pool = db::init(&cfg.database_url).await?;
    let ready_pool = ReadyDb::connect(&pool).await?;
    // Separate, un-replicated DB for cover blobs — see `db::init_covers`. Kept out
    // of the main DB so Litestream ships only accounts/social/catalogue to R2, never
    // the large (re-derivable) cover thumbnails.
    let cover_pool = db::init_covers(&cfg.covers_database_url).await?;

    // Covers are re-derivable. Repair dangling pointers in bounded transactions,
    // including a partly lost cache and a cache recreated after corruption.
    let cleared = db::reconcile_cover_pointers(&pool, &cover_pool).await?;
    if cleared > 0 {
        tracing::warn!(
            cleared,
            "cleared missing cover pointers; covers will re-cache"
        );
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
    // The MangaDex USAGE switch (`instance::resolve_mangadex`): `KOMIKA_MANGADEX`, then
    // `CATALOGUE_SYNC=on`, then the admin's stored choice, then on (off on a self-host).
    // Read once here to set the client; from then on `setMangadexEnabled` flips the live
    // client and the table together, and every loop reads the live value each pass.
    let mangadex_usage = instance::resolve_mangadex(&pool).await;
    mangadex.set_enabled(mangadex_usage.enabled);
    // The CRAWL is a separate, per-replica decision (`instance::catalogue_crawl`):
    // `CATALOGUE_SYNC` still means "run the catalogue sync on this replica".
    let crawl = instance::catalogue_crawl(|name| std::env::var(name).ok());
    tracing::info!(
        enabled = mangadex_usage.enabled,
        source = ?mangadex_usage.source,
        pinned_by = mangadex_usage.pinned_by.unwrap_or("-"),
        crawl = ?crawl,
        "mangadex switch"
    );
    let state = Arc::new(AppState {
        pool: pool.clone(),
        cover_pool: cover_pool.clone(),
        suwayomi,
        mangadex: mangadex.clone(),
        admin_users: cfg.admin_users.clone(),
        scan_health: std::sync::Mutex::new(ScanHealth::default()),
        auth_limiter: RateLimiter::new(cfg.auth_rate_limit_max, cfg.auth_rate_limit_window_secs),
        comment_limiter: graphql::comment_limiter(),
        password_work: std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
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
        setup_code: instance::SetupCode::default(),
    });

    // First-run setup: with no admin (none in the DB and none seeded from env just
    // above), issue the one-time code the setup screen trades for the owner account.
    instance::issue_setup_code_if_needed(&pool, &state.setup_code).await?;

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
    maintenance::spawn(
        state.clone(),
        cfg.extension_auto_update_interval_seconds,
        shutdown_tx.subscribe(),
    );

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

    // View-write coalescing: `views::record` buffers in memory; this flusher batches
    // the buffered views into one transaction every few seconds. The handle is awaited
    // after serve() returns so the final on-shutdown flush completes before exit.
    let views_flusher = views::spawn_flusher(pool.clone(), shutdown_tx.subscribe());
    // Same idea for the per-source request health counters (`source_health`).
    let health_flusher = source_health::spawn_flusher(pool.clone(), shutdown_tx.subscribe());

    // Keep `sqlite_stat1` current so the planner keeps choosing migration 0058's
    // indexes. Refresh each sampled index in a separate writer turn.
    db::spawn_analyze(pool.clone(), shutdown_tx.subscribe());
    // Local catalogue/search repair must run even with CATALOGUE_SYNC=off.
    mangadex::spawn_reconciler(
        pool.clone(),
        cfg.feed_reconcile_interval_secs,
        shutdown_tx.subscribe(),
    );

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
    //
    // Spawned for `CATALOGUE_SYNC=on` (as always) and, on a self-host with it unset,
    // to follow the admin's switch. Either way each loop parks on
    // `MangaDexClient::wait_enabled` while usage is off: a parked loop is one idle task,
    // and the switch starts it without a restart. `CATALOGUE_SYNC=off`, and unset on a
    // hosted server, never spawn it — exactly the old behaviour.
    if crawl != instance::CatalogueCrawl::Never {
        mangadex::spawn_recurring(
            pool.clone(),
            mangadex.clone(),
            cfg.catalogue_cover_phash,
            cfg.catalogue_sync_interval_secs,
            cfg.chapter_sync_interval_secs,
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
        // Same gate, same limiter, same shape: fills `chapter.scanlator` (the scanlation
        // group) on MangaDex chapters mirrored before the firehose asked for groups. Its
        // work-list is `scanlator IS NULL` on a MangaDex row (migration 0105's partial
        // index), so it resumes itself and costs an hourly empty-index probe once drained.
        mangadex::spawn_group_backfill(pool.clone(), mangadex.clone(), shutdown_tx.subscribe());
        // Same gate, same limiter, same shape: MangaDex follows + rating into `work_stats`
        // (migration 0106) for Browse's MOST_FOLLOWS / HIGHEST_RATED sorts and its
        // minFollows / minRating filters. 100 ids per request, 0.5 req/s; never-fetched works
        // first, then anything older than a week, so it never finishes for good — it idles
        // on an hourly probe between weekly refreshes.
        work_stats::spawn_stats_backfill(pool.clone(), mangadex.clone(), shutdown_tx.subscribe());
        match (mangadex_usage.enabled, mangadex_usage.pinned_by) {
            (true, _) => {}
            (false, Some(var)) => {
                tracing::info!(var, "catalogue sync waiting: MangaDex usage is pinned off")
            }
            (false, None) => tracing::info!(
                "catalogue sync waiting: MangaDex is off (turn it on in the admin console)"
            ),
        }
    } else {
        tracing::info!("catalogue sync disabled (set CATALOGUE_SYNC=on to enable)");
    }

    // Recurring auto-enrichment drainer (X1) — opt-in. Keeps newly-ingested
    // MangaDex-anchored works self-enriching (S2 metadata + F2 covers) in small
    // polite batches; shares the MangaDex rate limiter. `METADATA_BACKFILL=on` is an
    // ADDITIONAL opt-in on top of the MangaDex switch: each tick still checks it.
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
    // With MangaDex off it keeps running for Suwayomi covers and skips only its
    // MangaDex pass (see `cover::spawn`).
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

    // One-shot recompression of pre-switch LOSSLESS cover masters (150–200 KB each) into
    // the lossy encoding the pipeline produces today (20–40 KB). Pure local CPU — it
    // re-encodes the blob we already hold rather than refetching from MangaDex — so it
    // runs on every replica and stops by itself once the walk completes.
    if cfg.cover_recompress_enabled {
        cover_recompress::spawn(
            pool.clone(),
            cover_pool.clone(),
            cfg.cover_recompress_interval_secs,
            cfg.cover_recompress_batch,
            shutdown_tx.subscribe(),
        );
    } else {
        tracing::info!("cover recompress pass disabled (COVER_RECOMPRESS=off)");
    }

    // Trusted proxies (env, else what Cloudflare setup stored) and the Cloudflare
    // setup manager. The manager does nothing at boot unless a setup is stored.
    let trusted_proxies = Arc::new(TrustedProxies::load(&pool).await);
    let cloudflare = Arc::new(cloudflare::Manager::new(
        pool.clone(),
        cloudflare::Options::from_env(&cfg),
        trusted_proxies.clone(),
    ));

    let schema = graphql::schema_builder(state.clone(), !cfg.graphiql_enabled)
        .data(cloudflare.clone())
        .finish();

    let app = public_router(
        &cfg,
        RouterState {
            schema,
            pool: pool.clone(),
            ready_pool: ready_pool.clone(),
            request_budgets: Arc::new(request_budget::RequestBudgets::default()),
            cover_pool,
            trusted_proxies: trusted_proxies.clone(),
            upload_limiter: Arc::new(RateLimiter::new(20, 60)),
            app_state: state,
        },
        cloudflare.clone(),
    );

    // Loopback-only metrics listener, separate from the public router: binding to
    // 127.0.0.1 makes the scrape endpoint unreachable off-host by construction.
    // The handler extracts `State<Database>`, so a minimal router over the main
    // pool is all it needs. No graceful shutdown of its own — a detached task lets
    // the process exit when `axum::serve` on the public port returns (an in-flight
    // metrics scrape dying at shutdown is immaterial).
    // Public-IP mode serves this same router over HTTPS; a stored setup also brings
    // its tunnel connector back. Both are no-ops on a server never set up that way.
    cloudflare.set_router(app.clone());
    cloudflare.resume().await;

    let metrics_app = Router::new()
        .route("/metrics", get(metrics))
        .with_state(pool.clone());
    let metrics_addr = format!("127.0.0.1:{}", cfg.metrics_port);
    let metrics_listener = tokio::net::TcpListener::bind(&metrics_addr).await?;
    tracing::info!("metrics listening on http://{metrics_addr}/metrics (loopback only)");
    tokio::spawn(async move {
        let _ = axum::serve(metrics_listener, metrics_app).await;
    });

    let bind_ip: IpAddr = cfg.bind_addr.parse().map_err(|_| {
        anyhow::anyhow!(
            "BIND_ADDR must be an IP address (e.g. 0.0.0.0 or 127.0.0.1), got {:?}",
            cfg.bind_addr
        )
    })?;
    let addr = SocketAddr::new(bind_ip, cfg.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{addr}/graphql");
    let shutdown_deadline = shutdown_tx.subscribe();
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown_tx));
    // Wait for the view flusher's final drain so buffered views hit the DB — then
    // drain ONCE more: the flusher's shutdown flush races axum's graceful drain, so
    // a `recordView` served while connections were draining can land in the buffer
    // AFTER that flush (and if the flusher died in its supervisor backoff, its
    // shutdown flush never ran at all). This last call catches both.
    finish_with_shutdown_deadline(shutdown_deadline, async {
        server.await?;
        let _ = views_flusher.await;
        let _ = health_flusher.await;
        if let Err(e) = views::flush(&pool).await {
            tracing::warn!(error = %e, "post-shutdown view flush failed; buffered views lost");
        }
        ready_pool.0.close().await;
        Ok(())
    })
    .await?;
    Ok(())
}

/// Leave ten seconds of Compose's 30s stop window for Litestream's final sync.
/// The deadline covers both HTTP connections and the final buffered-write drain.
async fn finish_with_shutdown_deadline<F>(
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    drain: F,
) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    let deadline = async {
        while !*shutdown.borrow() {
            if shutdown.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
    };
    tokio::select! {
        result = drain => result,
        _ = deadline => {
            tracing::warn!("shutdown drain reached its 20s deadline; leaving time for backup sync");
            Ok(())
        }
    }
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

    #[tokio::test]
    async fn cancelled_cover_variant_request_retains_resize_slot_until_worker_finishes() {
        static TEST_RESIZE_LIMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let permit = TEST_RESIZE_LIMIT.acquire().await.unwrap();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let (finish, finish_wait) = std::sync::mpsc::channel();
        let request = tokio::spawn(async move {
            resize_cover_variant(permit, move || {
                started.send(()).unwrap();
                finish_wait.recv().unwrap();
            })
            .await
            .unwrap();
        });
        waiting.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(TEST_RESIZE_LIMIT.available_permits(), 0);
        assert!(
            TEST_RESIZE_LIMIT.try_acquire().is_err(),
            "cancelled request freed a running worker's slot"
        );
        finish.send(()).unwrap();
        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            TEST_RESIZE_LIMIT.acquire(),
        )
        .await
        .unwrap()
        .unwrap();
        drop(permit);
        assert_eq!(TEST_RESIZE_LIMIT.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_deadline_bounds_a_hanging_drain_after_the_signal() {
        let (signal, receiver) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(finish_with_shutdown_deadline(
            receiver,
            std::future::pending(),
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        assert!(!task.is_finished(), "deadline started before shutdown");
        signal.send(true).unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(19)).await;
        assert!(!task.is_finished());
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shutdown_deadline_keeps_completed_drain_errors() {
        let (_signal, receiver) = tokio::sync::watch::channel(false);
        let error = finish_with_shutdown_deadline(receiver, async { anyhow::bail!("drain error") })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "drain error");
    }

    #[tokio::test]
    async fn database_metrics_include_wal_pool_and_contention_counters() {
        let directory =
            std::env::temp_dir().join(format!("komiq-metrics-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let database = directory.join("main.sqlite3");
        let pool = db::init(&format!("sqlite://{}", database.display()))
            .await
            .unwrap();
        let response = metrics(State(pool.clone())).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), 100_000)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        let wal_bytes = std::fs::metadata(directory.join("main.sqlite3-wal"))
            .unwrap()
            .len();
        assert!(body.contains(&format!("komika_sqlite_wal_bytes {wal_bytes}\n")));
        for metric in [
            "komika_sqlite_pool_connections ",
            "komika_sqlite_pool_idle ",
            "komika_sqlite_write_queue_wait_seconds_total ",
            "komika_sqlite_write_queue_acquisitions_total ",
            "komika_sqlite_lock_errors_total ",
        ] {
            assert!(
                body.lines().any(|line| line.starts_with(metric)),
                "missing {metric}"
            );
        }
        pool.close().await;
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn readiness_has_a_reserved_query_only_connection_when_readers_are_exhausted() {
        let pool = crate::db::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let reserved = ReadyDb::connect(&pool).await.unwrap();
        let mut readers = Vec::new();
        for _ in 0..pool.read().options().get_max_connections() {
            readers.push(pool.read().acquire().await.unwrap());
        }
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ready(State(reserved.clone())),
        )
        .await
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let query_only: i64 = sqlx::query_scalar("PRAGMA query_only")
            .fetch_one(&reserved.0)
            .await
            .unwrap();
        assert_eq!(query_only, 1);
        drop(readers);
        reserved.0.close().await;
        pool.close().await;
    }

    #[tokio::test]
    async fn production_router_keeps_health_and_images_outside_graphql_admission() {
        use tower::ServiceExt;
        let pool = crate::db::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations")
            .run(pool.write())
            .await
            .unwrap();
        let covers = db::init_covers("sqlite::memory:").await.unwrap();
        sqlx::query("INSERT INTO work_cover_blob(work_id, webp, version, updated_at) VALUES ('cached', X'0102', 1, 't')")
            .execute(&covers).await.unwrap();
        let reserved = ReadyDb::connect(&pool).await.unwrap();
        let budgets = Arc::new(request_budget::RequestBudgets::default());
        let mangadex = Arc::new(mangadex::MangaDexClient::new("router-test", 50.0, 600.0));
        mangadex.set_enabled(false);
        let state = Arc::new(AppState {
            pool: pool.clone(),
            cover_pool: covers.clone(),
            suwayomi: SuwayomiClient::new("http://127.0.0.1:1".into(), None, None),
            mangadex,
            admin_users: vec![],
            scan_health: std::sync::Mutex::new(ScanHealth::default()),
            auth_limiter: RateLimiter::new(100, 60),
            comment_limiter: graphql::comment_limiter(),
            password_work: Arc::new(tokio::sync::Semaphore::new(4)),
            federated_limiter: RateLimiter::new(100, 60),
            session_ttl_secs: 3600,
            series_inflight: KeyedLocks::default(),
            chapters_inflight: KeyedLocks::default(),
            cover_crawl_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            catalogue_cover_phash: false,
            ext_icons_dir: std::path::PathBuf::new(),
            setup_code: instance::SetupCode::default(),
        });
        let trusted = Arc::new(TrustedProxies::for_tests(vec![], false));
        let mut cfg = Config::from_env();
        cfg.img_proxy_enabled = true;
        cfg.graphiql_enabled = true;
        cfg.web_dir = None;
        let cloudflare = Arc::new(cloudflare::Manager::new(
            pool.clone(),
            cloudflare::Options::from_env(&cfg),
            trusted.clone(),
        ));
        let schema = graphql::schema_builder(state.clone(), true)
            .data(cloudflare.clone())
            .finish();
        let app = public_router(
            &cfg,
            RouterState {
                schema,
                pool: pool.clone(),
                ready_pool: reserved.clone(),
                request_budgets: budgets.clone(),
                cover_pool: covers.clone(),
                trusted_proxies: trusted,
                upload_limiter: Arc::new(RateLimiter::new(20, 60)),
                app_state: state,
            },
            cloudflare,
        );
        let request = |path: &str| {
            let mut request = axum::http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));
            request
        };
        let held = budgets.exhaust_for_test().await;
        for (path, expected) in [
            ("/health", StatusCode::OK),
            ("/health/ready", StatusCode::OK),
            ("/graphiql", StatusCode::OK),
            ("/covers/cached.webp", StatusCode::OK),
            ("/img", StatusCode::BAD_REQUEST),
            ("/api/v1/manga/not-numeric", StatusCode::NOT_FOUND),
            ("/ext-icons/not..valid.png", StatusCode::NOT_FOUND),
            ("/avatars/missing.webp", StatusCode::NOT_FOUND),
            ("/comment-media/missing.webp", StatusCode::NOT_FOUND),
        ] {
            let response = app.clone().oneshot(request(path)).await.unwrap();
            assert_eq!(
                response.status(),
                expected,
                "actual production route {path}"
            );
        }
        let graphql_path = "/graphql?query=%7Bsession%7Btoken%7D%7D";
        assert_eq!(
            app.clone()
                .oneshot(request(graphql_path))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(held);
        let (images, api, health) = tokio::join!(
            futures::future::join_all(
                (0..140).map(|_| app.clone().oneshot(request("/covers/cached.webp")))
            ),
            app.clone().oneshot(request(graphql_path)),
            app.clone().oneshot(request("/health/ready")),
        );
        assert!(images
            .into_iter()
            .all(|response| response.unwrap().status() == StatusCode::OK));
        assert_eq!(api.unwrap().status(), StatusCode::OK);
        assert_eq!(health.unwrap().status(), StatusCode::OK);
        reserved.0.close().await;
        covers.close().await;
        pool.close().await;
    }

    #[test]
    fn graphql_pool_saturation_is_a_retryable_http_failure() {
        use async_graphql::ErrorExtensions;
        let error = async_graphql::Error::new("Server busy; retry soon")
            .extend_with(|_, extensions| extensions.set("code", "SERVICE_UNAVAILABLE"))
            .into_server_error(async_graphql::Pos::default());
        let graph = async_graphql::Response::from_errors(vec![error]);
        assert!(graphql_unavailable(&graph));
        let mut response = GraphQLResponse::from(graph).into_response();
        mark_unavailable(&mut response);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }

    /// "In the clear from the internet" is plain HTTP, a public peer, and no trusted
    /// proxy in between. `createOwner` is refused only there.
    #[test]
    fn plain_from_internet_is_a_public_peer_without_https_or_a_proxy() {
        use graphql::PlainFromInternet as Plain;
        let tls = || Some(axum::Extension(cloudflare::origin_tls::OverTls));
        let peer = |ip: &str| -> SocketAddr { format!("{ip}:5000").parse().unwrap() };
        let proxy = [Cidr::parse("203.0.113.0/24").unwrap()];
        let none = HeaderMap::new();
        let yes = |p: Plain| matches!(p, Plain::Yes);

        assert!(yes(plain_from_internet(
            None,
            peer("198.51.100.7"),
            &proxy,
            &none
        )));
        // IPv4 on a dual-stack socket.
        assert!(yes(plain_from_internet(
            None,
            peer("[::ffff:198.51.100.7]"),
            &[],
            &none
        )));
        assert!(!yes(plain_from_internet(
            tls(),
            peer("198.51.100.7"),
            &[],
            &none
        )));
        assert!(!yes(plain_from_internet(
            None,
            peer("203.0.113.9"),
            &proxy,
            &none
        )));
        for home in [
            "127.0.0.1",
            "192.168.1.20",
            "10.0.0.4",
            "100.64.0.9",
            "[::1]",
        ] {
            assert!(
                matches!(plain_from_internet(None, peer(home), &[], &none), Plain::No),
                "{home}"
            );
        }
    }

    /// Behind Docker Desktop or rootless Docker every peer is Docker's own private
    /// address. There the address the client typed decides.
    #[test]
    fn a_private_peer_is_judged_by_the_address_the_client_typed() {
        use graphql::PlainFromInternet as Plain;
        let docker: SocketAddr = "172.17.0.1:5000".parse().unwrap();
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.insert(*name, HeaderValue::from_str(value).unwrap());
            }
            map
        };
        let judge = |pairs: &[(&'static str, &str)]| {
            plain_from_internet(None, docker, &[], &headers(pairs))
        };

        assert!(matches!(judge(&[("host", "203.0.113.5:8080")]), Plain::Yes));
        // A name is looked up only where the answer is used (`createOwner`).
        assert!(matches!(
            judge(&[("host", "read.example.com:8080")]),
            Plain::Named(name) if name == "read.example.com"
        ));
        for home in [
            "localhost:8080",
            "127.0.0.1:8080",
            "192.168.1.20:8080",
            "100.64.0.9:8080",
            "komiq.local:8080",
            "server:8080",
        ] {
            assert!(matches!(judge(&[("host", home)]), Plain::No), "{home}");
        }
        // HTTPS ended in front of the server: a proxy says so, or the browser's page does.
        assert!(matches!(
            judge(&[("host", "203.0.113.5"), ("x-forwarded-proto", "https")]),
            Plain::No
        ));
        assert!(matches!(
            judge(&[
                ("host", "read.example.com"),
                ("origin", "https://read.example.com")
            ]),
            Plain::No
        ));
        // The desktop app's own origin says nothing about this connection.
        assert!(matches!(
            judge(&[
                ("host", "203.0.113.5:8080"),
                ("origin", "https://tauri.localhost")
            ]),
            Plain::Yes
        ));
        // A trusted proxy and the HTTPS listener are never "in the clear".
        let trusted = [Cidr::parse("172.17.0.0/16").unwrap()];
        let public = headers(&[("host", "203.0.113.5:8080")]);
        assert!(matches!(
            plain_from_internet(None, docker, &trusted, &public),
            Plain::No
        ));
        let tls = Some(axum::Extension(cloudflare::origin_tls::OverTls));
        assert!(matches!(
            plain_from_internet(tls, docker, &[], &public),
            Plain::No
        ));
    }

    /// The lazy `/covers/` fetch follows the MangaDex switch: off, a miss is a plain
    /// uncacheable 404 — no fetch, and no redirect that would move the MangaDex request
    /// into the browser; on, the cover is fetched, stored and served.
    #[tokio::test]
    async fn lazy_cover_fetch_follows_the_mangadex_switch() {
        use crate::database::TestPoolOptions;
        use std::sync::atomic::Ordering;
        let main = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations")
            .run(main.write())
            .await
            .unwrap();
        let covers = TestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for ddl in [
            "CREATE TABLE work_cover_blob (work_id TEXT PRIMARY KEY, webp BLOB NOT NULL, version INTEGER NOT NULL, updated_at TEXT NOT NULL)",
            "CREATE TABLE cover_variant (asset_key TEXT NOT NULL, width INTEGER NOT NULL, webp BLOB NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (asset_key, width))",
        ] {
            sqlx::query(ddl).execute(&covers).await.unwrap();
        }
        sqlx::query("INSERT INTO work (id, cover_file_name, created_at, updated_at) VALUES ('w1', 'c.jpg', 't', 't')")
            .execute(&main).await.unwrap();
        sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_key, created_at) VALUES ('ss1', 'w1', 'mangadex', 'md-1', 't')")
            .execute(&main).await.unwrap();
        // Stands in for the MangaDex cover CDN.
        let cdn = crate::suwayomi::testsrv::spawn(
            crate::suwayomi::testsrv::jpeg(300, 450),
            std::time::Duration::ZERO,
        )
        .await;
        let mangadex = Arc::new(
            mangadex::MangaDexClient::new("lazy-cover-test", 50.0, 600.0)
                .with_bases(&cdn.base_url, &cdn.base_url),
        );
        let app = Arc::new(AppState {
            pool: main.clone(),
            cover_pool: covers.clone(),
            suwayomi: SuwayomiClient::new("http://127.0.0.1:1".into(), None, None),
            mangadex: mangadex.clone(),
            admin_users: vec![],
            scan_health: std::sync::Mutex::new(ScanHealth::default()),
            auth_limiter: RateLimiter::new(100, 60),
            comment_limiter: graphql::comment_limiter(),
            password_work: Arc::new(tokio::sync::Semaphore::new(4)),
            federated_limiter: RateLimiter::new(100, 60),
            session_ttl_secs: 3600,
            series_inflight: KeyedLocks::default(),
            chapters_inflight: KeyedLocks::default(),
            cover_crawl_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            catalogue_cover_phash: false,
            ext_icons_dir: std::path::PathBuf::new(),
            setup_code: instance::SetupCode::default(),
        });
        let get = || {
            serve_cover(
                State(app.clone()),
                State(CoverDb(covers.clone())),
                UrlPath("w1.webp".to_string()),
                RawQuery(None),
            )
        };

        let res = get().await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            cdn.hits.load(Ordering::SeqCst),
            0,
            "no MangaDex fetch while off"
        );

        mangadex.set_enabled(true);
        let held_main_writer = main.begin().await.unwrap();
        let held_cover_writer = covers.begin().await.unwrap();
        let res = tokio::time::timeout(std::time::Duration::from_secs(2), get())
            .await
            .expect("cover GET waited for a cache writer");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(cdn.hits.load(Ordering::SeqCst), 1);
        held_cover_writer.commit().await.unwrap();
        held_main_writer.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_cover_blob")
                    .fetch_one(&covers)
                    .await
                    .unwrap();
                if stored > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("deferred cover cache did not persist");
        // Stored: served from our origin from now on, whatever the switch says.
        mangadex.set_enabled(false);
        assert_eq!(get().await.status(), StatusCode::OK);
        assert_eq!(cdn.hits.load(Ordering::SeqCst), 1);
    }

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

    /// The reader's edge SSR reaches this endpoint through SvelteKit's `load({ fetch })`,
    /// which simulates CORS server-side and accepts only `*` or the page's exact origin.
    /// A Worker subrequest carries the WORKER's origin, not the page's, so the allowlist
    /// echo is a value that check rejects — and every public page would then SSR to empty
    /// feeds behind a 60 s edge cache.
    #[test]
    fn an_anonymous_get_is_readable_from_any_origin() {
        let mut h = HeaderMap::new();
        allow_any_origin(&mut h);
        assert_eq!(
            h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
    }

    /// It REPLACES the allowlist echo rather than deferring to it — the echo is exactly the
    /// value that fails the check above, so leaving it in place would be the bug.
    #[test]
    fn the_allowlist_echo_is_replaced_not_kept() {
        let mut h = hdrs(&[("access-control-allow-origin", "http://komiq.cc")]);
        allow_any_origin(&mut h);
        assert_eq!(
            h.get_all(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .iter()
                .count(),
            1,
            "two ACAO headers is the same as none — every browser rejects it"
        );
        assert_eq!(
            h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
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

    /// GET /graphql accepts only query operations — mutations must never be
    /// reachable through the cacheable/replayable endpoint.
    #[test]
    fn no_cache_responses_are_never_edge_cacheable() {
        let ok = async_graphql::Response::new(async_graphql::Value::Null);
        assert!(get_response_cacheable(true, &ok));
        assert!(
            !get_response_cacheable(false, &ok),
            "signed-in reads stay private"
        );
        let mut no_cache = async_graphql::Response::new(async_graphql::Value::Null);
        no_cache.cache_control.max_age = -1;
        assert!(!get_response_cacheable(true, &no_cache));
    }

    #[test]
    fn get_graphql_rejects_non_query_operations() {
        assert!(get_document_is_query_only("query Q { health }"));
        assert!(get_document_is_query_only("{ health }")); // anonymous shorthand
        assert!(get_document_is_query_only(
            "fragment F on Series { id } query Q { discovery { key } }"
        ));
        assert!(!get_document_is_query_only(
            "mutation M { recordView(seriesId: \"1\") }"
        ));
        assert!(!get_document_is_query_only(
            // Multi-operation documents: ONE mutation poisons the whole document.
            "query A { health } mutation B { logout }"
        ));
        // Unparseable → allowed through, so execution returns the real parse error.
        assert!(get_document_is_query_only("not graphql at all {"));
    }

    #[test]
    fn graphql_get_etag_is_stable_and_if_none_match_understands_lists() {
        let a = weak_etag(b"body-one");
        assert_eq!(a, weak_etag(b"body-one"), "same body, same validator");
        assert_ne!(a, weak_etag(b"body-two"));
        assert!(a.starts_with("W/\""), "weak validator form: {a}");
        assert!(if_none_match_matches(Some(&a), &a));
        assert!(if_none_match_matches(
            Some(&format!("\"zzz\", {a}, \"yyy\"")),
            &a
        ));
        assert!(if_none_match_matches(Some("*"), &a));
        assert!(!if_none_match_matches(Some("W/\"deadbeef\""), &a));
        assert!(!if_none_match_matches(None, &a));
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
    fn trusted_headers_require_canonical_ip_addresses() {
        let peer: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let trusted = vec![Cidr::parse("127.0.0.1").unwrap()];
        let a = hdrs(&[("cf-connecting-ip", "2001:0db8:0:0:0:0:0:1")]);
        let b = hdrs(&[("cf-connecting-ip", "2001:db8::1")]);
        assert_eq!(
            resolve_client_ip(&a, peer, &trusted),
            resolve_client_ip(&b, peer, &trusted)
        );
        let invalid = hdrs(&[
            ("cf-connecting-ip", "arbitrary-bucket"),
            ("x-real-ip", "::ffff:192.0.2.1"),
        ]);
        assert_eq!(resolve_client_ip(&invalid, peer, &trusted), "192.0.2.1");
        let invalid = hdrs(&[("x-forwarded-for", "client, forged-bucket")]);
        assert_eq!(resolve_client_ip(&invalid, peer, &trusted), "127.0.0.1");
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
