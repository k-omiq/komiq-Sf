//! Optional static hosting of the built web apps for self-hosted installs
//! (`KOMIKA_WEB_DIR`). The folder holds two static SPA builds:
//!
//! - `reader/` served at `/`
//! - `admin/` served at `/admin`
//!
//! Each falls back to its own `index.html` for any path that is not a file, so
//! client-side routes survive a reload. Everything is mounted BELOW the API: the
//! admin build is nested under `/admin` (static route segments such as
//! `/admin/cover/{work_id}` still match first) and the reader is the router's
//! fallback, which only runs when no API route matched.
//!
//! Every HTML answer says `Cache-Control: no-cache`. The page names its scripts by
//! content hash, so a browser that reuses an old page after an upgrade asks for
//! scripts that are gone, gets the page again in their place, and shows nothing.

use std::path::{Path, PathBuf};

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, Response};
use axum::Router;
use tower_http::services::fs::ServeFileSystemResponseBody;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeader;

type HtmlNoCache = fn(&Response<ServeFileSystemResponseBody>) -> Option<HeaderValue>;

/// `no-cache` for a page (always checked with the server before reuse); nothing for
/// any other file.
fn html_no_cache(res: &Response<ServeFileSystemResponseBody>) -> Option<HeaderValue> {
    res.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"))
        .then(|| HeaderValue::from_static("no-cache"))
}

/// `root` as an SPA: real files as-is, anything else answers `index.html` with
/// 200 (`fallback`, not `not_found_service`, which would force a 404).
fn spa(root: PathBuf) -> SetResponseHeader<ServeDir<ServeFile>, HtmlNoCache> {
    let index = root.join("index.html");
    SetResponseHeader::overriding(
        ServeDir::new(root).fallback(ServeFile::new(index)),
        CACHE_CONTROL,
        html_no_cache as HtmlNoCache,
    )
}

/// Mount the reader and admin builds from `dir` under the already-routed `app`.
pub fn mount<S>(app: Router<S>, dir: &Path) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    for sub in ["reader", "admin"] {
        if !dir.join(sub).join("index.html").is_file() {
            tracing::warn!(
                dir = %dir.display(),
                "KOMIKA_WEB_DIR has no {sub}/index.html; that app will not load"
            );
        }
    }
    app.nest_service("/admin", spa(dir.join("admin")))
        .fallback_service(spa(dir.join("reader")))
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

    async fn get_body(app: &Router, method: &str, uri: &str) -> (StatusCode, String) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
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
}
