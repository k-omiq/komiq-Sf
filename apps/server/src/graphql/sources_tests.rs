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
    let pool = crate::database::TestPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations")
        .run(pool.write())
        .await
        .unwrap();
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
        comment_limiter: comment_limiter(),
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

const LIMITS_Q: &str = "{ sourceRateLimits { sourceId requests windowSecs fromEnv overridesEnv } }";

/// One test for everything that reads `SCAN_SOURCE_RATE_LIMITS`, so the env override
/// cannot race another read of it. An admin adds, replaces and removes a limit; a
/// source the env limits is listed with that default, and an admin's limit replaces
/// it until removed.
#[tokio::test]
async fn source_rate_limits_are_admin_only_stored_and_override_the_env() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    const VAR: &str = "SCAN_SOURCE_RATE_LIMITS";
    let saved = std::env::var(VAR).ok();
    std::env::remove_var(VAR);

    let d = ok(run(&s, LIMITS_Q, Some("admintok")).await);
    assert_eq!(d["sourceRateLimits"], serde_json::json!([]));

    for q in [
        LIMITS_Q,
        "mutation { setSourceRateLimit(sourceId: \"42\", requests: 10, windowSecs: 300) { sourceId } }",
        "mutation { removeSourceRateLimit(sourceId: \"42\") }",
    ] {
        assert_eq!(
            err(&run(&s, q, Some("bobtok")).await),
            "Admin access required"
        );
    }

    // Add, then replace: one row per source.
    let d = ok(run(
        &s,
        "mutation { setSourceRateLimit(sourceId: \" 42 \", requests: 10, windowSecs: 300) \
         { sourceId requests windowSecs fromEnv overridesEnv } }",
        Some("admintok"),
    )
    .await);
    assert_eq!(
        d["setSourceRateLimit"],
        serde_json::json!({"sourceId": "42", "requests": 10, "windowSecs": 300,
            "fromEnv": false, "overridesEnv": false})
    );
    ok(run(
        &s,
        "mutation { setSourceRateLimit(sourceId: \"42\", requests: 6, windowSecs: 60) { sourceId } }",
        Some("admintok"),
    )
    .await);
    let d = ok(run(&s, LIMITS_Q, Some("admintok")).await);
    assert_eq!(
        d["sourceRateLimits"],
        serde_json::json!([{"sourceId": "42", "requests": 6, "windowSecs": 60,
            "fromEnv": false, "overridesEnv": false}])
    );

    // Out-of-range values never reach the table.
    for (args, msg) in [
        (
            "sourceId: \"\", requests: 1, windowSecs: 1",
            "Choose a source.",
        ),
        (
            "sourceId: \"7\", requests: 0, windowSecs: 60",
            "Requests must be between 1 and 10000.",
        ),
        (
            "sourceId: \"7\", requests: 5, windowSecs: 0",
            "The time window must be between 1 second and 24 hours.",
        ),
    ] {
        let q = format!("mutation {{ setSourceRateLimit({args}) {{ sourceId }} }}");
        assert_eq!(err(&run(&s, &q, Some("admintok")).await), msg);
    }

    // The env names a source: listed first, as the server's default.
    std::env::set_var(VAR, "9=10/300");
    let env_row = |requests: i32, from_env: bool| {
        serde_json::json!({"sourceId": "9", "requests": requests, "windowSecs": 300,
            "fromEnv": from_env, "overridesEnv": !from_env})
    };
    let d = ok(run(&s, LIMITS_Q, Some("admintok")).await);
    assert_eq!(d["sourceRateLimits"][0], env_row(10, true));
    assert_eq!(d["sourceRateLimits"].as_array().unwrap().len(), 2);

    // An admin edits it: the stored limit wins and says it replaces the default.
    let d = ok(run(
        &s,
        "mutation { setSourceRateLimit(sourceId: \"9\", requests: 4, windowSecs: 300) \
         { sourceId requests windowSecs fromEnv overridesEnv } }",
        Some("admintok"),
    )
    .await);
    assert_eq!(d["setSourceRateLimit"], env_row(4, false));
    let d = ok(run(&s, LIMITS_Q, Some("admintok")).await);
    assert_eq!(d["sourceRateLimits"][0], env_row(4, false));

    // Removing the admin's limit restores the default; there is nothing more to remove.
    let rm9 = "mutation { removeSourceRateLimit(sourceId: \"9\") }";
    assert_eq!(
        ok(run(&s, rm9, Some("admintok")).await)["removeSourceRateLimit"],
        true
    );
    let d = ok(run(&s, LIMITS_Q, Some("admintok")).await);
    assert_eq!(d["sourceRateLimits"][0], env_row(10, true));
    assert_eq!(
        ok(run(&s, rm9, Some("admintok")).await)["removeSourceRateLimit"],
        false
    );
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_rate_limit")
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(stored, 1, "only the admin's own limit for 42 is left");

    // Remove: true once, false after.
    let rm = "mutation { removeSourceRateLimit(sourceId: \"42\") }";
    assert_eq!(
        ok(run(&s, rm, Some("admintok")).await)["removeSourceRateLimit"],
        true
    );
    assert_eq!(
        ok(run(&s, rm, Some("admintok")).await)["removeSourceRateLimit"],
        false
    );

    match saved {
        Some(v) => std::env::set_var(VAR, v),
        None => std::env::remove_var(VAR),
    }
}

