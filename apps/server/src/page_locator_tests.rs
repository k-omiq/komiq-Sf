use super::*;
use crate::engine_generation::tests::{chapter_json, manga_json, MockEngine};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Notify;
use tower::ServiceExt;

#[derive(Default)]
struct Pause {
    entered: Notify,
    release: Notify,
}

impl Pause {
    async fn hit(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}

#[derive(Clone, Copy, Default)]
enum RefreshAnswer {
    #[default]
    Success,
    GraphqlError,
    TransportError,
    /// A proxy in front of the engine answers 502 while the engine runs the refresh.
    ProxyError,
}

#[derive(Default)]
struct EngineState {
    swapped: bool,
    answer: RefreshAnswer,
    refresh_pause: Option<Arc<Pause>>,
    page_pause: Option<Arc<Pause>>,
    lookup_pause: Option<Arc<Pause>>,
}

#[derive(Default)]
struct Calls {
    co: AtomicUsize,
    pages: AtomicUsize,
    co_seen: Notify,
}

struct Engine {
    url: String,
    state: Arc<Mutex<EngineState>>,
    calls: Arc<Calls>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn chapter_rows(swapped: bool) -> Vec<Value> {
    [8, 9]
        .into_iter()
        .enumerate()
        .map(|(order, id)| {
            let mut chapter = chapter_json(id, 7);
            chapter["sourceOrder"] = json!(if swapped { 1 - order } else { order });
            chapter
        })
        .collect()
}

fn page_bytes(chapter: u8) -> Vec<u8> {
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        1,
        1,
        image::Rgba([chapter, 0, 0, 255]),
    ));
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

impl Engine {
    async fn spawn() -> Self {
        let state = Arc::new(Mutex::new(EngineState::default()));
        let calls = Arc::new(Calls::default());
        let app = Router::new()
            .route("/api/graphql", post({
                let state = state.clone();
                let calls = calls.clone();
                move |Json(request): Json<Value>| {
                    let state = state.clone();
                    let calls = calls.clone();
                    async move {
                        let query = request["query"].as_str().unwrap();
                        let id = request["variables"]["id"].as_i64().unwrap();
                        let refresh = query.contains("fetchChapters: true");
                        let lookup = query.starts_with("query CO(");
                        let pause = {
                            let mut state = state.lock().unwrap();
                            if refresh { state.refresh_pause.take() }
                            else if lookup { state.lookup_pause.take() }
                            else { None }
                        };
                        if let Some(pause) = pause { pause.hit().await; }
                        let mut state = state.lock().unwrap();
                        if refresh {
                            match state.answer {
                                RefreshAnswer::GraphqlError => return Json(json!({"errors":[{"message":"refresh refused"}]})).into_response(),
                                RefreshAnswer::TransportError => {
                                    state.swapped = !state.swapped;
                                    let body = axum::body::Body::from_stream(futures::stream::once(async {
                                        Err::<Vec<u8>, _>(std::io::Error::other("lost refresh response"))
                                    }));
                                    return axum::response::Response::new(body);
                                }
                                RefreshAnswer::ProxyError => {
                                    return StatusCode::BAD_GATEWAY.into_response();
                                }
                                RefreshAnswer::Success => state.swapped = !state.swapped,
                            }
                        }
                        let chapters = chapter_rows(state.swapped);
                        let data = if query.contains("fetchMangaAndChapters") {
                            json!({"fetchMangaAndChapters":{"manga":manga_json(7),"chapters":chapters}})
                        } else if lookup {
                            calls.co.fetch_add(1, Ordering::SeqCst);
                            calls.co_seen.notify_one();
                            json!({"chapters":{"nodes":chapters.into_iter().filter(|c| c["id"] == id).collect::<Vec<_>>()}})
                        } else if query.starts_with("query CM(") {
                            json!({"chapters":{"nodes":[{"mangaId":7}]}})
                        } else if query.contains("fetchChapterPages") {
                            json!({"fetchChapterPages":{"pages":["/api/v1/manga/7/chapter/0/page/0", "/api/v1/manga/7/chapter/0/page/1"]}})
                        } else {
                            json!({"manga":manga_json(7),"chapters":{"nodes":chapters}})
                        };
                        Json(json!({"data":data})).into_response()
                    }
                }
            }))
            .route("/api/v1/manga/{m}/chapter/{i}/page/{p}", get({
                let state = state.clone();
                let calls = calls.clone();
                move |UrlPath((manga, order, _page)): UrlPath<(i64, usize, usize)>| {
                    let state = state.clone();
                    let calls = calls.clone();
                    async move {
                        assert_eq!(manga, 7);
                        calls.pages.fetch_add(1, Ordering::SeqCst);
                        let pause = state.lock().unwrap().page_pause.take();
                        if let Some(pause) = pause { pause.hit().await; }
                        // Resolve after the pause: a refresh can make the old index
                        // name the neighbour before the engine returns its bytes.
                        let swapped = state.lock().unwrap().swapped;
                        let chapter = if (order == 0) == swapped { 9 } else { 8 };
                        ([(header::CONTENT_TYPE, "image/png")], page_bytes(chapter))
                    }
                }
            }));
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url,
            state,
            calls,
            server,
        }
    }

