//! Self-host "Sources" surface: the MangaDex switch, the MangaDex-only operations it
//! gates, and the extension repo list/remove — against a mock engine and a MangaDex
//! client that can only ever reach a dead local port.

use super::*;
use std::sync::{Arc, Mutex as StdMutex};

/// What the mock engine was asked to remove, in order.
type Removed = Arc<StdMutex<Vec<String>>>;

/// A mock Suwayomi engine holding two extension stores, answering `extensionStores`
/// and `removeExtensionStore` the way the engine does (GQL-SCHEMA-FINDINGS §B1).
async fn mock_engine() -> (String, Removed) {
    let stores = Arc::new(StdMutex::new(vec![
        serde_json::json!({"indexUrl": "https://github.com/keiyoushi/extensions/raw/repo/index.pb", "name": "Keiyoushi", "badgeLabel": "KEI"}),
        serde_json::json!({"indexUrl": "https://example.org/ext/index.min.json", "name": "Example", "badgeLabel": "EX"}),
    ]));
    let removed: Removed = Arc::default();
    let rec = removed.clone();
    let router = axum::Router::new().route(
        "/api/graphql",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let stores = stores.clone();
            let rec = rec.clone();
            async move {
                let q = body["query"].as_str().unwrap_or("");
                if q.contains("removeExtensionStore") {
                    let url = body["variables"]["url"].as_str().unwrap().to_string();
                    rec.lock().unwrap().push(url.clone());
                    stores
                        .lock()
                        .unwrap()
                        .retain(|s| s["indexUrl"] != url.as_str());
                    axum::Json(serde_json::json!({"data": {"removeExtensionStore":
                        {"extensionStore": {"indexUrl": url}}}}))
                } else if q.contains("extensionStores") {
                    let nodes = stores.lock().unwrap().clone();
                    axum::Json(serde_json::json!({"data": {"extensionStores": {"nodes": nodes}}}))
                } else {
                    axum::Json(serde_json::json!({"errors": [{"message": "unexpected query"}]}))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), removed)
}

/// Schema + state with an admin (`admintok`) and a reader (`bobtok`). The MangaDex
/// client starts OFF (as on a fresh install) and points at a dead port, so even a
/// switched-on call can never leave the machine.
async fn fixture(engine: &str) -> (ApiSchema, Arc<AppState>) {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    for (id, name, admin, tok) in [
        ("admin-id", "admin", 1, "admintok"),
        ("bob-id", "bob", 0, "bobtok"),
    ] {
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, is_admin, created_at) \
             VALUES (?, ?, ?, 'x', ?, '2020-01-01T00:00:00Z')",
        )
        .bind(id)
        .bind(name)
        .bind(format!("{name}@e.com"))
        .bind(admin)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (token, user_id, created_at, expires_at) \
             VALUES (?, ?, '2020-01-01T00:00:00Z', '2999-01-01T00:00:00Z')",
        )
        .bind(auth::hash_token(tok))
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    let state = Arc::new(AppState {
        pool: pool.clone(),
        cover_pool: pool,
        suwayomi: SuwayomiClient::new(engine.to_string(), None, None),
        mangadex: Arc::new(
            crate::mangadex::MangaDexClient::new("sources-test", 50.0, 600.0)
                .with_bases("http://127.0.0.1:1", "http://127.0.0.1:1"),
        ),
        admin_users: vec![],
        scan_health: Mutex::new(ScanHealth::default()),
        auth_limiter: RateLimiter::new(100, 60),
        password_work: Arc::new(tokio::sync::Semaphore::new(4)),
        federated_limiter: RateLimiter::new(100, 60),
        session_ttl_secs: 3600,
        series_inflight: KeyedLocks::default(),
        chapters_inflight: KeyedLocks::default(),
        cover_crawl_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        catalogue_cover_phash: false,
        ext_icons_dir: std::path::PathBuf::new(),
        setup_code: crate::instance::SetupCode::default(),
    });
    (build_schema(state.clone(), false), state)
}

async fn run(schema: &ApiSchema, query: &str, token: Option<&str>) -> async_graphql::Response {
    schema
        .execute(
            async_graphql::Request::new(query)
                .data(RequestAuth(token.map(str::to_string)))
                .data(ClientIp(Some("1.1.1.1".into())))
                .data(RequestUserCache::default())
                .data(RequestLibraryCache::default()),
        )
        .await
}

