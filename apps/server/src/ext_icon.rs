//! Extension icons for the source picker, admin extension list, and translator
//! chips.
//!
//! WHY WE HOST THESE. Komika used to point the browser straight at Keiyoushi's
//! `extensions/repo/icon/{pkgName}.png`. When Keiyoushi migrated their index
//! from `index.min.json` to `index.pb` they emptied that directory, so every
//! icon URL in the product started 404ing at once. The engine's own
//! `/api/v1/extension/icon/…` was no fallback: it unpacks the icon from the
//! INSTALLED apk, so it 500s for everything in the browse list.
//!
//! So the icons are vendored. `assets/ext-icons/` holds a snapshot (refreshed by
//! `scripts/fetch-ext-icons.mjs`) that `deploy/server.Dockerfile` copies into the
//! image, and `serve_ext_icon` serves it from our own origin. That directory is
//! authoritative and answers essentially every request without touching the
//! network or the DB.
//!
//! The lazy fetch below covers everything the snapshot can't: extensions
//! published AFTER a given snapshot was taken, and — on a self-hosted server,
//! where the operator adds whatever repos they like — every extension from any
//! other repo. Each is fetched once, cached in `extension_icon_blob`, and served
//! from there afterwards, so a newly-listed source gets its icon without a
//! redeploy.
//!
//! WHERE THE ICON COMES FROM. The extension's OWN repo ([`icon_candidates`]): a
//! Mihon-format repo keeps `icon/{pkgName}.png` next to its index, so the URL is
//! derived from the repo the engine reports for the extension. Keiyoushi is the one
//! repo known not to (its `icon/` directory is the one that was emptied), so for a
//! Keiyoushi extension the source-tree layout is tried after it — the same fetch
//! the snapshot was originally built from. The snapshot itself stays the first
//! tier, because it is local disk and answers the hosted instance's ~1,300
//! Keiyoushi icons without a single outbound request.

use sqlx::SqlitePool;
use std::time::Duration;

