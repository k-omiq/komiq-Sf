//! Optional static hosting of the built web apps for self-hosted installs
//! (`KOMIKA_WEB_DIR`). The folder holds two static SPA builds:
//!
//! - `reader/` served at `/`
//! - `admin/` served at `/admin`
//!
//! Each falls back to its own `index.html` for client-side routes, while missing
//! `/_app/` assets return 404 without caching. Everything is mounted BELOW the API: the
//! admin build is nested under `/admin` (static route segments such as
//! `/admin/cover/{work_id}` still match first) and the reader is the router's
//! fallback, which only runs when no API route matched.
//!
//! Every HTML answer says `Cache-Control: no-cache` and carries a CSP computed
//! from that app's inline bootstrap scripts, refreshed when index.html changes.

use std::io::Read;
use std::path::{Path, PathBuf};

use axum::body::{Body, Bytes};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use axum::http::{HeaderValue, Method, Response, StatusCode};
use axum::routing::get;
use axum::Router;
use base64::Engine;
use sha2::{Digest, Sha256};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

fn is_html(res: &Response<Body>) -> bool {
    res.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"))
}

/// Revalidate pages, never cache missing assets, and leave other files alone.
fn cache_control(res: &Response<Body>) -> Option<HeaderValue> {
    if res.status() == StatusCode::NOT_FOUND {
        Some(HeaderValue::from_static("no-store"))
    } else {
        is_html(res).then(|| HeaderValue::from_static("no-cache"))
    }
}

/// Match deploy/static-csp.mjs: hash exact, nonempty inline script bodies, skip
/// scripts with src, and include each hash once. Scan ASCII tags without changing
/// the original UTF-8 script bytes that the browser hashes.
fn content_security_policy(html: &str) -> HeaderValue {
    let lower = html.to_ascii_lowercase();
    let mut rest = lower.as_str();
    let mut hashes = Vec::new();
    while let Some(start) = rest.find("<script") {
        rest = &rest[start + 7..];
        if rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let Some((attributes, body)) = rest.split_once('>') else {
            break;
        };
        rest = body;
        let Some((end, close_end)) = rest.match_indices("</script").find_map(|(end, _)| {
            let tail = &rest[end + 8..];
            let whitespace = tail.len() - tail.trim_start().len();
            tail[whitespace..]
                .starts_with('>')
                .then_some((end, end + 8 + whitespace + 1))
        }) else {
            break;
        };
        let start = html.len() - rest.len();
        let script = &html[start..start + end];
        let has_src = attributes.match_indices("src").any(|(i, _)| {
            (i == 0 || attributes[..i].ends_with(char::is_whitespace))
                && attributes[i + 3..].trim_start().starts_with('=')
        });
        if !has_src && !script.trim().is_empty() {
            let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(script));
            let source = format!("'sha256-{hash}'");
            if !hashes.contains(&source) {
                hashes.push(source);
            }
        }
        rest = &rest[close_end..];
    }

    // Cloudflare Turnstile (`crate::turnstile`): the widget script and its frame, used
    // only when the server the apps talk to sends a site key.
    let mut policy =
        "default-src 'self'; script-src 'self' https://challenges.cloudflare.com".to_string();
    for hash in hashes {
        policy.push(' ');
        policy.push_str(&hash);
    }
    // Like the hosted policy, with HTTP(S) destinations for servers selected at
    // runtime, including LAN servers, and blob/data images used by the reader.
    policy.push_str("; style-src 'self' 'unsafe-inline'; img-src 'self' https: http: data: blob:; connect-src 'self' https: http:; font-src 'self' data:; frame-src https://challenges.cloudflare.com; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'");
    HeaderValue::from_str(&policy).expect("CSP contains only fixed directives and base64 hashes")
}

#[derive(PartialEq)]
struct IndexStamp {
    modified: std::time::SystemTime,
    size: u64,
}

#[derive(Clone)]
struct IndexDocument {
    html: Bytes,
    policy: HeaderValue,
}

