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
//! The lazy fetch below exists only for the gap the snapshot can't cover:
//! extensions Keiyoushi publishes AFTER a given snapshot was taken. Those are
//! fetched once, cached in `extension_icon_blob`, and served from there
//! afterwards — so a newly-listed source gets its icon without a redeploy.

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

/// GET a PNG, or `None` for any failure — transport error, non-success status,
/// or a body that isn't actually a PNG.
async fn fetch_png(url: &str) -> Option<Vec<u8>> {
    let res = http().get(url).timeout(FETCH_TIMEOUT).send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    let bytes = res.bytes().await.ok()?.to_vec();
    is_png(&bytes).then_some(bytes)
}

/// Fetch a package's icon from Keiyoushi and record the outcome — the bytes on
/// success, an empty-BLOB tombstone on failure. Returns the bytes to serve.
///
/// Cache writes are best-effort: a failed write costs a repeat fetch next time,
/// which is not worth failing an image request over.
pub async fn fetch_and_cache(pool: &SqlitePool, pkg: &str) -> Option<Vec<u8>> {
    let bytes = match keiyoushi_source_url(pkg) {
        // NB: every failure here must FALL THROUGH to the tombstone write below,
        // not return early. An upstream 404 is precisely the case the tombstone
        // exists for — returning from here on a non-success status would leave
        // the ~41 iconless extensions re-fetching on every single request.
        Some(url) => fetch_png(&url).await,
        None => None,
    };
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
        tracing::debug!(pkg, "no Keiyoushi-hosted icon; recorded miss");
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
        assert_eq!(fetch_and_cache(&pool, "com.example.custom").await, None);
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
        assert!(keiyoushi_source_url(pkg).is_some());
        assert_eq!(fetch_and_cache(&pool, pkg).await, None);
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
    fn png_magic_rejects_html_error_bodies() {
        assert!(is_png(&[
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00
        ]));
        assert!(!is_png(b"<!DOCTYPE html><html>404"));
        assert!(!is_png(b""));
    }
}