/// Scanlator AI tags and Browse's source list, end to end: only an admin reads the
/// scanlator list or writes a tag, everyone reads the tags, and a source is named by
/// its extension package when the engine does not answer (the mock engine refuses the
/// `sources` query).
#[tokio::test]
async fn scanlator_tags_are_admin_written_public_to_read_and_sources_are_listed() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    sqlx::query(
        "INSERT INTO work (id, primary_title, created_at, updated_at) \
         VALUES ('w1', 'One', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO browse_catalogue (work_id, reader_id, title, is_nsfw, en_chapter_count, created_at) \
         VALUES ('w1', 'w1', 'One', 0, 3, '2026-01-01T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
         VALUES ('md1', 'w1', 'mangadex', 'mangadex', 'u1', '2026-01-01T00:00:00Z'), \
                ('sy1', 'w1', 'suwayomi', '77', '1', '2026-01-01T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO source_extension (source_id, pkg_name, repo_url, lang, updated_at) \
         VALUES ('77', 'ext.en.example', 'https://example.org/repo', 'en', '2026-01-01T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO chapter (id, source_series_id, external_id, scanlator, created_at) \
         VALUES ('c1', 'md1', 'e1', 'Robo Scans', '2026-01-01T00:00:00Z'), \
                ('c2', 'md1', 'e2', 'Human Scans & Robo Scans', '2026-01-01T00:00:00Z'), \
                ('c3', 'sy1', 'e3', 'Human Scans', '2026-01-01T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();

    const LIST_Q: &str =
        "{ scanlators { total hasNextPage items { name chapterCount seriesCount ai effectiveAi } } }";
    const TAG_Q: &str = "mutation { setScanlatorAi(name: \"robo scans\", ai: AI) }";
    const TAGS_Q: &str = "{ scanlatorTags { name ai } }";

    // The list and the write are the console's; a reader gets neither.
    for q in [LIST_Q, TAG_Q] {
        assert_eq!(
            err(&run(&s, q, Some("bobtok")).await),
            "Admin access required"
        );
        assert!(!run(&s, q, None).await.errors.is_empty());
    }
    assert_eq!(
        ok(run(&s, TAGS_Q, None).await)["scanlatorTags"],
        serde_json::json!([])
    );

    // Tag one group: true once, false when nothing changes; a blank name is refused.
    assert_eq!(
        ok(run(&s, TAG_Q, Some("admintok")).await)["setScanlatorAi"],
        true
    );
    assert_eq!(
        ok(run(&s, TAG_Q, Some("admintok")).await)["setScanlatorAi"],
        false
    );
    assert_eq!(
        err(&run(
            &s,
            "mutation { setScanlatorAi(name: \"  \", ai: PARTIAL) }",
            Some("admintok")
        )
        .await),
        "Choose a scanlator."
    );

    // Anyone can read the tags, signed in or not.
    assert_eq!(
        ok(run(&s, TAGS_Q, None).await)["scanlatorTags"],
        serde_json::json!([{ "name": "robo scans", "ai": "AI" }])
    );

    // The list: the tagged group, and the joint credit that takes its tag.
    let d = ok(run(&s, LIST_Q, Some("admintok")).await);
    assert_eq!(d["scanlators"]["total"], 3);
    assert_eq!(d["scanlators"]["hasNextPage"], false);
    let row = |name: &str| {
        d["scanlators"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap_or_else(|| panic!("no row {name}"))
            .clone()
    };
    assert_eq!(
        row("Robo Scans"),
        serde_json::json!({ "name": "Robo Scans", "chapterCount": 1, "seriesCount": 1,
                            "ai": "AI", "effectiveAi": "AI" })
    );
    assert_eq!(
        row("Human Scans & Robo Scans")["ai"],
        serde_json::Value::Null
    );
    assert_eq!(row("Human Scans & Robo Scans")["effectiveAi"], "AI");
    assert_eq!(row("Human Scans")["effectiveAi"], serde_json::Value::Null);

    // Filters: by the tag that applies, by source (any case for the mirror), by name.
    let names = |q: &'static str| {
        let s = s.clone();
        async move {
            let d = ok(run(&s, q, Some("admintok")).await);
            d["scanlators"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        names("{ scanlators(ai: UNTAGGED) { items { name } } }").await,
        ["Human Scans"]
    );
    assert_eq!(
        names("{ scanlators(source: \"77\") { items { name } } }").await,
        ["Human Scans"]
    );
    assert_eq!(
        names("{ scanlators(source: \"MangaDex\", search: \"human\") { items { name } } }").await,
        ["Human Scans & Robo Scans"]
    );

    // Browse's source list: the mirror, and the source under its package's last part
    // (biggest first; these two tie, and a tie orders by id).
    assert_eq!(
        ok(run(&s, "{ browseSources { id name lang count } }", None).await)["browseSources"],
        serde_json::json!([
            { "id": "77", "name": "example", "lang": "en", "count": 1 },
            { "id": "mangadex", "name": "MangaDex", "lang": null, "count": 1 },
        ])
    );

    // Removing the tag clears it for everyone.
    assert_eq!(
        ok(run(
            &s,
            "mutation { setScanlatorAi(name: \"Robo Scans\", ai: null) }",
            Some("admintok")
        )
        .await)["setScanlatorAi"],
        true
    );
    assert_eq!(
        ok(run(&s, TAGS_Q, None).await)["scanlatorTags"],
        serde_json::json!([])
    );
}
