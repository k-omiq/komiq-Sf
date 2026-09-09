//! `GET /img?src=<url>` — the server's own image proxy (self-host).
//!
//! WHY. The hosted reader loads MangaDex covers and pages through the `img.komiq.cc`
//! Cloudflare Worker (`apps/worker/src/index.ts`): MangaDex answers hotlinks with a
//! wrong image and its CDNs send no CORS headers, so the browser cannot load them
//! directly. A self-hosted server has no Worker, so it serves the same URL scheme
//! itself and the reader points its proxy base here (`PUBLIC_KOMIKA_IMG_MODE=server`).
//!
//! NOT AN OPEN PROXY. This is the Worker's rule set, ported rule for rule, because a
//! proxy on our own origin is a far better SSRF/laundering primitive than one on a
//! Worker:
//!
//! - **Host allowlist** ([`ALLOWED_SOURCE_HOSTS`]) — the Worker's `ALLOWED_SOURCE_HOSTS`
//!   verbatim (a test pins the two together). Exact host or a dot-suffix. Nothing else
//!   is ever fetched, whatever the URL.
//! - **Plain http(s) on the default port, no credentials** — so `src` cannot aim at a
//!   side service on an allowlisted name.
//! - **Redirects followed by hand**, at most [`MAX_REDIRECTS`], with the allowlist and
//!   URL rules re-checked on every hop. `reqwest`'s automatic follow would let an
//!   allowlisted host bounce us anywhere.
//! - **Raster images only** ([`is_image_content_type`]); SVG is refused (it is an
//!   active document). An upstream HTML error page is never re-served as ours.
//! - **Size cap** ([`MAX_IMAGE_BYTES`]) on the declared length AND while reading, and a
//!   hard request timeout.
//! - **MangaDex hosts follow the MangaDex switch**: with MangaDex turned off, a
//!   `*.mangadex.*` source is refused here as well, so the proxy cannot become the
//!   path around the switch.
//!
//! Successes carry the Worker's year-long-ish immutable cache (7 days) so a CDN in
//! front of the server (the plan's Cloudflare cache rule for `/img`) and the browser
//! absorb repeat loads; every refusal is `no-store`, so a transient upstream failure is
//! never pinned in a cache.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use reqwest::Url;

use crate::mangadex::MangaDexClient;

/// The upstream hosts `/img` may fetch from. MUST equal the Worker's
/// `ALLOWED_SOURCE_HOSTS` in `apps/worker/wrangler.toml` (pinned by
/// `allowlist_matches_the_worker`): MangaDex covers (`uploads.mangadex.org`), the
/// MangaDex@Home page nodes (`*.mangadex.network`), and the hosted API origin whose
/// Suwayomi page URLs the hosted reader also routes through the proxy.
pub const ALLOWED_SOURCE_HOSTS: &[&str] =
    &["uploads.mangadex.org", "mangadex.network", "api.komiq.cc"];

/// Same ceiling as the Worker (`MAX_IMAGE_BYTES`) and the native reader.
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// Same as the Worker. Each hop is re-validated.
pub const MAX_REDIRECTS: usize = 4;

/// Whole-request budget per hop, as the Worker's `AbortSignal.timeout(15_000)`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Upstream fetches in flight at once. Each one buffers up to [`MAX_IMAGE_BYTES`]
/// before it answers (the cap must be enforced before a byte is served), so this
/// bounds the proxy's worst-case memory at 16 × 32 MiB. A reader opening a chapter
/// asks for a page window, not the whole chapter, so 16 does not throttle reading;
/// further requests wait for a slot rather than fail.
const MAX_INFLIGHT: usize = 16;

const IMMUTABLE_CACHE: &str = "public, max-age=604800, immutable";

/// The proxy's fixed configuration. Production uses [`ImgProxy::new`]; tests swap the
/// allowlist and allow a port so a local mock can stand in for MangaDex.
pub struct ImgProxy {
    http: reqwest::Client,
    hosts: Vec<String>,
    /// Production: false (default port only, as the Worker). Tests: true, because a
    /// mock server cannot listen on 80.
    allow_ports: bool,
    max_bytes: usize,
    inflight: tokio::sync::Semaphore,
    /// For the MangaDex switch (see the module doc).
    mangadex: Arc<MangaDexClient>,
}

