//! Exercise the production router and upstream transports, with real cold HTTP
//! fetches occupying the upstream resource while cached reader paths keep working.

use super::*;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tower::ServiceExt;

const SERIES_ID: i64 = 7701;
const CHAPTER_ID: i64 = 7711;
const READER_TOKEN: &str = "upstream-admission-reader";

#[derive(Clone, Default)]
struct EngineCalls {
    total: Arc<AtomicUsize>,
    cold: Arc<AtomicUsize>,
    manga_dex: Arc<AtomicUsize>,
}

struct Engine {
    url: String,
    calls: EngineCalls,
    stop: Arc<tokio::sync::Notify>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.notify_waiters();
        self.server.abort();
    }
}

impl Engine {
    async fn start() -> Self {
        let calls = EngineCalls::default();
        let stop = Arc::new(tokio::sync::Notify::new());
        let cold_stop = stop.clone();
        let suwayomi_calls = calls.clone();
        let md_calls = calls.clone();
        let router = Router::new()
            .route(
                "/api/graphql",
                post(move |Json(request): Json<Value>| {
                    let calls = suwayomi_calls.clone();
                    let stop = cold_stop.clone();
                    async move {
                        calls.total.fetch_add(1, Ordering::SeqCst);
                        let query = request["query"].as_str().unwrap();
                        let id = request["variables"]["id"].as_i64().unwrap();
                        if query.contains("fetchMangaAndChapters") && id >= 9_000_000 {
                            calls.cold.fetch_add(1, Ordering::SeqCst);
                            stop.notified().await;
                            return Json(json!({"errors": [{"message": "fixture stopped"}]}));
                        }
                        if query.contains("fetchChapterPages") {
                            assert_eq!(id, CHAPTER_ID);
                            return Json(json!({"data": {"fetchChapterPages": {
                                "pages": ["https://pages.example/one.webp", "https://pages.example/two.webp"]
                            }}}));
                        }
                        if query.contains("query CM(") {
                            assert_eq!(id, CHAPTER_ID);
                            return Json(json!({"data": {"chapters": {
                                "nodes": [{"mangaId": SERIES_ID}]
                            }}}));
                        }
                        panic!("unexpected engine request: {request}");
                    }
                }),
            )
            .route(
                "/at-home/server/admission-md-ch",
                get(move || {
                    md_calls.manga_dex.fetch_add(1, Ordering::SeqCst);
                    async {
                        Json(json!({
                            "baseUrl": "https://admission.mangadex.network",
                            "chapter": {"hash": "cached", "data": ["one.webp", "two.webp"]}
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url,
            calls,
            stop,
            server,
        }
    }

    async fn start_stalled_manga_dex_bodies() -> Self {
        let calls = EngineCalls::default();
        let stop = Arc::new(tokio::sync::Notify::new());
        let body_stop = stop.clone();
        let body_calls = calls.clone();
        let router = Router::new().route(
            "/at-home/server/{id}",
            get(move |UrlPath(id): UrlPath<String>| {
                let calls = body_calls.clone();
                let stop = body_stop.clone();
                async move {
                    calls.manga_dex.fetch_add(1, Ordering::SeqCst);
                    if id == "warm" {
                        return Json(json!({
                            "baseUrl": "https://body.mangadex.network",
                            "chapter": {"hash": "warm", "data": ["one.webp"]}
                        }))
                        .into_response();
                    }
                    // Send successful headers and a JSON prefix immediately. The
                    // caller must retain its permit while decoding the unfinished
                    // body, after get_with_retry has returned its Response.
                    let prefix = futures::stream::once(async move {
                        calls.cold.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(
                            br#"{"baseUrl":"https://body.mangadex.network","chapter":{"#,
                        ))
                    });
                    let tail = futures::stream::once(async move {
                        stop.notified().await;
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(
                            br#""hash":"stopped","data":["one.webp"]}}"#,
                        ))
                    });
                    axum::http::Response::builder()
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(axum::body::Body::from_stream(prefix.chain(tail)))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url,
            calls,
            stop,
            server,
        }
    }
}

fn http_request(
    query: &str,
    client: usize,
    authenticated: bool,
) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/graphql")
        .header(header::CONTENT_TYPE, "application/json");
    if authenticated {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {READER_TOKEN}"));
    }
    let mut request = builder
        .body(axum::body::Body::from(json!({"query": query}).to_string()))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(
        format!("198.18.0.{client}:1234")
            .parse::<SocketAddr>()
            .unwrap(),
    ));
    request
}

async fn execute(app: &Router, query: &str, client: usize, authenticated: bool) -> Value {
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        app.clone()
            .oneshot(http_request(query, client, authenticated)),
    )
    .await
    .unwrap_or_else(|_| panic!("cached request stalled behind cold fetches: {query}"))
    .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{query}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value.get("errors").is_none(), "{query}: {value}");
    value["data"].clone()
}

async fn wait_for_cold(calls: &EngineCalls, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while calls.cold.load(Ordering::SeqCst) != count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "expected {count} real hanging engine calls, got {}",
            calls.cold.load(Ordering::SeqCst)
        )
    });
}