    fn co(&self) -> usize {
        self.calls.co.load(Ordering::SeqCst)
    }
}

async fn app_state(
    mut suwayomi: SuwayomiClient,
    wait: Duration,
) -> (crate::cover_store::test_support::TestDir, Arc<AppState>) {
    suwayomi.page_locator = Arc::new(page_locator::PageLocator::with_refresh_wait(wait));
    let pool = db::Database::connect("sqlite::memory:")
        .await
        .unwrap()
        .foreground();
    let cover_dir = crate::cover_store::test_support::TestDir::new();
    let covers = crate::cover_store::test_support::store(&cover_dir);
    let app = Arc::new(AppState {
        pool,
        cover_pool: covers,
        mangadex: Arc::new(
            mangadex::MangaDexClient::new("page-locator-test", 100.0, 100.0)
                .with_bases(&suwayomi.abs(Some("/")), &suwayomi.abs(Some("/"))),
        ),
        suwayomi,
        admin_users: vec![],
        scan_health: Mutex::new(ScanHealth::default()),
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
        ext_icons_dir: Default::default(),
        media_manifests: None,
        policy: Default::default(),
        setup_code: instance::SetupCode::default(),
    });
    (cover_dir, app)
}

async fn fixture() -> (
    Engine,
    Arc<AppState>,
    crate::cover_store::test_support::TestDir,
) {
    let engine = Engine::spawn().await;
    let (cover_dir, app) = app_state(
        SuwayomiClient::new(engine.url.clone(), None, None),
        page_locator::REFRESH_WAIT,
    )
    .await;
    (engine, app, cover_dir)
}

fn router(app: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/media/v1/c/{chapterId}/{page}",
            get(serve_chapter_page)
                .layer(axum::middleware::from_fn(not_modified))
                .layer(axum::middleware::from_fn_with_state(
                    Duration::from_secs(60),
                    request_budget::limit_image_time,
                ))
                .layer::<_, std::convert::Infallible>(telemetry::MediaLayer(
                    telemetry::MediaRoute::Page,
                )),
        )
        .route(
            "/media/v1/c/{epoch}/{chapterId}/{page}",
            get(serve_generation_page)
                .layer(axum::middleware::from_fn(not_modified))
                .layer(axum::middleware::from_fn_with_state(
                    Duration::from_secs(60),
                    request_budget::limit_image_time,
                ))
                .layer::<_, std::convert::Infallible>(telemetry::MediaLayer(
                    telemetry::MediaRoute::Page,
                )),
        )
        .route(
            "/api/v1/manga/{*rest}",
            get(
                |State(app): State<Arc<AppState>>, UrlPath(rest): UrlPath<String>| async move {
                    serve_suwayomi_image(
                        State(app.clone()),
                        State(CoverDb(app.cover_pool.clone())),
                        UrlPath(rest),
                        RawQuery(None),
                    )
                    .await
                },
            ),
        )
        .with_state(app)
}