impl ImgProxy {
    pub fn new(mangadex: Arc<MangaDexClient>) -> Self {
        Self::with(
            http_client(),
            ALLOWED_SOURCE_HOSTS.iter().map(|h| h.to_string()).collect(),
            false,
            MAX_IMAGE_BYTES,
            mangadex,
        )
    }

    fn with(
        http: reqwest::Client,
        hosts: Vec<String>,
        allow_ports: bool,
        max_bytes: usize,
        mangadex: Arc<MangaDexClient>,
    ) -> Self {
        Self {
            http,
            hosts,
            allow_ports,
            max_bytes,
            inflight: tokio::sync::Semaphore::new(MAX_INFLIGHT),
            mangadex,
        }
    }

    /// Exact host or a dot-suffix of an allowlisted one. An empty list allows nothing.
    fn host_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.hosts
            .iter()
            .any(|a| host == *a || host.ends_with(&format!(".{a}")))
    }

    /// The Worker's `safeUpstreamUrl` plus the allowlist and the MangaDex switch, in
    /// one check that every hop goes through.
    fn check(&self, url: &Url) -> Result<(), Refusal> {
        let scheme_ok = matches!(url.scheme(), "http" | "https");
        let creds = !url.username().is_empty() || url.password().is_some();
        // `Url::port()` is None for the scheme's default port, as the Worker's `port === ''`.
        let port_ok = self.allow_ports || url.port().is_none();
        if !scheme_ok || creds || !port_ok {
            return Err(Refusal::bad(
                "src must be http(s) without credentials or nonstandard ports",
            ));
        }
        let Some(host) = url.host_str() else {
            return Err(Refusal::bad("src has no host"));
        };
        if !self.host_allowed(host) {
            return Err(Refusal::bad("src host not allowed"));
        }
        if is_mangadex_host(host) && !self.mangadex.is_enabled() {
            return Err(Refusal {
                status: StatusCode::FORBIDDEN,
                message: crate::mangadex::MANGADEX_OFF_MESSAGE,
            });
        }
        Ok(())
    }

    /// Fetch `start`, following redirects by hand with every hop re-checked, then
    /// apply the content-type and size rules. Returns the content type and bytes.
    async fn fetch(&self, start: Url) -> Result<(String, Vec<u8>), Refusal> {
        let _permit = self
            .inflight
            .acquire()
            .await
            .map_err(|_| Refusal::upstream("proxy is shutting down"))?;
        let mut current = start;
        for _ in 0..=MAX_REDIRECTS {
            let referer = format!("{}://{}/", current.scheme(), current.authority());
            let res = self
                .http
                .get(current.clone())
                .header(header::REFERER, referer)
                .header(header::ACCEPT, "image/*,*/*;q=0.8")
                .send()
                .await
                .map_err(|_| Refusal::upstream("Upstream fetch failed"))?;
            if res.status().is_redirection() {
                let next = res
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or(Refusal::upstream(
                        "Upstream redirect without a Location header",
                    ))?;
                // Relative locations resolve against the current hop, as the Worker's
                // `new URL(location, current)`.
                let next = current
                    .join(next)
                    .map_err(|_| Refusal::upstream("Upstream redirect to an invalid Location"))?;
                self.check(&next).map_err(|r| Refusal {
                    message: match r.message {
                        "src host not allowed" => "redirect host not allowed",
                        m => m,
                    },
                    ..r
                })?;
                current = next;
                continue;
            }
            if !res.status().is_success() {
                return Err(Refusal::upstream("Upstream returned an error"));
            }
            let content_type = res
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if !is_image_content_type(&content_type) {
                return Err(Refusal::upstream("Upstream is not an image"));
            }
            let bytes = read_capped(res, self.max_bytes).await?;
            return Ok((content_type, bytes));
        }
        Err(Refusal::upstream("Too many redirects"))
    }
}

/// The proxy's own client: no automatic redirects (we follow by hand), bounded
/// connect and total time. Deliberately not the MangaDex client: that one carries
/// MangaDex's API rate budget, which image bytes must not spend.
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(FETCH_TIMEOUT)
        .user_agent(concat!(
            "komiq-server/",
            env!("CARGO_PKG_VERSION"),
            " (image proxy)"
        ))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// `*.mangadex.org` / `*.mangadex.network` — the hosts the MangaDex switch governs.