impl IndexDocument {
    fn response(self, method: Method) -> Response<Body> {
        Response::builder()
            .header(CONTENT_TYPE, "text/html; charset=utf-8")
            .header(CONTENT_LENGTH, self.html.len())
            .header(CONTENT_SECURITY_POLICY, self.policy)
            .body(if method == Method::HEAD {
                Body::empty()
            } else {
                Body::from(self.html)
            })
            .unwrap()
    }
}

struct CachedIndex {
    index: PathBuf,
    value: std::sync::Mutex<Option<(IndexStamp, IndexDocument)>>,
}

impl CachedIndex {
    fn get(&self) -> std::io::Result<IndexDocument> {
        let mut cached = self.value.lock().unwrap_or_else(|p| p.into_inner());
        let read = || -> std::io::Result<(IndexStamp, Option<IndexDocument>)> {
            let metadata = std::fs::metadata(&self.index)?;
            let stamp = IndexStamp {
                modified: metadata.modified()?,
                size: metadata.len(),
            };
            if cached
                .as_ref()
                .is_some_and(|(previous, _)| *previous == stamp)
            {
                return Ok((stamp, None));
            }
            // Read metadata and bytes from the same open file. An atomic deploy
            // can replace the path at any point without splitting HTML from CSP.
            let mut file = std::fs::File::open(&self.index)?;
            let metadata = file.metadata()?;
            let stamp = IndexStamp {
                modified: metadata.modified()?,
                size: metadata.len(),
            };
            let mut html = String::new();
            file.read_to_string(&mut html)?;
            let document = IndexDocument {
                policy: content_security_policy(&html),
                html: Bytes::from(html),
            };
            Ok((stamp, Some(document)))
        };
        match read() {
            Ok((stamp, Some(document))) => *cached = Some((stamp, document)),
            Ok((_, None)) => {}
            Err(error) => {
                tracing::warn!(path = %self.index.display(), %error,
                    "cannot read app index.html");
                *cached = None;
                return Err(error);
            }
        }
        Ok(cached.as_ref().unwrap().1.clone())
    }

    fn response(&self, method: Method) -> Response<Body> {
        match self.get() {
            Ok(document) => document.response(method),
            Err(error) => Response::builder()
                .status(if error.kind() == std::io::ErrorKind::NotFound {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                })
                .header(CACHE_CONTROL, "no-store")
                .body(Body::empty())
                .unwrap(),
        }
    }
}

/// Real files as-is; missing assets are 404, other paths fall back to index.html.
fn spa(root: PathBuf) -> Router {
    let index = std::sync::Arc::new(CachedIndex {
        index: root.join("index.html"),
        value: std::sync::Mutex::new(None),
    });
    let _ = index.get();
    let page = get(move |method: Method| {
        let index = index.clone();
        async move { index.response(method) }
    });
    Router::new()
        .route("/", page.clone())
        .route("/index.html", page.clone())
        .nest_service("/_app", ServeDir::new(root.join("_app")))
        .fallback_service(ServeDir::new(root).fallback(page))
        .layer(SetResponseHeaderLayer::overriding(
            CACHE_CONTROL,
            cache_control,
        ))
}

/// Choose which built web apps accompany the public API.
pub enum Apps {
    None,
    ReaderAndAdmin(PathBuf),
    ReaderOnly(PathBuf),
}

impl Apps {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        match &cfg.web_dir {
            Some(dir) => Self::ReaderAndAdmin(dir.into()),
            None => Self::None,
        }
    }
}

/// Mount the reader and admin builds from `dir` under the already-routed `app`.
pub fn mount<S>(app: Router<S>, dir: &Path) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    app.nest_service("/admin", spa(dir.join("admin")))
        .fallback_service(spa(dir.join("reader")))
}