fn ok(resp: async_graphql::Response) -> serde_json::Value {
    assert!(resp.errors.is_empty(), "unexpected: {:?}", resp.errors);
    resp.data.into_json().unwrap()
}

fn err(resp: &async_graphql::Response) -> String {
    resp.errors
        .first()
        .map(|e| e.message.clone())
        .unwrap_or_default()
}

const SETTING_Q: &str =
    "{ mangadexSetting { enabled pinned pinnedBy } instance { mangadexEnabled } }";

/// One test for everything that reads `KOMIKA_MANGADEX` / `CATALOGUE_SYNC`, so the env
/// overrides cannot race another test's read of the same variables. (The full
/// deployment matrix is `instance::tests::mangadex_usage_and_crawl_across_deployments`,
/// with an injected environment.)
#[tokio::test]
async fn the_mangadex_switch_is_admin_only_toggles_live_and_env_pins_it() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    const VARS: [&str; 2] = ["CATALOGUE_SYNC", "KOMIKA_MANGADEX"];
    let saved: Vec<Option<String>> = VARS.iter().map(|v| std::env::var(v).ok()).collect();
    for v in VARS {
        std::env::remove_var(v);
    }

    // A server that booted with MangaDex off (a self-host): off, not pinned.
    let d = ok(run(&s, SETTING_Q, Some("admintok")).await);
    assert_eq!(d["mangadexSetting"]["enabled"], false);
    assert_eq!(d["mangadexSetting"]["pinned"], false);
    assert_eq!(d["instance"]["mangadexEnabled"], false);

    // Readers can neither read the admin view nor flip it.
    for q in [
        "{ mangadexSetting { enabled } }",
        "mutation { setMangadexEnabled(enabled: true) }",
    ] {
        assert_eq!(
            err(&run(&s, q, Some("bobtok")).await),
            "Admin access required"
        );
    }
    assert!(!st.mangadex.is_enabled());

    // On: stored, applied to the live client (what every loop reads), reported.
    let d = ok(run(
        &s,
        "mutation { setMangadexEnabled(enabled: true) }",
        Some("admintok"),
    )
    .await);
    assert_eq!(d["setMangadexEnabled"], true);
    assert!(st.mangadex.is_enabled());
    let stored: String =
        sqlx::query_scalar("SELECT value FROM instance_setting WHERE key = 'mangadex_enabled'")
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(stored, "true");
    let d = ok(run(&s, SETTING_Q, Some("admintok")).await);
    assert_eq!(d["mangadexSetting"]["enabled"], true);
    assert_eq!(d["instance"]["mangadexEnabled"], true);

    // And off again, at runtime.
    let d = ok(run(
        &s,
        "mutation { setMangadexEnabled(enabled: false) }",
        Some("admintok"),
    )
    .await);
    assert_eq!(d["setMangadexEnabled"], false);
    assert!(!st.mangadex.is_enabled());

    // Env wins: a pinned value is shown read-only, naming the variable, and the
    // mutation is refused without writing anything.
    for (var, value) in [("CATALOGUE_SYNC", "on"), ("KOMIKA_MANGADEX", "off")] {
        std::env::set_var(var, value);
        let d = ok(run(&s, SETTING_Q, Some("admintok")).await);
        assert_eq!(d["mangadexSetting"]["pinned"], true, "{var}");
        assert_eq!(d["mangadexSetting"]["pinnedBy"], var);
        let r = run(
            &s,
            "mutation { setMangadexEnabled(enabled: true) }",
            Some("admintok"),
        )
        .await;
        assert_eq!(
            err(&r),
            format!("MangaDex is set by {var} on this server and cannot be changed here.")
        );
        let stored: String =
            sqlx::query_scalar("SELECT value FROM instance_setting WHERE key = 'mangadex_enabled'")
                .fetch_one(&st.pool)
                .await
                .unwrap();
        assert_eq!(stored, "false", "a refused change writes nothing");
        std::env::remove_var(var);
    }

    // CATALOGUE_SYNC=off only stops the crawl on this replica: usage stays the admin's.
    std::env::set_var("CATALOGUE_SYNC", "off");
    let d = ok(run(&s, SETTING_Q, Some("admintok")).await);
    assert_eq!(d["mangadexSetting"]["pinned"], false);
    let d = ok(run(
        &s,
        "mutation { setMangadexEnabled(enabled: true) }",
        Some("admintok"),
    )
    .await);
    assert_eq!(d["setMangadexEnabled"], true);

    for (var, value) in VARS.iter().zip(saved) {
        match value {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }
}