/// Bound the outbound fetch hard. This runs on a request path, and a stalled
/// upstream must surface as a placeholder rather than a hung image slot.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a recorded miss (empty-BLOB tombstone) suppresses re-fetching. ~41
/// extensions publish no icon anywhere; without this, each one would hit
/// Keiyoushi on EVERY request for it. A week is short enough that an icon added
/// upstream still appears on its own, and long enough that the misses cost
/// nothing.
const MISS_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// Reject anything that isn't a plain extension package name before it reaches
/// the filesystem or an outbound URL. Package names are dot-separated
/// alphanumeric segments, so allowing exactly that also rules out `/`, `\`, and
/// `..` path traversal — the file name is joined onto the icons directory.
pub fn is_valid_pkg(pkg: &str) -> bool {
    !pkg.is_empty()
        && pkg.len() <= 200
        && !pkg.starts_with('.')
        && !pkg.ends_with('.')
        && !pkg.contains("..")
        && pkg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// The `repo` branch base every Keiyoushi index URL canonicalizes to, once
/// `github.com/…/raw/…` is rewritten to the direct raw host ([`repo_base`]). The
/// one repo whose `icon/` directory is known to be empty, and the origin of the
/// vendored snapshot.
pub const KEIYOUSHI_REPO_BASE: &str = "https://raw.githubusercontent.com/keiyoushi/extensions/repo";

/// The directory an extension repo's index lives in, as a fetchable base URL.
///
/// The engine reports an extension's repo as its store's index URL (for Keiyoushi,
/// canonicalized to `https://github.com/keiyoushi/extensions/raw/repo/index.pb`).
/// A GitHub `/raw/` path is rewritten to `raw.githubusercontent.com`, skipping the
/// redirect. `None` when the URL does not end in an `index.*` file: an unknown
/// layout, where guessing an icon path would only produce 404s.
pub fn repo_base(repo: &str) -> Option<String> {
    let (base, index) = repo.trim().rsplit_once('/')?;
    if !index.starts_with("index.")
        || !(base.starts_with("https://") || base.starts_with("http://"))
    {
        return None;
    }
    Some(match base.strip_prefix("https://github.com/") {
        Some(rest) if rest.contains("/raw/") => format!(
            "https://raw.githubusercontent.com/{}",
            rest.replacen("/raw/", "/", 1)
        ),
        _ => base.to_string(),
    })
}

/// Where to look for `pkg`'s icon, best first, given the repo the engine reports
/// for it (`None` = the extension is not known to the engine).
///
/// 1. The extension's own repo: `{base}/icon/{pkg}.png`, the Mihon repo layout.
/// 2. For a Keiyoushi extension (its repo is Keiyoushi's, or unknown and the
///    package is Keiyoushi-shaped): Keiyoushi's source tree, since their `icon/`
///    directory no longer holds the icons.
pub fn icon_candidates(repo: Option<&str>, pkg: &str) -> Vec<String> {
    let base = repo.and_then(repo_base);
    let mut out = Vec::new();
    if let Some(base) = &base {
        out.push(format!("{base}/icon/{pkg}.png"));
    }
    let keiyoushi_repo = match &base {
        Some(b) => b == KEIYOUSHI_REPO_BASE,
        None => repo.is_none(),
    };
    if keiyoushi_repo {
        out.extend(keiyoushi_source_url(pkg));
    }
    out
}

/// Keiyoushi's source-repo icon URL for a package.
///
/// `eu.kanade.tachiyomi.extension.en.foo` →
/// `extensions-source@main/src/en/foo/res/mipmap-xhdpi/ic_launcher.png`.
/// This is the layout `index.pb` itself points at; verified against the live
/// index, it reproduces all 1327 published icon URLs exactly. `None` for a
/// package that doesn't split into `{lang}.{dir}` (e.g. a locally-built apk),
/// which has no Keiyoushi-hosted icon to fetch.
pub fn keiyoushi_source_url(pkg: &str) -> Option<String> {
    let (lang, dir) = pkg
        .strip_prefix("eu.kanade.tachiyomi.extension.")?
        .split_once('.')?;
    if lang.is_empty() || dir.is_empty() || dir.contains('.') {
        return None;
    }
    Some(format!(
        "https://cdn.jsdelivr.net/gh/keiyoushi/extensions-source@main/src/{lang}/{dir}/res/mipmap-xhdpi/ic_launcher.png"
    ))
}

/// PNG magic. jsDelivr answers a missing path with an HTML error body, and on a
/// 200 that would otherwise be cached and served as a corrupt image.
fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
}

/// A cached row: `Some(bytes)` = icon, `None` = a live tombstone (recorded miss
/// still inside its TTL). An absent/expired row yields `Err(())` meaning
/// "nothing usable cached — go fetch".
#[allow(clippy::result_unit_err)]
pub async fn cached(pool: &SqlitePool, pkg: &str) -> Result<Option<Vec<u8>>, ()> {
    let row: Option<(Vec<u8>, i64)> = sqlx::query_as(
        "SELECT png, CAST(strftime('%s','now') AS INTEGER) - CAST(strftime('%s', updated_at) AS INTEGER) \
         FROM extension_icon_blob WHERE pkg_name = ?",
    )
    .bind(pkg)
    .fetch_optional(pool)
    .await
    .unwrap_or(None);
    match row {
        Some((png, _)) if !png.is_empty() => Ok(Some(png)),
        // Tombstone: honor it until it ages out, then re-check upstream.
        Some((_, age)) if age < MISS_TTL_SECS => Ok(None),
        _ => Err(()),
    }
}

/// Dedicated client for the tier-3 fetch. Deliberately NOT the MangaDex or
/// Suwayomi client: those carry upstream-specific rate limiting, retry policy
/// and concurrency permits that mean nothing to a jsDelivr GET. Held in a
/// `OnceLock` so connections still pool across requests.
fn http() -> &'static reqwest::Client {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(FETCH_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Extension icons are small (launcher PNGs are a few KB); anything near this is not
/// an icon, and the repo URL is operator-supplied, so the read is bounded.
const MAX_ICON_BYTES: usize = 1024 * 1024;