fn start_cold(tasks: &mut tokio::task::JoinSet<()>, app: &Router, id: usize, client: usize) {
    let app = app.clone();
    tasks.spawn(async move {
        let query = format!("{{ series(id: \"{}\") {{ id title }} }}", 9_000_000 + id);
        let response = app
            .oneshot(http_request(&query, client, false))
            .await
            .unwrap();
        panic!(
            "cold request unexpectedly completed with {}",
            response.status()
        );
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cached_reader_requests_survive_all_upstream_slots_hanging_and_client_limit() {
    let engine = Engine::start().await;
    let pool = db::Database::connect("sqlite::memory:").await.unwrap();
    crate::db::MIGRATOR_PG.run(pool.write()).await.unwrap();
    let cover_dir = crate::cover_store::test_support::TestDir::new();
    let covers = crate::cover_store::test_support::store(&cover_dir);
    let reserved = ReadyDb::connect(&pool).await.unwrap();
    let reader_id = crate::db_types::test_uuid("admission-reader");
    sqlx::query("INSERT INTO users (id, username, email, password_hash, created_at) VALUES ($1, 'admission-reader', 'admission@example.test', 'unused', '2020-01-01T00:00:00Z')")
        .bind(reader_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO sessions (token, user_id, created_at, expires_at) VALUES ($1, $2, '2020-01-01T00:00:00Z', '2999-01-01T00:00:00Z')")
        .bind(auth::hash_token(READER_TOKEN)).bind(reader_id).execute(&pool).await.unwrap();
    catalog::upsert_work_from_mangadex(
        &pool,
        "admission-md",
        &catalog::WorkInput {
            primary_title: Some("Cached canonical title".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let canonical_source =
        catalog::find_source_series_id(&pool, "mangadex", "mangadex", "admission-md")
            .await
            .unwrap()
            .unwrap();
    catalog::upsert_chapter(
        &pool,
        &canonical_source,
        &catalog::ChapterInput {
            external_id: "admission-md-ch".into(),
            number: Some("1".into()),
            lang: Some("en".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let work_id: String = sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1")
        .bind(&canonical_source)
        .fetch_one(&pool)
        .await
        .unwrap();
    let manga = suwayomi::SuwayomiManga {
        engine_generation: None,
        real_url: None,
        id: SERIES_ID,
        title: "Cached mirror title".into(),
        url: None,
        thumbnail_url: None,
        author: None,
        artist: None,
        description: None,
        genre: vec![],
        status: "ONGOING".into(),
        in_library: true,
        in_library_at: None,
        last_fetched_at: None,
        latest_chapter_at: None,
        source_id: "admission-source".into(),
        source: Some(suwayomi::SuwayomiSourceLang {
            lang: Some("en".into()),
        }),
        chapters: Some(suwayomi::ChapterCount { total_count: 1 }),
    };
    series_cache::put_series(&pool, &manga).await.unwrap();
    catalog::upsert_source_series(
        &pool,
        &work_id,
        "suwayomi",
        "admission-source",
        &SERIES_ID.to_string(),
        None,
        false,
    )
    .await
    .unwrap();
    series_cache::put_chapters(
        &pool,
        SERIES_ID,
        &[suwayomi::SuwayomiChapter {
            engine_generation: None,
            manga: None,
            url: None,
            real_url: None,
            source_order: None,
            id: CHAPTER_ID,
            manga_id: SERIES_ID,
            name: "Cached chapter".into(),
            chapter_number: 1.0,
            scanlator: None,
            upload_date: None,
            is_read: false,
            is_bookmarked: false,
            is_downloaded: false,
            last_page_read: 0,
            page_count: 2,
        }],
    )
    .await
    .unwrap();
    let budgets = Arc::new(request_budget::RequestBudgets::default());
    let mangadex = Arc::new(
        mangadex::MangaDexClient::new("admission-test", 50.0, 600.0)
            .with_bases(&engine.url, &engine.url),
    );
    mangadex.set_enabled(true);
    let state = Arc::new(AppState {
        pool: pool.clone(),
        cover_pool: covers.clone(),
        suwayomi: SuwayomiClient::new(engine.url.clone(), None, Some("admission-source".into())),
        mangadex,
        admin_users: vec![],
        scan_health: std::sync::Mutex::new(ScanHealth::default()),
        auth_limiter: RateLimiter::new(100, 60),
        comment_limiter: graphql::comment_limiter(),
        rating_limiter: graphql::rating_limiter(),
        password_work: Arc::new(tokio::sync::Semaphore::new(4)),
        federated_limiter: RateLimiter::new(100, 60),
        session_ttl_secs: 3600,
        series_inflight: KeyedLocks::default(),
        chapters_inflight: KeyedLocks::default(),
        source_budgets: Default::default(),
        catalogue_cover_phash: false,
        ext_icons_dir: std::path::PathBuf::new(),
        media_manifests: None,
        policy: Default::default(),
        setup_code: instance::SetupCode::default(),
    });
    let trusted = Arc::new(TrustedProxies::for_tests(vec![], false));
    let mut cfg = Config::from_env();
    cfg.img_proxy_enabled = false;
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
        web_dir::Apps::from_config(&cfg),
        cfg.img_proxy_enabled,
    );

    let pages_query = format!("{{ pages(chapterId: \"{CHAPTER_ID}\") {{ index sourceUrl }} }}");
    let canonical_pages_query =
        "{ canonicalPages(chapterId: \"admission-md-ch\") { index sourceUrl } }";
    let warmed_pages = execute(&app, &pages_query, 1, false).await;
    assert_eq!(
        warmed_pages["pages"],
        json!([
            {"index": 0, "sourceUrl": "https://pages.example/one.webp"},
            {"index": 1, "sourceUrl": "https://pages.example/two.webp"},
        ])
    );
    let warmed_canonical = execute(&app, canonical_pages_query, 1, false).await;
    assert_eq!(
        warmed_canonical["canonicalPages"],
        json!([
            {"index": 0, "sourceUrl": "https://admission.mangadex.network/data/cached/one.webp"},
            {"index": 1, "sourceUrl": "https://admission.mangadex.network/data/cached/two.webp"},
        ])
    );
    assert_eq!(engine.calls.total.load(Ordering::SeqCst), 2);
    assert_eq!(engine.calls.manga_dex.load(Ordering::SeqCst), 1);

    let mut cold = tokio::task::JoinSet::new();
    for id in 0..3 {
        start_cold(&mut cold, &app, id, 1);
    }
    wait_for_cold(&engine.calls, 3).await;
    assert_eq!(budgets.upstream_available_for_test(), 13);
    let refused = tokio::time::timeout(
        Duration::from_secs(2),
        app.clone()
            .oneshot(http_request("{ series(id: \"9999999\") { id } }", 1, false)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.headers()[header::RETRY_AFTER], "1");
    assert_eq!(
        engine.calls.cold.load(Ordering::SeqCst),
        3,
        "fourth client miss reached engine"
    );
    assert_eq!(
        engine.calls.total.load(Ordering::SeqCst),
        5,
        "fourth client request sent fallback HTTP"
    );
    // Another client can join the same cold key without using network slots,
    // but those single-flight waiters still consume that client's three-slot
    // allowance. Cancelling them must return both waiter and client capacity.
    let mut waiters = tokio::task::JoinSet::new();
    for _ in 0..3 {
        start_cold(&mut waiters, &app, 0, 2);
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while budgets.upstream_waiters_available_for_test() != 13 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("same-key waiters did not enter bounded single-flight waiting");
    assert_eq!(budgets.upstream_available_for_test(), 13);
    assert_eq!(engine.calls.total.load(Ordering::SeqCst), 5);
    assert_eq!(waiters.len(), 3);
    assert!(waiters.try_join_next().is_none());
    let refused = tokio::time::timeout(
        Duration::from_secs(2),
        app.clone()
            .oneshot(http_request("{ series(id: \"9999997\") { id } }", 2, false)),
    )
    .await
    .expect("a fourth request queued behind the client's single-flight waiters")
    .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(engine.calls.total.load(Ordering::SeqCst), 5);
    waiters.abort_all();
    while let Some(result) = waiters.join_next().await {
        assert!(result.unwrap_err().is_cancelled());
    }
    assert_eq!(budgets.upstream_waiters_available_for_test(), 16);
    assert_eq!(budgets.upstream_available_for_test(), 13);
    start_cold(&mut cold, &app, 3, 2);
    wait_for_cold(&engine.calls, 4).await;
    assert_eq!(
        budgets.upstream_available_for_test(),
        12,
        "other client could not use remaining capacity"
    );
    for id in 4..16 {
        start_cold(&mut cold, &app, id, 3 + (id - 4) / 3);
    }
    wait_for_cold(&engine.calls, 16).await;
    assert_eq!(budgets.upstream_available_for_test(), 0);
    let globally_refused = tokio::time::timeout(
        Duration::from_secs(2),
        app.clone()
            .oneshot(http_request("{ series(id: \"9999998\") { id } }", 7, false)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(globally_refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(globally_refused.headers()[header::RETRY_AFTER], "1");
    assert_eq!(
        engine.calls.total.load(Ordering::SeqCst),
        18,
        "seventeenth fetch reached engine"
    );

    // A single actor must not fill all local slots by joining one slow cache
    // key. Every duplicate is refused promptly, with no extra engine request.
    let duplicates = tokio::time::timeout(
        Duration::from_secs(5),
        futures::future::join_all((0..64).map(|_| {
            app.clone().oneshot(http_request(
                "{ series(id: \"9000000\") { id title } }",
                1,
                false,
            ))
        })),
    )
    .await
    .expect("duplicate same-key requests occupied the local lane while waiting");
    for response in duplicates {
        let response = response.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    }
    assert_eq!(engine.calls.total.load(Ordering::SeqCst), 18);
    assert_eq!(budgets.upstream_available_for_test(), 0);
    assert_eq!(budgets.upstream_waiters_available_for_test(), 16);

    let series_page_query = format!(
        r#"{{ seriesPage(workId: "{work_id}") {{
        workId series {{ title }} spineChapters {{ id number }}
        sources {{ source {{ sourceType sourceKey }} series {{ title }} chapters {{ id number }} }}
    }} }}"#
    );
    let page = execute(&app, &series_page_query, 1, false).await;
    let page = &page["seriesPage"];
    assert_eq!(page["workId"], work_id);
    assert_eq!(page["series"]["title"], "Cached canonical title");
    assert_eq!(
        page["spineChapters"],
        json!([{"id": "admission-md-ch", "number": 1.0}])
    );
    let mirror = page["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["source"]["sourceType"] == "suwayomi")
        .unwrap();
    assert_eq!(mirror["source"]["sourceKey"], SERIES_ID.to_string());
    assert_eq!(
        mirror["series"]["title"], "Cached mirror title",
        "bundle silently dropped cached metadata"
    );
    assert_eq!(
        mirror["chapters"],
        json!([{"id": CHAPTER_ID.to_string(), "number": 1.0}]),
        "bundle silently dropped cached chapters"
    );
    assert_eq!(execute(&app, &pages_query, 1, false).await, warmed_pages);
    assert_eq!(
        execute(&app, canonical_pages_query, 1, false).await,
        warmed_canonical
    );
    let discovery = execute(&app, "{ discovery { title items { id title } } }", 1, false).await;
    assert!(
        discovery["discovery"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rail| rail["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["title"] == "Cached mirror title")),
        "local discovery lost cached title: {discovery}"
    );
    let series = execute(
        &app,
        &format!("{{ series(id: \"{SERIES_ID}\") {{ title }} }}"),
        1,
        false,
    )
    .await;
    assert_eq!(series["series"]["title"], "Cached mirror title");
    let chapters = execute(
        &app,
        &format!("{{ chapters(seriesId: \"{SERIES_ID}\") {{ id number }} }}"),
        1,
        false,
    )
    .await;
    assert_eq!(
        chapters["chapters"],
        json!([{"id": CHAPTER_ID.to_string(), "number": 1.0}])
    );
    let mark = execute(
        &app,
        &format!(
            "mutation {{ mark(seriesId: \"{SERIES_ID}\", marked: true) {{ title isMarked }} }}"
        ),
        1,
        true,
    )
    .await;
    assert_eq!(
        mark["mark"],
        json!({"title": "Cached mirror title", "isMarked": true})
    );
    let member: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user_library WHERE user_id = $1 AND series_id = $2",
    )
    .bind(reader_id)
    .bind(&work_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(member, 1, "mark did not persist membership");
    let favorite = execute(&app, &format!("mutation {{ setFavorite(seriesId: \"{SERIES_ID}\", favorite: true) {{ title isFavorite }} }}"), 1, true).await;
    assert_eq!(
        favorite["setFavorite"],
        json!({"title": "Cached mirror title", "isFavorite": true})
    );
    let status = execute(&app, &format!("mutation {{ setLibraryStatus(seriesId: \"{SERIES_ID}\", status: \"reading\") {{ title libraryStatus }} }}"), 1, true).await;
    assert_eq!(
        status["setLibraryStatus"],
        json!({"title": "Cached mirror title", "libraryStatus": "reading"})
    );
    assert_eq!(
        engine.calls.total.load(Ordering::SeqCst),
        18,
        "cached request went upstream"
    );
    assert_eq!(
        engine.calls.manga_dex.load(Ordering::SeqCst),
        1,
        "cached canonical pages went upstream"
    );
    assert_eq!(budgets.upstream_available_for_test(), 0);
    assert_eq!(cold.len(), 16);
    assert!(
        cold.try_join_next().is_none(),
        "a cold fetch stopped hanging"
    );

    cold.abort_all();
    while let Some(result) = cold.join_next().await {
        assert!(result.unwrap_err().is_cancelled());
    }
    assert_eq!(
        budgets.upstream_available_for_test(),
        16,
        "cancellation leaked upstream permits"
    );
    assert_eq!(budgets.upstream_waiters_available_for_test(), 16);
    assert_eq!(execute(&app, &pages_query, 1, false).await, warmed_pages);
    assert_eq!(engine.calls.total.load(Ordering::SeqCst), 18);
    reserved.0.close().await;
    pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manga_dex_upstream_permits_cover_response_bodies_and_skip_cached_at_home() {
    let engine = Engine::start_stalled_manga_dex_bodies().await;
    let budgets = Arc::new(request_budget::RequestBudgets::default());
    let client = Arc::new(
        mangadex::MangaDexClient::new("body-admission-test", 1000.0, 60_000.0)
            .with_bases(&engine.url, &engine.url),
    );
    client.set_enabled(true);
    let warmed = budgets
        .scope_upstream("md-body-client-1".into(), client.at_home("warm"))
        .await
        .unwrap();
    assert_eq!(warmed, ["https://body.mangadex.network/data/warm/one.webp"]);
    assert_eq!(engine.calls.manga_dex.load(Ordering::SeqCst), 1);

    let mut cold = tokio::task::JoinSet::new();
    let start = |tasks: &mut tokio::task::JoinSet<()>, id: usize, actor: usize| {
        let client = client.clone();
        let budgets = budgets.clone();
        tasks.spawn(async move {
            let chapter = format!("cold-{id}");
            let result = budgets
                .scope_upstream(format!("md-body-client-{actor}"), client.at_home(&chapter))
                .await;
            panic!("unfinished MangaDex body unexpectedly completed: {result:?}");
        });
    };
    for id in 0..3 {
        start(&mut cold, id, 1);
    }
    wait_for_cold(&engine.calls, 3).await;
    // Let the HTTP clients receive the already-sent headers and JSON prefix;
    // their futures now wait solely for the rest of the response body.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(budgets.upstream_available_for_test(), 13);
    let refused = tokio::time::timeout(
        Duration::from_secs(2),
        budgets.scope_upstream("md-body-client-1".into(), client.at_home("fourth")),
    )
    .await
    .expect("per-client limit queued a fourth MangaDex call")
    .unwrap_err();
    assert!(request_budget::is_upstream_busy(&refused), "{refused}");
    assert_eq!(engine.calls.manga_dex.load(Ordering::SeqCst), 4);

    for id in 3..16 {
        start(&mut cold, id, 2 + (id - 3) / 3);
    }
    wait_for_cold(&engine.calls, 16).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(budgets.upstream_available_for_test(), 0);
    let refused = tokio::time::timeout(
        Duration::from_secs(2),
        budgets.scope_upstream("md-body-client-7".into(), client.at_home("seventeenth")),
    )
    .await
    .expect("global limit queued a seventeenth MangaDex call")
    .unwrap_err();
    assert!(request_budget::is_upstream_busy(&refused), "{refused}");
    assert_eq!(engine.calls.manga_dex.load(Ordering::SeqCst), 17);
    let cached = tokio::time::timeout(
        Duration::from_secs(2),
        budgets.scope_upstream("md-body-client-1".into(), client.at_home("warm")),
    )
    .await
    .expect("cached at-home competed with unfinished response bodies")
    .unwrap();
    assert_eq!(cached, warmed);
    assert_eq!(engine.calls.manga_dex.load(Ordering::SeqCst), 17);
    assert_eq!(budgets.upstream_available_for_test(), 0);
    assert_eq!(cold.len(), 16);
    assert!(cold.try_join_next().is_none());

    cold.abort_all();
    while let Some(result) = cold.join_next().await {
        assert!(result.unwrap_err().is_cancelled());
    }
    assert_eq!(budgets.upstream_available_for_test(), 16);
    assert_eq!(
        budgets
            .scope_upstream("md-body-client-1".into(), client.at_home("warm"))
            .await
            .unwrap(),
        warmed
    );
    assert_eq!(engine.calls.manga_dex.load(Ordering::SeqCst), 17);
}