/// Mount only the embedded reader; there is no admin web-app nest.
pub fn mount_reader<S>(app: Router<S>, reader_dir: &Path) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    app.fallback_service(spa(reader_dir.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{get, post};
    use tower::ServiceExt;

    fn fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("komika-web-{}", uuid::Uuid::new_v4()));
        for (path, body) in [
            ("reader/index.html", "reader-index"),
            ("reader/app.js", "reader-js"),
            ("reader/_app/reader.js", "reader-chunk"),
            ("admin/index.html", "admin-index"),
            ("admin/_app/admin.js", "admin-js"),
        ] {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    fn app(dir: &Path) -> Router {
        let api = Router::new()
            .route("/graphql", get(|| async { "api-graphql" }))
            .route("/health", get(|| async { "ok" }))
            .route("/admin/cover/{work_id}", post(|| async { "api-cover" }));
        mount(api, dir)
    }

    async fn response(app: &Router, method: &str, uri: &str) -> Response<Body> {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn get_body(app: &Router, method: &str, uri: &str) -> (StatusCode, String) {
        let res = response(app, method, uri).await;
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn serves_both_apps_with_spa_fallback_and_api_first() {
        let dir = fixture();
        let app = app(&dir);

        // Reader at the root, files and client routes.
        assert_eq!(
            get_body(&app, "GET", "/").await,
            (StatusCode::OK, "reader-index".into())
        );
        assert_eq!(
            get_body(&app, "GET", "/app.js").await,
            (StatusCode::OK, "reader-js".into())
        );
        assert_eq!(
            get_body(&app, "GET", "/_app/reader.js").await,
            (StatusCode::OK, "reader-chunk".into())
        );
        assert_eq!(
            get_body(&app, "GET", "/series/42").await,
            (StatusCode::OK, "reader-index".into())
        );

        // Admin under /admin, files and client routes.
        for uri in ["/admin", "/admin/", "/admin/users/7"] {
            assert_eq!(
                get_body(&app, "GET", uri).await,
                (StatusCode::OK, "admin-index".into()),
                "{uri}"
            );
        }
        assert_eq!(
            get_body(&app, "GET", "/admin/_app/admin.js").await,
            (StatusCode::OK, "admin-js".into())
        );

        // API routes always win over static files.
        assert_eq!(get_body(&app, "GET", "/graphql").await.1, "api-graphql");
        assert_eq!(get_body(&app, "GET", "/health").await.1, "ok");
        assert_eq!(
            get_body(&app, "POST", "/admin/cover/w1").await.1,
            "api-cover"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn pages_are_never_reused_without_asking() {
        let dir = fixture();
        // The fixture's files have no extension the server maps to HTML except these.
        std::fs::write(dir.join("admin/index.html"), "<!doctype html>").unwrap();
        let app = app(&dir);
        let cache = |uri: &'static str| {
            let app = app.clone();
            async move {
                let res = app
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                res.headers()
                    .get("cache-control")
                    .map(|v| v.to_str().unwrap().to_string())
            }
        };
        // The page itself, a client route that falls back to it, and the reader's.
        for uri in ["/admin/", "/admin/setup", "/", "/series/42"] {
            assert_eq!(cache(uri).await.as_deref(), Some("no-cache"), "{uri}");
        }
        // Hashed files keep the browser's own handling.
        assert_eq!(cache("/admin/_app/admin.js").await, None);
        assert_eq!(cache("/app.js").await, None);

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn missing_app_assets_are_not_spa_pages_or_cached() {
        let dir = fixture();
        let app = app(&dir);
        for uri in [
            "/_app/missing.js",
            "/_app/immutable/chunks/old.js?v=1",
            "/admin/_app/missing.js",
            "/admin/_app/immutable/chunks/old.js?v=1",
        ] {
            for method in ["GET", "HEAD"] {
                let res = response(&app, method, uri).await;
                assert_eq!(res.status(), StatusCode::NOT_FOUND, "{method} {uri}");
                assert_eq!(res.headers()[CACHE_CONTROL], "no-store", "{method} {uri}");
                assert!(
                    !res.headers().contains_key(CONTENT_SECURITY_POLICY),
                    "{uri}"
                );
                let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                    .await
                    .unwrap();
                assert!(body.is_empty(), "{method} {uri}");
            }
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn html_has_its_own_bootstrap_csp_and_files_do_not() {
        let dir = fixture();
        for (sub, script) in [
            ("reader", "\nwindow.reader = true;\n"),
            ("admin", "\nwindow.admin = true;\n"),
        ] {
            std::fs::write(
                dir.join(sub).join("index.html"),
                format!("<!doctype html><script src=\"/_app/app.js\"></script><script>{script}</script>"),
            )
            .unwrap();
        }
        let app = app(&dir);
        for (uris, hash) in [
            (
                ["/", "/index.html", "/series/42"],
                "U/erItL3lzMKTlO+MZ+Up/5YC1NeejrqFRov3UWJYdI=",
            ),
            (
                ["/admin/", "/admin/index.html", "/admin/users/7"],
                "9lvHtpNOfmB/+/O9+eRclDWUHv5FBHHfhNOJBO+dc1k=",
            ),
        ] {
            for uri in uris {
                let res = response(&app, "GET", uri).await;
                assert_eq!(res.status(), StatusCode::OK, "{uri}");
                assert_eq!(res.headers()[CACHE_CONTROL], "no-cache", "{uri}");
                let policy = res.headers()[CONTENT_SECURITY_POLICY].to_str().unwrap();
                assert!(
                    policy.contains(&format!(
                        "script-src 'self' https://challenges.cloudflare.com 'sha256-{hash}';"
                    )),
                    "{uri}: {policy}"
                );
                for directive in [
                    "default-src 'self'",
                    "style-src 'self' 'unsafe-inline'",
                    "img-src 'self' https: http: data: blob:",
                    "connect-src 'self' https: http:",
                    "font-src 'self' data:",
                    "frame-src https://challenges.cloudflare.com",
                    "object-src 'none'",
                    "base-uri 'self'",
                    "form-action 'self'",
                    "frame-ancestors 'none'",
                ] {
                    assert!(
                        policy.split(';').any(|part| part.trim() == directive),
                        "{uri}: {directive}"
                    );
                }
            }
        }
        for uri in [
            "/app.js",
            "/_app/reader.js",
            "/admin/_app/admin.js",
            "/health",
        ] {
            let res = response(&app, "GET", uri).await;
            assert_eq!(res.status(), StatusCode::OK, "{uri}");
            assert!(
                !res.headers().contains_key(CONTENT_SECURITY_POLICY),
                "{uri}"
            );
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn index_appearing_after_mount_gets_its_bootstrap_csp() {
        let dir = fixture();
        std::fs::remove_file(dir.join("reader/index.html")).unwrap();
        let app = app(&dir);
        // A web build can appear after mount, with no server restart.
        std::fs::write(dir.join("reader/index.html"), "<script>boot()</script>").unwrap();
        let res = response(&app, "GET", "/").await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[CACHE_CONTROL], "no-cache");
        assert_eq!(
            res.headers()[CONTENT_SECURITY_POLICY],
            content_security_policy("<script>boot()</script>")
        );
        assert!(response(&app, "GET", "/admin/")
            .await
            .headers()
            .contains_key(CONTENT_SECURITY_POLICY));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn html_and_csp_stay_paired_across_atomic_index_replacement() {
        let dir = fixture();
        let old_html = "<script>old()</script>";
        let new_html = "<script>new()</script>";
        assert_eq!(old_html.len(), new_html.len());
        for sub in ["reader", "admin"] {
            std::fs::write(dir.join(sub).join("index.html"), old_html).unwrap();
        }
        let app = app(&dir); // Warm each app's cached snapshot before deployment.
        let pending = response(&app, "GET", "/").await;
        let mut times = Vec::new();
        for sub in ["reader", "admin"] {
            let path = dir.join(sub).join("index.html");
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            let replacement = path.with_extension("next");
            std::fs::write(&replacement, new_html).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&replacement)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
            std::fs::rename(replacement, &path).unwrap();
            times.push((path, modified));
        }
        // Keeping the stamp unchanged deterministically reproduces the old
        // split-read bug: ServeFile read new HTML but the CSP cache stayed old.
        // A cached pair may stay old, but must never mix the two deployments.
        for html in [old_html, new_html] {
            for uri in [
                "/",
                "/index.html",
                "/series/42",
                "/admin/",
                "/admin/index.html",
                "/admin/users/7",
            ] {
                for method in ["GET", "HEAD"] {
                    let res = response(&app, method, uri).await;
                    assert_eq!(res.status(), StatusCode::OK, "{method} {uri}");
                    assert_eq!(res.headers()[CACHE_CONTROL], "no-cache");
                    assert_eq!(
                        res.headers()[CONTENT_SECURITY_POLICY],
                        content_security_policy(html)
                    );
                    assert_eq!(res.headers()[CONTENT_LENGTH], html.len().to_string());
                    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                        .await
                        .unwrap();
                    assert_eq!(
                        body.as_ref(),
                        if method == "HEAD" {
                            b""
                        } else {
                            html.as_bytes()
                        },
                        "{method} {uri}"
                    );
                }
            }
            // A changed stamp refreshes bytes and policy together for both apps.
            for (path, modified) in &times {
                std::fs::File::options()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_times(
                        std::fs::FileTimes::new()
                            .set_modified(*modified + std::time::Duration::from_secs(10)),
                    )
                    .unwrap();
            }
        }
        // A response already returned before deployment retains its own pair,
        // even after other requests have refreshed the shared cache.
        assert_eq!(
            pending.headers()[CONTENT_SECURITY_POLICY],
            content_security_policy(old_html)
        );
        assert_eq!(
            axum::body::to_bytes(pending.into_body(), 1 << 20)
                .await
                .unwrap()
                .as_ref(),
            old_html.as_bytes()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn html_csp_refreshes_when_index_size_or_modified_time_changes() {
        let dir = fixture();
        let app = app(&dir);
        for (sub, uri) in [("reader", "/series/42"), ("admin", "/admin/users/7")] {
            let index = dir.join(sub).join("index.html");
            let initial =
                response(&app, "GET", uri).await.headers()[CONTENT_SECURITY_POLICY].clone();
            let original_time = std::fs::metadata(&index).unwrap().modified().unwrap();
            for (html, modified) in [
                ("<script>one()</script>", original_time), // Size changes, mtime stays fixed.
                (
                    "<script>two()</script>",
                    original_time + std::time::Duration::from_secs(10),
                ), // Same size, new mtime.
            ] {
                std::fs::write(&index, html).unwrap();
                std::fs::File::options()
                    .write(true)
                    .open(&index)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(modified))
                    .unwrap();
                for method in ["GET", "HEAD"] {
                    let res = response(&app, method, uri).await;
                    assert_eq!(
                        res.headers()[CONTENT_SECURITY_POLICY],
                        content_security_policy(html),
                        "{method} {uri}"
                    );
                    assert_ne!(res.headers()[CONTENT_SECURITY_POLICY], initial);
                    assert_eq!(res.headers()[CACHE_CONTROL], "no-cache");
                    assert_eq!(res.headers()[CONTENT_LENGTH], html.len().to_string());
                    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
                        .await
                        .unwrap();
                    assert_eq!(
                        body.as_ref(),
                        if method == "HEAD" {
                            b""
                        } else {
                            html.as_bytes()
                        }
                    );
                }
            }
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn csp_hashes_inline_scripts_verbatim_like_static_deployment() {
        let policy = content_security_policy(
            "<scripture>ignored</scripture>\
             <SCRIPT SRC \n=\"/app.js\">ignored</SCRIPT>\
             <script> \n\t </script><script></script>\
             <ScRiPt type=\"module\">\nwindow.boot = \"読者\";\n</sCrIpT \n>\
             <script>\nwindow.boot = \"読者\";\n</script>\
             <script data-src=\"inline\">boot()</script>",
        );
        let script_src = policy.to_str().unwrap().split(';').nth(1).unwrap().trim();
        assert_eq!(
            script_src,
            "script-src 'self' https://challenges.cloudflare.com 'sha256-+AdlzVIaxZ7zO7M+D5zeromszQ+2o0eZ6bjYMOFOaHI=' 'sha256-MeZS89WlF0u+o0hCvHTBt4q1WHU+U+sJKgbdRUc36mY='"
        );
    }
}