/// GET a PNG, or `None` for any failure — transport error, non-success status,
/// a body over [`MAX_ICON_BYTES`], or a body that isn't actually a PNG.
async fn fetch_png(url: &str) -> Option<Vec<u8>> {
    let mut res = http().get(url).timeout(FETCH_TIMEOUT).send().await.ok()?;
    if !res.status().is_success()
        || res
            .content_length()
            .is_some_and(|n| n > MAX_ICON_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = res.chunk().await.ok()? {
        if bytes.len() + chunk.len() > MAX_ICON_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    is_png(&bytes).then_some(bytes)
}

/// What the engine knows about an extension's repo, for [`fetch_and_cache`].
#[derive(Debug, Clone, Copy)]
pub enum RepoLookup<'a> {
    /// The extension's repo (its store's index URL).
    Repo(&'a str),
    /// The engine answered and does not know the extension (or it has no repo).
    Unknown,
    /// The engine could not be asked. A miss is then not recorded: the icon may
    /// well exist in a repo we simply could not name this time.
    Unavailable,
}

/// Fetch a package's icon from its own repo (then the Keiyoushi fallback, see
/// [`icon_candidates`]) and record the outcome — the bytes on success, an
/// empty-BLOB tombstone on a definite miss. Returns the bytes to serve.
///
/// Cache writes are best-effort: a failed write costs a repeat fetch next time,
/// which is not worth failing an image request over.
pub async fn fetch_and_cache(
    pool: &SqlitePool,
    pkg: &str,
    repo: RepoLookup<'_>,
) -> Option<Vec<u8>> {
    let candidates = icon_candidates(
        match repo {
            RepoLookup::Repo(r) => Some(r),
            RepoLookup::Unknown | RepoLookup::Unavailable => None,
        },
        pkg,
    );
    // NB: every failure here must FALL THROUGH to the tombstone write below, not
    // return early. An upstream 404 is precisely the case the tombstone exists for —
    // returning from here on a non-success status would leave the ~41 iconless
    // extensions re-fetching on every single request.
    let mut bytes = None;
    for url in &candidates {
        if let Some(b) = fetch_png(url).await {
            bytes = Some(b);
            break;
        }
    }
    if bytes.is_none() && matches!(repo, RepoLookup::Unavailable) {
        tracing::debug!(
            pkg,
            "extension icon: engine unavailable; not recording a miss"
        );
        return None;
    }
    let store: &[u8] = bytes.as_deref().unwrap_or(&[]);
    if let Err(e) = sqlx::query(
        "INSERT INTO extension_icon_blob (pkg_name, png, updated_at) \
         VALUES (?, ?, datetime('now')) \
         ON CONFLICT(pkg_name) DO UPDATE SET png = excluded.png, updated_at = excluded.updated_at",
    )
    .bind(pkg)
    .bind(store)
    .execute(pool)
    .await
    {
        tracing::warn!(error = %e, pkg, "extension icon cache write failed");
    }
    if bytes.is_none() {
        tracing::debug!(
            pkg,
            tried = candidates.len(),
            "no repo-hosted icon; recorded miss"
        );
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal_and_junk_package_names() {
        assert!(is_valid_pkg("eu.kanade.tachiyomi.extension.en.foo"));
        assert!(is_valid_pkg("some_pkg-1"));
        // Traversal and separators must never reach a path join or a URL.
        assert!(!is_valid_pkg("../../etc/passwd"));
        assert!(!is_valid_pkg("a/b"));
        assert!(!is_valid_pkg("a\\b"));
        assert!(!is_valid_pkg("a..b"));
        assert!(!is_valid_pkg(".hidden"));
        assert!(!is_valid_pkg("trailing."));
        assert!(!is_valid_pkg(""));
        assert!(!is_valid_pkg(&"a".repeat(201)));
        // Percent-encoded traversal decodes before us; the decoded form is caught.
        assert!(!is_valid_pkg("%2e%2e/x"));
    }

    #[test]
    fn derives_keiyoushi_source_urls() {
        assert_eq!(
            keiyoushi_source_url("eu.kanade.tachiyomi.extension.en.foo").as_deref(),
            Some("https://cdn.jsdelivr.net/gh/keiyoushi/extensions-source@main/src/en/foo/res/mipmap-xhdpi/ic_launcher.png")
        );
        assert_eq!(
            keiyoushi_source_url("eu.kanade.tachiyomi.extension.all.mangadex").as_deref(),
            Some("https://cdn.jsdelivr.net/gh/keiyoushi/extensions-source@main/src/all/mangadex/res/mipmap-xhdpi/ic_launcher.png")
        );
        // Not a Keiyoushi package, or missing the `{lang}.{dir}` tail → nothing to fetch.
        assert_eq!(keiyoushi_source_url("com.example.custom"), None);
        assert_eq!(
            keiyoushi_source_url("eu.kanade.tachiyomi.extension.en"),
            None
        );
        assert_eq!(
            keiyoushi_source_url("eu.kanade.tachiyomi.extension.en.foo.bar"),
            None
        );
    }

    /// An in-memory clone of the `extension_icon_blob` schema from `db::init_covers`.
    async fn cache_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE extension_icon_blob (\
                 pkg_name   TEXT PRIMARY KEY,\
                 png        BLOB NOT NULL,\
                 updated_at TEXT NOT NULL\
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn put(pool: &SqlitePool, pkg: &str, png: &[u8], age_secs: i64) {
        sqlx::query(
            "INSERT INTO extension_icon_blob (pkg_name, png, updated_at) \
             VALUES (?, ?, datetime('now', ?))",
        )
        .bind(pkg)
        .bind(png)
        .bind(format!("-{age_secs} seconds"))
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cache_distinguishes_hit_live_tombstone_and_refetch() {
        let pool = cache_pool().await;

        // Nothing cached → "go fetch".
        assert!(cached(&pool, "pkg.absent").await.is_err());

        // A stored icon is served back verbatim.
        put(&pool, "pkg.hit", b"\x89PNG\r\n\x1a\nbytes", 0).await;
        assert_eq!(
            cached(&pool, "pkg.hit").await.unwrap().unwrap(),
            b"\x89PNG\r\n\x1a\nbytes".to_vec()
        );

        // A fresh tombstone suppresses the refetch (serves 404) rather than
        // hammering Keiyoushi on every request for an icon that doesn't exist.
        put(&pool, "pkg.miss", b"", MISS_TTL_SECS / 2).await;
        assert_eq!(cached(&pool, "pkg.miss").await.unwrap(), None);

        // Once it ages past the TTL it stops counting, so an icon added upstream
        // is picked up without anyone clearing the cache by hand.
        put(&pool, "pkg.stale", b"", MISS_TTL_SECS + 60).await;
        assert!(cached(&pool, "pkg.stale").await.is_err());
    }

    #[tokio::test]
    async fn non_keiyoushi_package_records_a_tombstone_without_fetching() {
        let pool = cache_pool().await;
        // No derivable URL → no outbound request, but the miss is still recorded
        // so the next request short-circuits instead of retrying the derivation.
        assert_eq!(
            fetch_and_cache(&pool, "com.example.custom", RepoLookup::Unknown).await,
            None
        );
        assert_eq!(cached(&pool, "com.example.custom").await.unwrap(), None);
    }

    /// Regression: an earlier version used `?` on the status check inside
    /// `fetch_and_cache`, which returned from the whole function on a non-success
    /// response and so never wrote the tombstone — leaving the ~41 iconless
    /// extensions re-fetching from Keiyoushi on EVERY request. A failed fetch must
    /// always leave a recorded miss behind.
    #[tokio::test]
    async fn a_failed_fetch_still_records_a_tombstone() {
        let pool = cache_pool().await;
        // A derivable-but-unroutable URL exercises the same fall-through the
        // upstream-404 path takes, without depending on the network.
        let pkg = "eu.kanade.tachiyomi.extension.zz.definitely-not-a-real-extension";
        // Port 1 on loopback: a refused connection, never the network.
        let repo = "http://127.0.0.1:1/repo/index.min.json";
        assert_eq!(
            fetch_and_cache(&pool, pkg, RepoLookup::Repo(repo)).await,
            None
        );
        let tombstoned: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM extension_icon_blob WHERE pkg_name = ?")
                .bind(pkg)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(tombstoned, 1, "a failed fetch must record a miss");
        assert_eq!(cached(&pool, pkg).await.unwrap(), None);
    }

    #[test]
    fn icons_come_from_the_extensions_own_repo() {
        let pkg = "eu.kanade.tachiyomi.extension.en.foo";
        // A Mihon-format repo: its own `icon/` directory, nothing else (no Keiyoushi
        // guess for an extension that is not Keiyoushi's, even when shaped like one).
        assert_eq!(
            icon_candidates(Some("https://example.org/ext/index.min.json"), pkg),
            vec![format!("https://example.org/ext/icon/{pkg}.png")]
        );
        // GitHub `/raw/` index URLs go straight to the raw host.
        assert_eq!(
            icon_candidates(
                Some("https://github.com/someone/repo/raw/main/index.min.json"),
                pkg
            ),
            vec![format!(
                "https://raw.githubusercontent.com/someone/repo/main/icon/{pkg}.png"
            )]
        );
        // Keiyoushi: its own repo first, then the source tree (its `icon/` is empty).
        assert_eq!(
            icon_candidates(
                Some("https://github.com/keiyoushi/extensions/raw/repo/index.pb"),
                pkg
            ),
            vec![
                format!("{KEIYOUSHI_REPO_BASE}/icon/{pkg}.png"),
                keiyoushi_source_url(pkg).unwrap(),
            ]
        );
        // Unknown to the engine: only the legacy Keiyoushi derivation, if any.
        assert_eq!(
            icon_candidates(None, pkg),
            vec![keiyoushi_source_url(pkg).unwrap()]
        );
        assert!(icon_candidates(None, "com.example.custom").is_empty());
        // A repo URL with no index file: no guess at all.
        assert!(icon_candidates(Some("https://example.org/store"), "com.example.x").is_empty());
        assert_eq!(repo_base("ftp://example.org/index.json"), None);
    }

    /// A local "repo" serving one icon at the Mihon path, counting requests.
    async fn icon_repo() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::{http::header, routing::get, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/repo/icon/{file}",
            get(
                move |axum::extract::Path(file): axum::extract::Path<String>| {
                    let h = h.clone();
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        if file == "pkg.has.icon.png" {
                            (
                                axum::http::StatusCode::OK,
                                [(header::CONTENT_TYPE, "image/png")],
                                b"\x89PNG\r\n\x1a\nicon".to_vec(),
                            )
                        } else {
                            (
                                axum::http::StatusCode::NOT_FOUND,
                                [(header::CONTENT_TYPE, "text/html")],
                                b"<html>404</html>".to_vec(),
                            )
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/repo/index.min.json"), hits)
    }

    #[tokio::test]
    async fn fetches_from_the_repo_then_serves_from_cache() {
        use std::sync::atomic::Ordering;
        let pool = cache_pool().await;
        let (repo, hits) = icon_repo().await;

        let got = fetch_and_cache(&pool, "pkg.has.icon", RepoLookup::Repo(&repo)).await;
        assert_eq!(got.as_deref(), Some(&b"\x89PNG\r\n\x1a\nicon"[..]));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // Cached now: the route's tier 2 answers without the repo.
        assert_eq!(
            cached(&pool, "pkg.has.icon").await.unwrap().as_deref(),
            Some(&b"\x89PNG\r\n\x1a\nicon"[..])
        );

        // The repo has no icon for this one: a definite miss, recorded.
        assert_eq!(
            fetch_and_cache(&pool, "pkg.no.icon", RepoLookup::Repo(&repo)).await,
            None
        );
        assert_eq!(cached(&pool, "pkg.no.icon").await.unwrap(), None);

        // The engine could not be asked: nothing to try for a non-Keiyoushi package,
        // and NO tombstone — the icon may exist in a repo we could not name this time.
        assert_eq!(
            fetch_and_cache(&pool, "pkg.engine.down", RepoLookup::Unavailable).await,
            None
        );
        assert!(cached(&pool, "pkg.engine.down").await.is_err());
    }

    #[test]
    fn png_magic_rejects_html_error_bodies() {
        assert!(is_png(&[
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00
        ]));
        assert!(!is_png(b"<!DOCTYPE html><html>404"));
        assert!(!is_png(b""));
    }
}