fn is_mangadex_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    ["mangadex.org", "mangadex.network"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Raster image types only — the Worker's list. SVG is deliberately absent: it is an
/// active document that could run script under our origin.
pub fn is_image_content_type(content_type: &str) -> bool {
    let t = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        t.as_str(),
        "image/jpeg"
            | "image/png"
            | "image/webp"
            | "image/gif"
            | "image/avif"
            | "image/bmp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
    )
}

/// Read the body, refusing a declared or actual length past `cap`.
async fn read_capped(mut res: reqwest::Response, cap: usize) -> Result<Vec<u8>, Refusal> {
    if res.content_length().is_some_and(|n| n > cap as u64) {
        return Err(Refusal::upstream("Upstream image too large"));
    }
    let mut out = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|_| Refusal::upstream("Upstream fetch failed"))?
    {
        if out.len() + chunk.len() > cap {
            return Err(Refusal::upstream("Upstream image too large"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// A refused request: the status and a short plain-text reason (never the upstream
/// body, which could be anything).
#[derive(Debug)]
struct Refusal {
    status: StatusCode,
    message: &'static str,
}

impl Refusal {
    fn bad(message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }
    fn upstream(message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message,
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (
            self.status,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            self.message,
        )
            .into_response()
    }
}

/// `GET /img?src=…` (HEAD is answered by axum from the same handler, without a body).
pub async fn serve(State(proxy): State<Arc<ImgProxy>>, RawQuery(query): RawQuery) -> Response {
    let src = query.as_deref().and_then(|q| {
        url_form_pairs(q)
            .into_iter()
            .find(|(k, _)| k == "src")
            .map(|(_, v)| v)
    });
    let Some(src) = src.filter(|s| !s.is_empty()) else {
        return Refusal::bad("Missing ?src").into_response();
    };
    let Ok(upstream) = Url::parse(&src) else {
        return Refusal::bad("Invalid ?src URL").into_response();
    };
    if let Err(r) = proxy.check(&upstream) {
        return r.into_response();
    }
    match proxy.fetch(upstream).await {
        Ok((content_type, bytes)) => {
            let ct = HeaderValue::from_str(&content_type)
                .unwrap_or(HeaderValue::from_static("application/octet-stream"));
            (
                [
                    (header::CONTENT_TYPE, ct),
                    (
                        header::CACHE_CONTROL,
                        HeaderValue::from_static(IMMUTABLE_CACHE),
                    ),
                    // Never let a browser reinterpret the bytes, nor offer them as a
                    // download (the Worker's same pair).
                    (
                        header::X_CONTENT_TYPE_OPTIONS,
                        HeaderValue::from_static("nosniff"),
                    ),
                    (
                        header::CONTENT_DISPOSITION,
                        HeaderValue::from_static("inline"),
                    ),
                    // Images are public by construction; `*` lets the reader draw them on
                    // a canvas wherever it is served from, as the Worker allows.
                    (
                        header::ACCESS_CONTROL_ALLOW_ORIGIN,
                        HeaderValue::from_static("*"),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Err(r) => {
            tracing::debug!(status = %r.status, reason = r.message, "img proxy: refused");
            r.into_response()
        }
    }
}

/// `application/x-www-form-urlencoded` query pairs, decoded. `Url` already carries
/// the parser; this avoids a dependency for the one parameter we read.
fn url_form_pairs(query: &str) -> Vec<(String, String)> {
    Url::parse(&format!("http://q.invalid/?{query}"))
        .map(|u| u.query_pairs().into_owned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    /// The allowlist is the Worker's, not a lookalike: read it out of
    /// `wrangler.toml` and compare.
    #[test]
    fn allowlist_matches_the_worker() {
        let toml = include_str!("../../worker/wrangler.toml");
        let line = toml
            .lines()
            .find(|l| l.trim_start().starts_with("ALLOWED_SOURCE_HOSTS"))
            .expect("wrangler.toml declares ALLOWED_SOURCE_HOSTS");
        let value = line.split_once('=').unwrap().1.trim().trim_matches('"');
        let worker: Vec<&str> = value.split(',').map(str::trim).collect();
        assert_eq!(worker, ALLOWED_SOURCE_HOSTS);
    }

    #[test]
    fn content_type_and_host_rules_match_the_worker() {
        assert!(is_image_content_type("image/jpeg"));
        assert!(is_image_content_type("Image/PNG; charset=binary"));
        assert!(!is_image_content_type("image/svg+xml"));
        assert!(!is_image_content_type("text/html"));
        assert!(!is_image_content_type(""));
        assert!(is_mangadex_host("cmdx.mangadex.network"));
        assert!(is_mangadex_host("uploads.mangadex.org"));
        assert!(!is_mangadex_host("mangadex.network.evil.test"));
        assert!(!is_mangadex_host("notmangadex.org"));
    }

    /// A mock upstream: every path the tests need, and a hit counter so a refusal can
    /// be shown to have happened BEFORE any request left the server.
    async fn upstream() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new()
            .route(
                "/ok.jpg",
                get(|| async { ([(header::CONTENT_TYPE, "image/jpeg")], vec![0xFFu8; 64]) }),
            )
            .route(
                "/page.html",
                get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<script>x</script>") }),
            )
            .route(
                "/logo.svg",
                get(|| async { ([(header::CONTENT_TYPE, "image/svg+xml")], "<svg/>") }),
            )
            .route(
                "/big.jpg",
                get(|| async { ([(header::CONTENT_TYPE, "image/jpeg")], vec![0u8; 4096]) }),
            )
            .route(
                "/hop",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/ok.jpg")], "") }),
            )
            .route(
                "/away",
                get(
                    |axum::extract::Query(q): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        (
                            StatusCode::FOUND,
                            [(header::LOCATION, q.get("to").cloned().unwrap_or_default())],
                            "",
                        )
                    },
                ),
            )
            .route(
                "/loop",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/loop")], "") }),
            )
            .layer(axum::middleware::from_fn(
                move |req: Request<Body>, next: axum::middleware::Next| {
                    let h = h.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        next.run(req).await
                    }
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, hits)
    }

    /// `/img` over a proxy whose allowlist is `uploads.mangadex.org` (MangaDex, so the
    /// switch applies) and `allowed.test`, both resolving to the mock; `blocked.test`
    /// resolves there too, so only the allowlist stands between it and a fetch.
    fn proxy_app(addr: std::net::SocketAddr, mangadex_on: bool, max_bytes: usize) -> Router {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .resolve("allowed.test", addr)
            .resolve("uploads.mangadex.org", addr)
            .resolve("blocked.test", addr)
            .build()
            .unwrap();
        let md = Arc::new(MangaDexClient::new("img-test", 5.0, 40.0));
        md.set_enabled(mangadex_on);
        let proxy = ImgProxy::with(
            http,
            vec!["allowed.test".into(), "uploads.mangadex.org".into()],
            true,
            max_bytes,
            md,
        );
        Router::new()
            .route("/img", get(serve))
            .with_state(Arc::new(proxy))
    }

    async fn call(app: &Router, src: &str) -> (StatusCode, String, Option<String>, Vec<u8>) {
        let uri = format!("/img?src={}", encode(src));
        let res = app
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let cache = res
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec();
        (status, ct, cache, body)
    }

    fn encode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn serves_an_allowlisted_image_with_long_cache() {
        let (addr, hits) = upstream().await;
        let app = proxy_app(addr, true, MAX_IMAGE_BYTES);
        let port = addr.port();
        let (status, ct, cache, body) =
            call(&app, &format!("http://allowed.test:{port}/ok.jpg")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ct, "image/jpeg");
        assert_eq!(cache.as_deref(), Some(IMMUTABLE_CACHE));
        assert_eq!(body.len(), 64);
        // A same-host redirect is followed (and re-checked).
        let (status, _, _, body) = call(&app, &format!("http://allowed.test:{port}/hop")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.len(), 64);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn refuses_everything_that_would_make_it_an_open_proxy() {
        let (addr, hits) = upstream().await;
        let app = proxy_app(addr, true, MAX_IMAGE_BYTES);
        let port = addr.port();

        // Refused before any request leaves the server.
        for (src, why) in [
            (
                format!("http://blocked.test:{port}/ok.jpg"),
                "src host not allowed",
            ),
            // A lookalike suffix is not a subdomain.
            (
                format!("http://allowed.test.blocked.test:{port}/ok.jpg"),
                "src host not allowed",
            ),
            (
                format!("http://127.0.0.1:{port}/ok.jpg"),
                "src host not allowed",
            ),
            (
                format!("ftp://allowed.test:{port}/ok.jpg"),
                "src must be http(s) without credentials or nonstandard ports",
            ),
            (
                format!("http://user:pw@allowed.test:{port}/ok.jpg"),
                "src must be http(s) without credentials or nonstandard ports",
            ),
            ("not a url".to_string(), "Invalid ?src URL"),
        ] {
            let (status, _, cache, body) = call(&app, &src).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{src}");
            assert_eq!(String::from_utf8_lossy(&body), why, "{src}");
            assert_eq!(cache.as_deref(), Some("no-store"), "{src}");
        }
        let res = app
            .clone()
            .oneshot(Request::get("/img").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "no refused src may be fetched"
        );

        // A redirect from an allowed host to a disallowed one: the first hop is
        // fetched, the second never is.
        let to = encode(&format!("http://blocked.test:{port}/ok.jpg"));
        let (status, _, _, body) =
            call(&app, &format!("http://allowed.test:{port}/away?to={to}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(String::from_utf8_lossy(&body), "redirect host not allowed");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Endless redirects stop at the limit.
        let before = hits.load(Ordering::SeqCst);
        let (status, _, _, body) = call(&app, &format!("http://allowed.test:{port}/loop")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(String::from_utf8_lossy(&body), "Too many redirects");
        assert_eq!(hits.load(Ordering::SeqCst) - before, MAX_REDIRECTS + 1);

        // Non-images (HTML, and SVG which is an active document) are not re-served.
        for path in ["page.html", "logo.svg"] {
            let (status, ct, cache, body) =
                call(&app, &format!("http://allowed.test:{port}/{path}")).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{path}");
            assert!(ct.starts_with("text/plain"), "{path}: {ct}");
            assert_eq!(cache.as_deref(), Some("no-store"));
            assert_eq!(String::from_utf8_lossy(&body), "Upstream is not an image");
        }
    }

    #[tokio::test]
    async fn refuses_an_oversized_body() {
        let (addr, _) = upstream().await;
        // Cap below the 4 KiB body: refused (declared length) rather than truncated.
        let app = proxy_app(addr, true, 1024);
        let port = addr.port();
        let (status, _, _, body) = call(&app, &format!("http://allowed.test:{port}/big.jpg")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(String::from_utf8_lossy(&body), "Upstream image too large");
        // The 64-byte image still fits.
        let (status, _, _, _) = call(&app, &format!("http://allowed.test:{port}/ok.jpg")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn mangadex_hosts_follow_the_switch() {
        let (addr, hits) = upstream().await;
        let port = addr.port();
        let src = format!("http://uploads.mangadex.org:{port}/ok.jpg");
        let off = proxy_app(addr, false, MAX_IMAGE_BYTES);
        let (status, _, _, body) = call(&off, &src).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            String::from_utf8_lossy(&body),
            crate::mangadex::MANGADEX_OFF_MESSAGE
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        // A non-MangaDex allowlisted host is unaffected by the switch.
        let (status, _, _, _) = call(&off, &format!("http://allowed.test:{port}/ok.jpg")).await;
        assert_eq!(status, StatusCode::OK);

        let on = proxy_app(addr, true, MAX_IMAGE_BYTES);
        let (status, _, _, _) = call(&on, &src).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn production_proxy_refuses_ports() {
        let md = Arc::new(MangaDexClient::new("img-test", 5.0, 40.0));
        md.set_enabled(true);
        let p = ImgProxy::new(md);
        let ok = Url::parse("https://uploads.mangadex.org/covers/x/y.jpg").unwrap();
        assert!(p.check(&ok).is_ok());
        let port = Url::parse("https://uploads.mangadex.org:8443/covers/x/y.jpg").unwrap();
        assert!(p.check(&port).is_err());
        let node = Url::parse("https://abc.mangadex.network/data/h/1.png").unwrap();
        assert!(p.check(&node).is_ok());
        let local = Url::parse("http://localhost/x.png").unwrap();
        assert!(p.check(&local).is_err());
    }
}