#[tokio::test]
async fn mangadex_only_operations_refuse_while_off() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    let off = crate::mangadex::MANGADEX_OFF_MESSAGE;
    // `canonicalPages` is a reader query; the other two are admin jobs.
    for (q, tok) in [
        (r#"{ canonicalPages(chapterId: "ch-1") { index } }"#, None),
        (
            r#"{ canonicalPages(chapterId: "ch-1") { index } }"#,
            Some("bobtok"),
        ),
        ("mutation { backfillMangadexMetadata }", Some("admintok")),
        ("mutation { resyncCatalogue }", Some("admintok")),
    ] {
        assert_eq!(err(&run(&s, q, tok).await), off, "{q}");
    }
    // Admin gates still come first for the admin jobs.
    assert_eq!(
        err(&run(&s, "mutation { resyncCatalogue }", Some("bobtok")).await),
        "Admin access required"
    );

    // Switched on, `canonicalPages` goes past the gate to the (dead) MangaDex port:
    // the refusal above was the switch, not an accident of the fixture.
    st.mangadex.set_enabled(true);
    let r = run(
        &s,
        r#"{ canonicalPages(chapterId: "ch-1") { index } }"#,
        None,
    )
    .await;
    let msg = err(&r);
    assert!(!msg.is_empty() && msg != off, "{msg}");
}

#[tokio::test]
async fn extension_repos_are_listed_and_removed_through_the_engine() {
    let (engine, removed) = mock_engine().await;
    let (s, _) = fixture(&engine).await;
    let list = "{ extensionRepos { indexUrl name badgeLabel } }";

    let d = ok(run(&s, list, Some("admintok")).await);
    let repos = d["extensionRepos"].as_array().unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0]["name"], "Keiyoushi");
    assert_eq!(repos[0]["badgeLabel"], "KEI");

    // Readers: refused before the engine is asked.
    for q in [
        list.to_string(),
        r#"mutation { removeExtensionRepo(indexUrl: "https://example.org/ext/index.min.json") }"#
            .to_string(),
    ] {
        assert_eq!(
            err(&run(&s, &q, Some("bobtok")).await),
            "Admin access required"
        );
    }

    // An URL the engine does not list (here: the pre-canonical Keiyoushi form) is
    // refused, and nothing is sent to the engine.
    let r = run(
        &s,
        r#"mutation { removeExtensionRepo(indexUrl: "https://raw.githubusercontent.com/keiyoushi/extensions/repo/index.min.json") }"#,
        Some("admintok"),
    )
    .await;
    assert_eq!(err(&r), "No such repo.");
    assert!(removed.lock().unwrap().is_empty());

    // The listed URL is removed by exactly that URL.
    let d = ok(run(
        &s,
        r#"mutation { removeExtensionRepo(indexUrl: " https://example.org/ext/index.min.json ") }"#,
        Some("admintok"),
    )
    .await);
    assert_eq!(d["removeExtensionRepo"], true);
    assert_eq!(
        *removed.lock().unwrap(),
        ["https://example.org/ext/index.min.json"]
    );
    let d = ok(run(&s, list, Some("admintok")).await);
    assert_eq!(d["extensionRepos"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn an_unreachable_engine_is_reported_plainly() {
    // Port 1: nothing listens, as on a fresh self-host before the engine is up.
    let (s, _) = fixture("http://127.0.0.1:1").await;
    for q in [
        "{ extensionRepos { indexUrl } }",
        "{ extensions { pkgName } }",
        r#"mutation { removeExtensionRepo(indexUrl: "https://example.org/index.min.json") }"#,
        r#"mutation { addExtensionRepo(indexUrl: "https://example.org/index.min.json") }"#,
    ] {
        assert_eq!(
            err(&run(&s, q, Some("admintok")).await),
            ENGINE_UNREACHABLE,
            "{q}"
        );
    }
}