async fn get_page(app: Arc<AppState>, path: &str) -> axum::response::Response {
    router(app)
        .oneshot(
            axum::http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn assert_page(response: axum::response::Response, chapter: u8) {
    assert_page_with_cache(response, chapter, image_delivery::MUTABLE_CACHE).await;
}

async fn assert_page_with_cache(response: axum::response::Response, chapter: u8, cache: &str) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], cache);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), page_bytes(chapter));
}

fn assert_busy(response: &axum::response::Response) {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(response.headers()[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .contains("no-store"));
    assert!(response.headers().contains_key(header::RETRY_AFTER));
}

/// `pages` hands out `/media/v1/c/{epoch}/…`: the same bytes as the old path, kept a day
/// with a strong ETag, and refused, before any fetch, once a new engine generation has
/// raised the epoch and may have reused the chapter id.
#[tokio::test]
async fn generation_page_urls_serve_the_same_bytes_until_the_epoch_moves() {
    let (engine, app, _cover_dir) = fixture().await;
    let old = get_page(app.clone(), "/media/v1/c/8/0").await;
    let etag = old.headers()[header::ETAG].clone();
    assert_eq!(etag, strong_etag(&page_bytes(8)));
    assert_page(old, 8).await;
    let new = get_page(app.clone(), "/media/v1/c/0/8/0").await;
    assert_eq!(new.headers()[header::ETAG], etag);
    assert_page_with_cache(new, 8, image_delivery::GENERATION_PAGE_CACHE).await;
    let revalidated = router(app.clone())
        .oneshot(
            axum::http::Request::builder()
                .uri("/media/v1/c/0/8/0")
                .header(header::IF_NONE_MATCH, etag.clone())
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        revalidated.headers()[header::CACHE_CONTROL],
        image_delivery::GENERATION_PAGE_CACHE
    );
    assert_eq!(revalidated.headers()[header::ETAG], etag);

    // A new engine generation raises the numeric epoch.
    sqlx::query(
        "INSERT INTO engine_numeric_state (singleton, epoch, changed_at) VALUES (true, 5, now())",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    let fetched = engine.calls.pages.load(Ordering::SeqCst);
    for path in [
        "/media/v1/c/0/8/0",
        "/media/v1/c/4/8/0",
        "/media/v1/c/6/8/0",
        "/media/v1/c/x/8/0",
        "/media/v1/c/-5/8/0",
        "/media/v1/c/99999999999999999999/8/0",
    ] {
        let refused = get_page(app.clone(), path).await;
        assert_eq!(refused.status(), StatusCode::NOT_FOUND, "{path}");
        assert!(
            refused.headers()[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("no-store"),
            "{path}"
        );
    }
    assert_eq!(engine.calls.pages.load(Ordering::SeqCst), fetched);
    assert_page_with_cache(
        get_page(app.clone(), "/media/v1/c/5/8/0").await,
        8,
        image_delivery::GENERATION_PAGE_CACHE,
    )
    .await;
    // Old clients' locator URLs keep their short lifetime.
    assert_page(get_page(app, "/media/v1/c/8/0").await, 8).await;
}

#[tokio::test]
async fn warm_locator_survives_reorder_through_both_refresh_methods() {
    for combined in [false, true] {
        let (engine, app, _cover_dir) = fixture().await;
        assert_page(get_page(app.clone(), "/media/v1/c/8/0").await, 8).await;
        assert_page(get_page(app.clone(), "/media/v1/c/8/0").await, 8).await;
        assert_eq!(engine.co(), 1);
        if combined {
            app.suwayomi
                .series_and_chapters_with_provenance(7)
                .await
                .unwrap();
        } else {
            app.suwayomi.chapters_with_provenance(7).await.unwrap();
        }
        assert_page(get_page(app, "/media/v1/c/8/0").await, 8).await;
        assert_eq!(engine.co(), 2);
    }
}

#[tokio::test]
async fn in_flight_page_drops_neighbour_bytes_and_retries() {
    let (engine, app, _cover_dir) = fixture().await;
    let pause = Arc::new(Pause::default());
    engine.state.lock().unwrap().page_pause = Some(pause.clone());
    let page = tokio::spawn(get_page(app.clone(), "/media/v1/c/8/0"));
    pause.entered.notified().await;
    app.suwayomi.chapters_with_provenance(7).await.unwrap();
    pause.release.notify_one();
    assert_page(page.await.unwrap(), 8).await;
    assert_eq!(engine.co(), 2);
    assert_eq!(engine.calls.pages.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn lookup_overlapping_refresh_is_repeated_before_page_fetch() {
    let (engine, app, _cover_dir) = fixture().await;
    let pause = Arc::new(Pause::default());
    engine.state.lock().unwrap().lookup_pause = Some(pause.clone());
    let page = tokio::spawn(get_page(app.clone(), "/media/v1/c/8/0"));
    pause.entered.notified().await;
    app.suwayomi.chapters_with_provenance(7).await.unwrap();
    pause.release.notify_one();
    assert_page(page.await.unwrap(), 8).await;
    assert_eq!(engine.co(), 2);
    assert_eq!(engine.calls.pages.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_chapter_opening_all_pages_at_once_sends_one_cold_lookup() {
    let (engine, app, _cover_dir) = fixture().await;
    let pause = Arc::new(Pause::default());
    engine.state.lock().unwrap().lookup_pause = Some(pause.clone());
    let first = tokio::spawn(get_page(app.clone(), "/media/v1/c/8/0"));
    pause.entered.notified().await;
    let rest: Vec<_> = (0..20)
        .map(|_| tokio::spawn(get_page(app.clone(), "/media/v1/c/8/0")))
        .collect();
    tokio::time::sleep(Duration::from_millis(50)).await;
    pause.release.notify_one();
    assert_page(first.await.unwrap(), 8).await;
    for page in rest {
        assert_page(page.await.unwrap(), 8).await;
    }
    assert_eq!(engine.co(), 1);
}

#[tokio::test]
async fn page_waits_for_refresh_then_succeeds() {
    let (engine, app, _cover_dir) = fixture().await;
    let pause = Arc::new(Pause::default());
    engine.state.lock().unwrap().refresh_pause = Some(pause.clone());
    let client = app.suwayomi.clone();
    let refresh = tokio::spawn(async move { client.chapters_with_provenance(7).await });
    pause.entered.notified().await;
    let page = tokio::spawn(get_page(app.clone(), "/media/v1/c/8/0"));
    engine.calls.co_seen.notified().await;
    assert_eq!(engine.calls.pages.load(Ordering::SeqCst), 0);
    assert!(!page.is_finished());
    pause.release.notify_one();
    refresh.await.unwrap().unwrap();
    assert_page(page.await.unwrap(), 8).await;
}

#[tokio::test]
async fn page_and_legacy_waits_expire_without_fetching_bytes() {
    let engine = Engine::spawn().await;
    let (_cover_dir, app) = app_state(
        SuwayomiClient::new(engine.url.clone(), None, None),
        Duration::from_millis(50),
    )
    .await;
    let pause = Arc::new(Pause::default());
    engine.state.lock().unwrap().refresh_pause = Some(pause.clone());
    let client = app.suwayomi.clone();
    let refresh = tokio::spawn(async move { client.chapters_with_provenance(7).await });
    pause.entered.notified().await;
    assert_busy(&get_page(app.clone(), "/media/v1/c/8/0").await);
    assert_busy(&get_page(app, "/api/v1/manga/7/chapter/0/page/0").await);
    assert_eq!(engine.calls.pages.load(Ordering::SeqCst), 0);
    pause.release.notify_one();
    refresh.await.unwrap().unwrap();
}

#[tokio::test]
async fn transport_failure_disables_memo_but_graphql_failure_does_not() {
    for answer in [
        RefreshAnswer::GraphqlError,
        RefreshAnswer::TransportError,
        RefreshAnswer::ProxyError,
    ] {
        let transport = !matches!(answer, RefreshAnswer::GraphqlError);
        let (engine, app, _cover_dir) = fixture().await;
        assert_page(get_page(app.clone(), "/media/v1/c/8/0").await, 8).await;
        engine.state.lock().unwrap().answer = answer;
        let (_, provenance) = app.suwayomi.chapters_with_provenance(7).await.unwrap();
        assert_eq!(provenance, suwayomi::Provenance::CachedFallback);
        for _ in 0..2 {
            assert_page_with_cache(
                get_page(app.clone(), "/media/v1/c/8/0").await,
                8,
                if transport {
                    "no-store"
                } else {
                    image_delivery::MUTABLE_CACHE
                },
            )
            .await;
        }
        assert_eq!(engine.co(), if transport { 3 } else { 2 });
        if transport {
            // No socket operation runs while time is paused.
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::time::resume();
            for _ in 0..2 {
                assert_page(get_page(app.clone(), "/media/v1/c/8/0").await, 8).await;
            }
            assert_eq!(engine.co(), 4);
        }
    }
}

#[tokio::test]
async fn unknown_outcome_refresh_late_reorder_is_never_cached() {
    let (engine, app, _cover_dir) = fixture().await;
    assert_page(get_page(app.clone(), "/media/v1/c/8/0").await, 8).await;
    engine.state.lock().unwrap().answer = RefreshAnswer::ProxyError;
    let (_, provenance) = app.suwayomi.chapters_with_provenance(7).await.unwrap();
    assert_eq!(provenance, suwayomi::Provenance::CachedFallback);
    let pause = Arc::new(Pause::default());
    engine.state.lock().unwrap().page_pause = Some(pause.clone());
    let page = tokio::spawn(get_page(app.clone(), "/media/v1/c/8/0"));
    pause.entered.notified().await;
    // The proxy has answered, but the engine finishes reordering after the lookup.
    engine.state.lock().unwrap().swapped = true;
    pause.release.notify_one();
    assert_page_with_cache(page.await.unwrap(), 9, "no-store").await;
    assert_page_with_cache(get_page(app, "/media/v1/c/8/0").await, 8, "no-store").await;
    assert_eq!(engine.co(), 3);
}

#[tokio::test]
async fn path_validation_unknown_chapters_and_legacy_index_path() {
    let (engine, app, _cover_dir) = fixture().await;
    for path in [
        "/media/v1/c/a/0",
        "/media/v1/c/8/a",
        "/media/v1/c/-8/0",
        "/media/v1/c/8/%2B1",
        "/media/v1/c/%EF%BC%98/0",
        "/media/v1/c/%FF/0",
        "/media/v1/c/8/%FF",
        "/media/v1/c/9999999999999999999999/0",
        "/media/v1/c/999/0",
    ] {
        let response = get_page(app.clone(), path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("no-store"));
    }
    assert_eq!(engine.calls.pages.load(Ordering::SeqCst), 0);
    assert_page(get_page(app, "/api/v1/manga/7/chapter/0/page/0").await, 8).await;
}

#[tokio::test]
async fn pages_use_locator_only_for_active_generation_keys() {
    let engine = Engine::spawn().await;
    // The Docker image's setting: image URLs are relative to this server.
    let (_cover_dir, app) = app_state(
        SuwayomiClient::new(engine.url.clone(), Some("/".into()), None),
        page_locator::REFRESH_WAIT,
    )
    .await;
    let generation = engine_keys::adopt(&app.pool).await.unwrap();
    sqlx::query("INSERT INTO engine_chapter_key(generation,chapter_id,manga_id,source_order,chapter_key,updated_at) VALUES($1,8,7,0,'/chapter/8',now())")
        .bind(generation).execute(&app.pool).await.unwrap();
    let schema = graphql::build_schema(app.clone(), false);
    for mode in ["active", "detached", "missing", "unreadable"] {
        match mode {
            "detached" => {
                sqlx::query("UPDATE engine_generation SET state='detached' WHERE id=$1")
                    .bind(generation)
                    .execute(&app.pool)
                    .await
                    .unwrap();
            }
            "missing" => {
                sqlx::raw_sql(
                    "DELETE FROM engine_chapter_key; UPDATE engine_generation SET state='active'",
                )
                .execute(&app.pool)
                .await
                .unwrap();
            }
            "unreadable" => {
                sqlx::query("ALTER TABLE engine_chapter_key RENAME TO unavailable_chapter_key")
                    .execute(&app.pool)
                    .await
                    .unwrap();
            }
            _ => {}
        }
        let result = schema
            .execute("{ pages(chapterId: \"8\") { index sourceUrl } }")
            .await;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let data = result.data.into_json().unwrap();
        assert_eq!(data["pages"].as_array().unwrap().len(), 2);
        for index in 0..2 {
            let path = if mode == "active" {
                format!("/media/v1/c/0/8/{index}")
            } else {
                format!("/api/v1/manga/7/chapter/0/page/{index}")
            };
            assert_eq!(data["pages"][index]["sourceUrl"], path);
        }
    }
}

#[tokio::test]
async fn pages_keep_engine_urls_when_images_point_at_the_engine() {
    let engine = Engine::spawn().await;
    // The desktop app calls the engine with a login and names it without one.
    let with_login = engine.url.replacen("http://", "http://user:secret@", 1);
    for (url, public) in [
        (engine.url.clone(), None),
        (engine.url.clone(), Some(engine.url.clone())),
        (with_login, Some(format!("{}/", engine.url))),
    ] {
        let (_cover_dir, app) = app_state(
            SuwayomiClient::new(url, public, None),
            page_locator::REFRESH_WAIT,
        )
        .await;
        let generation = engine_keys::adopt(&app.pool).await.unwrap();
        sqlx::query("INSERT INTO engine_chapter_key(generation,chapter_id,manga_id,source_order,chapter_key,updated_at) VALUES($1,8,7,0,'/chapter/8',now())")
            .bind(generation).execute(&app.pool).await.unwrap();
        let result = graphql::build_schema(app.clone(), false)
            .execute("{ pages(chapterId: \"8\") { sourceUrl } }")
            .await;
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        // The engine has no /media/v1 route: a locator URL there would be a 404.
        assert_eq!(
            result.data.into_json().unwrap()["pages"][0]["sourceUrl"],
            format!("{}/api/v1/manga/7/chapter/0/page/0", engine.url)
        );
    }
}

/// A `SUWAYOMI_PUBLIC_URL` naming another public Suwayomi host must not carry the
/// locator: that host has no `/media/v1` route. The locator is relative to this server,
/// whatever origin the owner media route names (a web reader would send a foreign
/// owner origin through the image Worker, which refuses `/media/`).
#[tokio::test]
async fn locator_urls_never_use_a_public_engine_host() {
    let engine = Engine::spawn().await;
    let (_cover_dir, app) = app_state(
        SuwayomiClient::new(
            engine.url.clone(),
            Some("https://suwayomi.example".into()),
            None,
        ),
        page_locator::REFRESH_WAIT,
    )
    .await;
    let generation = engine_keys::adopt(&app.pool).await.unwrap();
    sqlx::query("INSERT INTO engine_chapter_key(generation,chapter_id,manga_id,source_order,chapter_key,updated_at) VALUES($1,8,7,0,'/chapter/8',now())")
        .bind(generation).execute(&app.pool).await.unwrap();
    // The URL names the engine's current numeric epoch.
    sqlx::query(
        "INSERT INTO engine_numeric_state(singleton,epoch,changed_at) VALUES(true,5,now())",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    let result = graphql::build_schema(app.clone(), false)
        .execute("{ pages(chapterId: \"8\") { sourceUrl } }")
        .await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(
        result.data.into_json().unwrap()["pages"][1]["sourceUrl"],
        "/media/v1/c/5/8/1"
    );
    let owner = media_routes::MediaRoutes::parse(Some(
        r#"[{"id":"main","kind":"owner","base":"https://komiq.example/"}]"#,
    ))
    .unwrap();
    let result = graphql::build_schema(app.clone(), false)
        .execute(
            async_graphql::Request::new("{ pages(chapterId: \"8\") { sourceUrl } }").data(owner),
        )
        .await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert_eq!(
        result.data.into_json().unwrap()["pages"][1]["sourceUrl"],
        "/media/v1/c/5/8/1"
    );
}

#[tokio::test]
async fn block_rules_refuse_both_page_paths_and_fail_closed_on_read_errors() {
    let (engine, app, _cover_dir) = fixture().await;
    sqlx::raw_sql("INSERT INTO work(id,created_at,updated_at) VALUES('w_blocked',now(),now()); INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('ss_blocked','w_blocked','suwayomi','source','7',now())")
        .execute(&app.pool).await.unwrap();
    let rule = block::BlockRule::new(Vec::<String>::new(), ["safe"]).unwrap();
    block::replace(&app.pool, block::Scope::Site, &[rule])
        .await
        .unwrap();
    for path in ["/media/v1/c/8/0", "/api/v1/manga/7/chapter/0/page/0"] {
        let response = get_page(app.clone(), path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("no-store"));
    }
    app.pool.close().await;
    // The denied set is held in memory; a re-read that fails refuses the page.
    app.policy.invalidate();
    for path in ["/media/v1/c/8/0", "/api/v1/manga/7/chapter/0/page/0"] {
        assert_busy(&get_page(app.clone(), path).await);
    }
    assert_eq!(engine.calls.pages.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn quarantine_refuses_locator_without_any_page_fetch() {
    let engine = MockEngine::spawn().await;
    let (_cover_dir, mut app) = app_state(
        SuwayomiClient::new(engine.url.clone(), None, None),
        page_locator::REFRESH_WAIT,
    )
    .await;
    let state = Arc::get_mut(&mut app).unwrap();
    let fence =
        engine_generation::Fence::attach(&state.pool, &state.cover_pool, &mut state.suwayomi)
            .await
            .unwrap();
    let (manga, chapters) = state.suwayomi.series_and_chapters(7).await.unwrap();
    series_cache::put_series(&state.pool, &manga).await.unwrap();
    series_cache::put_chapters_in_generation(
        &state.pool,
        7,
        &chapters,
        engine_keys::token(manga.engine_generation.as_deref()).unwrap(),
    )
    .await
    .unwrap();
    engine.data.lock().await.mangas[0]["sourceId"] = json!("another-source");
    fence.check().await;
    assert_eq!(
        engine_generation::states(&state.pool).await.unwrap()[0].state,
        "quarantined"
    );
    // Point the transport at a counting page engine: quarantine must stop before it.
    let page_engine = Engine::spawn().await;
    state.suwayomi.base_url_for_test(&page_engine.url);
    assert_busy(&get_page(app, "/media/v1/c/8/0").await);
    assert_eq!(page_engine.calls.pages.load(Ordering::SeqCst), 0);
    assert_eq!(page_engine.co(), 0);
}
