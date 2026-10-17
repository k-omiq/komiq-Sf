use super::*;

pub(crate) struct TempDir(pub std::path::PathBuf);
impl TempDir {
    pub(crate) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("komika-media-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn key_file_manifests_off_never_touches_key_path() {
    let dir = TempDir::new();
    let mut config = Config::from_env();
    config.media_manifests = false;
    config.media_signing_key_file = dir.0.join("absent.key");
    assert!(media_manifest::MediaState::from_config(&config)
        .unwrap()
        .is_none());
    assert!(!config.media_signing_key_file.exists());
}

use crate::media_manifest::{ingest::ingest_chapter, store};
use crate::media_verify::{verify_key_set, verify_manifest, PinnedKeys};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::Duration;
use tower::ServiceExt;

struct Engine {
    url: String,
    pages: Arc<Mutex<Vec<Vec<u8>>>>,
    /// The chapter key the engine answers for chapter 8 (the stored one is `/chapter`).
    chapter_url: Arc<Mutex<String>>,
    /// When set, the engine moves chapter 8 to position 1 once it served a page,
    /// as the engine's own updater could in the middle of an ingest pass.
    reorder_on_page: Arc<std::sync::atomic::AtomicBool>,
    page_pause: Arc<Mutex<Option<crate::locks::pause_points::PausePoints>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.server.abort();
    }
}
fn image_bytes(value: u8) -> Vec<u8> {
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        2,
        3,
        image::Rgba([value, 0, 0, 255]),
    ));
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}
impl Engine {
    async fn spawn() -> Self {
        let pages = Arc::new(Mutex::new(vec![image_bytes(1), image_bytes(2)]));
        let chapter_url = Arc::new(Mutex::new("/chapter".to_string()));
        let reorder_on_page = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let page_pause: Arc<Mutex<Option<crate::locks::pause_points::PausePoints>>> =
            Default::default();
        let order = Arc::new(std::sync::atomic::AtomicI32::new(0));
        let routes = Router::new()
            .route("/api/graphql", post({
                let pages = pages.clone();
                let chapter_url = chapter_url.clone();
                let order = order.clone();
                move |Json(request): Json<Value>| {
                    let pages = pages.clone();
                    let chapter_url = chapter_url.clone();
                    let order = order.clone();
                    async move {
                        let query = request["query"].as_str().unwrap();
                        let data = if query.contains("fetchChapterPages") {
                            let count = pages.lock().unwrap().len();
                            json!({"fetchChapterPages":{"pages":(0..count).map(|idx| format!("/api/v1/manga/7/chapter/0/page/{idx}")).collect::<Vec<_>>()}})
                        } else if query.starts_with("query CO(") {
                            let url = chapter_url.lock().unwrap().clone();
                            let order = order.load(std::sync::atomic::Ordering::SeqCst);
                            json!({"chapters":{"nodes":[{"mangaId":7,"sourceOrder":order,"url":url,"realUrl":null,
                                "manga":{"url":"/manga","realUrl":null,"title":"Fixture","sourceId":"source"}}]}})
                        } else if query.starts_with("query CM(") {
                            json!({"chapters":{"nodes":[{"mangaId":7}]}})
                        } else {
                            panic!("unexpected mock engine query: {query}");
                        };
                        Json(json!({"data":data}))
                    }
                }
            }))
            .route("/api/v1/manga/{m}/chapter/{c}/page/{p}", get({
                let pages = pages.clone();
                let reorder_on_page = reorder_on_page.clone();
                let order = order.clone();
                let page_pause = page_pause.clone();
                move |UrlPath((m,c,p)): UrlPath<(i64,i32,usize)>| {
                    let pages = pages.clone();
                    let reorder_on_page = reorder_on_page.clone();
                    let order = order.clone();
                    let page_pause = page_pause.clone();
                    async move {
                        assert_eq!((m,c), (7,0));
                        let pause = page_pause.lock().unwrap().take();
                        if let Some(points) = pause { points.hit("page-fetch").await; }
                        if reorder_on_page.load(std::sync::atomic::Ordering::SeqCst) {
                            order.store(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        match pages.lock().unwrap().get(p).cloned() {
                            Some(bytes) => ([(header::CONTENT_TYPE,"image/png")], bytes).into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                }
            }));
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        Self {
            url,
            pages,
            chapter_url,
            reorder_on_page,
            page_pause,
            server,
        }
    }
}

async fn fixture(
    enabled: bool,
) -> (
    TempDir,
    Engine,
    Arc<AppState>,
    crate::cover_store::test_support::TestDir,
) {
    let cover_dir = crate::cover_store::test_support::TestDir::new();
    let dir = TempDir::new();
    let engine = Engine::spawn().await;
    let pool = db::Database::connect("sqlite::memory:")
        .await
        .unwrap()
        .foreground();
    let generation = engine_keys::adopt(&pool).await.unwrap();
    sqlx::query("INSERT INTO engine_manga_key(generation,manga_id,source_id,manga_key,updated_at) VALUES($1,7,'source','/manga',now())")
        .bind(generation).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO engine_chapter_key(generation,chapter_id,manga_id,source_order,chapter_key,updated_at) VALUES($1,8,7,0,'/chapter',now())")
        .bind(generation).execute(&pool).await.unwrap();
    let mut config = Config::from_env();
    config.media_manifests = enabled;
    config.media_signing_key_file = dir.0.join("signing.key");
    let app = Arc::new(AppState {
        pool,
        cover_pool: crate::cover_store::test_support::store(&cover_dir),
        suwayomi: SuwayomiClient::new(engine.url.clone(), None, None),
        mangadex: Arc::new(
            mangadex::MangaDexClient::new("media-manifest-test", 100.0, 100.0)
                .with_bases(&engine.url, &engine.url),
        ),
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
        setup_code: instance::SetupCode::default(),
        policy: media_policy::Policy::new(enabled),
        media_manifests: media_manifest::MediaState::from_config(&config).unwrap(),
    });
    (dir, engine, app, cover_dir)
}

fn router(app: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/media/v1/o/{copy_id}/{sha256}",
            get(serve_media_object)
                .layer(axum::middleware::from_fn_with_state(
                    Duration::from_secs(60),
                    request_budget::limit_image_time,
                ))
                .layer::<_, std::convert::Infallible>(telemetry::MediaLayer(
                    telemetry::MediaRoute::Object,
                )),
        )
        .route(
            "/media/v1/manifest/{copy_id}",
            get(serve_media_manifest)
                .layer(axum::middleware::from_fn_with_state(
                    Duration::from_secs(10),
                    request_budget::limit_image_time,
                ))
                .layer::<_, std::convert::Infallible>(telemetry::MediaLayer(
                    telemetry::MediaRoute::Manifest,
                )),
        )
        .route(
            "/media/v1/keys",
            get(serve_media_keys)
                .layer(axum::middleware::from_fn_with_state(
                    Duration::from_secs(10),
                    request_budget::limit_image_time,
                ))
                .layer::<_, std::convert::Infallible>(telemetry::MediaLayer(
                    telemetry::MediaRoute::Policy,
                )),
        )
        .route("/media/v1/ready", get(serve_media_ready))
        .with_state(app)
}
async fn get_media(app: &Arc<AppState>, path: &str) -> axum::response::Response {
    tokio::time::timeout(
        Duration::from_secs(3),
        router(app.clone()).oneshot(
            axum::http::Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, "ignored")
                .header(header::COOKIE, "ignored=yes")
                .body(axum::body::Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap()
}
async fn body(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec()
}
async fn pages(app: &Arc<AppState>) -> Value {
    let result = graphql::build_schema(app.clone(),false)
        .execute("{ pages(chapterId: \"8\") { index sourceUrl mediaPath mediaSha256 mediaLength mediaMime } }")
        .await;
    assert!(result.errors.is_empty(), "{:?}", result.errors);
    result.data.into_json().unwrap()["pages"].clone()
}
fn pins(app: &AppState) -> PinnedKeys {
    let entry = app.media_manifests.as_ref().unwrap().signer.public_entry();
    PinnedKeys::new([(entry.kid, entry.public_key)]).unwrap()
}
async fn assert_hashes(app: &AppState, copy: &str, rev: i32, expected: &[Vec<u8>]) {
    let manifest = store::revision(&app.pool, copy, rev)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        manifest
            .body
            .pages
            .iter()
            .map(|p| p.sha256.clone())
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|p| hex::encode(Sha256::digest(p)))
            .collect::<Vec<_>>()
    );
    assert!(manifest
        .body
        .pages
        .iter()
        .all(|p| p.width == Some(2) && p.height == Some(3)));
}

async fn insert_orphan_object(app: &AppState, bytes: &[u8]) -> Vec<u8> {
    let hash = Sha256::digest(bytes).to_vec();
    sqlx::query(
        "INSERT INTO media_object (sha256,byte_length,mime,width,height,first_seen) \
         VALUES ($1,$2,'image/png',2,3,now())",
    )
    .bind(&hash)
    .bind(bytes.len() as i64)
    .execute(&app.pool)
    .await
    .unwrap();
    hash
}

#[tokio::test]
async fn source_order_art_revisions_and_unchanged_ingest() {
    let (_dir, engine, app, _cover_dir) = fixture(true).await;
    let mut copy = String::new();
    for rev in 1..=4 {
        {
            let mut bytes = engine.pages.lock().unwrap();
            match rev {
                2 => bytes[0] = image_bytes(3),
                3 => bytes.swap(0, 1),
                4 => bytes.push(image_bytes(4)),
                _ => {}
            }
        }
        app.suwayomi.clear_numeric_memos();
        assert_eq!(ingest_chapter(&app, 8).await.unwrap(), Some(rev));
        copy = store::existing_copy_for_chapter(&app.pool, 8)
            .await
            .unwrap()
            .unwrap();
        let expected = engine.pages.lock().unwrap().clone();
        assert_hashes(&app, &copy, rev, &expected).await;
    }
    let before: Vec<(String, String)> =
        sqlx::query_as("SELECT xmin::text,created_at::text FROM media_manifest ORDER BY revision")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert_eq!(ingest_chapter(&app, 8).await.unwrap(), None);
    let after: Vec<(String, String)> =
        sqlx::query_as("SELECT xmin::text,created_at::text FROM media_manifest ORDER BY revision")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    store::mark_stale(&app.pool, &copy, 4).await.unwrap();
    assert_eq!(ingest_chapter(&app, 8).await.unwrap(), Some(5));
    assert!(
        !store::latest(&app.pool, &copy)
            .await
            .unwrap()
            .unwrap()
            .stale
    );
}

#[tokio::test]
async fn object_mismatch_marks_stale_and_pages_stop_exact_paths() {
    let (_dir, engine, app, _cover_dir) = fixture(true).await;
    // Opening a chapter before its first ingest hands out no exact paths and writes no row.
    let before = pages(&app).await;
    assert!(before[0]["mediaPath"].is_null());
    let copies: i64 = sqlx::query_scalar("SELECT count(*) FROM media_copy")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(copies, 0);
    assert_eq!(ingest_chapter(&app, 8).await.unwrap(), Some(1));
    let listed = pages(&app).await;
    let path = listed[0]["mediaPath"].as_str().unwrap();
    let copy = path.split('/').nth(4).unwrap();
    assert_eq!(listed[0]["mediaMime"], "image/png");
    let response = get_media(&app, path).await;
    assert_eq!(response.status(), StatusCode::OK);
    // s-maxage is the policy lease's remaining life (S5).
    let cache = response.headers()[header::CACHE_CONTROL].to_str().unwrap();
    let s_maxage: i64 = cache
        .strip_prefix("public, max-age=31536000, immutable, s-maxage=")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        (1..=media_policy::LEASE_SECS).contains(&s_maxage),
        "{cache}"
    );
    assert_eq!(body(response).await, engine.pages.lock().unwrap()[0]);
    engine.pages.lock().unwrap()[0] = image_bytes(9);
    let response = get_media(&app, path).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(response.headers()[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .contains("no-store"));
    assert!(store::latest(&app.pool, copy).await.unwrap().unwrap().stale);
    assert!(pages(&app)
        .await
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["mediaPath"].is_null()));
    assert_eq!(
        get_media(&app, &format!("/media/v1/manifest/{copy}"))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let signed = body(get_media(&app, &format!("/media/v1/manifest/{copy}?rev=1")).await).await;
    assert_eq!(verify_manifest(&signed, &pins(&app)).unwrap().revision, 1);
}

#[tokio::test]
async fn object_fill_rechecks_copy_and_work_takedowns_after_fetch() {
    for kind in [media_policy::Kind::Copy, media_policy::Kind::Work] {
        let (_dir, engine, app, _cover_dir) = fixture(true).await;
        sqlx::raw_sql("INSERT INTO work(id,created_at,updated_at) VALUES('w_fill',now(),now()); INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('ss_fill','w_fill','suwayomi','source','7',now())")
            .execute(&app.pool).await.unwrap();
        ingest_chapter(&app, 8).await.unwrap();
        let listed = pages(&app).await;
        let path = listed[0]["mediaPath"].as_str().unwrap().to_string();
        let copy = path.split('/').nth(4).unwrap();
        let points = crate::locks::pause_points::PausePoints::default();
        let fetched = points.arm("page-fetch");
        *engine.page_pause.lock().unwrap() = Some(points);
        let request = tokio::spawn({
            let app = app.clone();
            let path = path.clone();
            async move { get_media(&app, &path).await }
        });
        fetched.reached().await;
        let mut tx = app.pool.begin().await.unwrap();
        let subject = if kind == media_policy::Kind::Copy {
            copy
        } else {
            "w_fill"
        };
        media_policy::add(&mut tx, kind, subject, "during object fill", None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        app.policy.invalidate();
        fetched.release();
        let response = request.await.unwrap();
        let denied = get_media(&app, &path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{kind:?}");
        assert_eq!(response.status(), denied.status());
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            denied.headers()[header::CACHE_CONTROL]
        );
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("no-store"));
        assert_eq!(body(response).await, body(denied).await);
        assert!(!store::latest(&app.pool, copy).await.unwrap().unwrap().stale);
    }
}

#[tokio::test]
async fn signed_manifest_keys_superseded_records_and_pruning() {
    let (_dir, engine, app, _cover_dir) = fixture(true).await;
    ingest_chapter(&app, 8).await.unwrap();
    let copy = store::existing_copy_for_chapter(&app.pool, 8)
        .await
        .unwrap()
        .unwrap();
    let path = format!("/media/v1/manifest/{copy}");
    let response = get_media(&app, &path).await;
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=60"
    );
    let first = body(response).await;
    assert_eq!(verify_manifest(&first, &pins(&app)).unwrap().revision, 1);
    assert_eq!(body(get_media(&app, &path).await).await, first);
    let keyset = body(get_media(&app, "/media/v1/keys").await).await;
    assert_eq!(verify_key_set(&keyset, &pins(&app)).unwrap().len(), 1);
    engine.pages.lock().unwrap()[0] = image_bytes(5);
    ingest_chapter(&app, 8).await.unwrap();
    assert_eq!(
        body(get_media(&app, &format!("{path}?rev=1")).await).await,
        first
    );
    for query in ["rev=0", "rev=-1", "rev=no", "rev=2147483648", "rev=999"] {
        assert_eq!(
            get_media(&app, &format!("{path}?{query}")).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    sqlx::query("UPDATE media_manifest SET superseded_at=now()-interval '8 days' WHERE copy_id=$1 AND revision=1")
        .bind(&copy).execute(&app.pool).await.unwrap();
    assert_eq!(
        get_media(&app, &format!("{path}?rev=1")).await.status(),
        StatusCode::NOT_FOUND
    );
    store::prune(&app.pool).await.unwrap();
    assert!(store::revision(&app.pool, &copy, 1)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store::latest(&app.pool, &copy)
            .await
            .unwrap()
            .unwrap()
            .body
            .revision,
        2
    );
    let orphans: i64 = sqlx::query_scalar("SELECT count(*) FROM media_object o WHERE NOT EXISTS(SELECT 1 FROM media_manifest_page p WHERE p.sha256=o.sha256)")
        .fetch_one(&app.pool).await.unwrap();
    assert_eq!(orphans, 0);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM media_object")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn publish_retries_when_prune_removes_an_object_before_key_share() {
    let (_dir, engine, app, _cover_dir) = fixture(true).await;
    let expected = {
        let mut pages = engine.pages.lock().unwrap();
        let repeated = pages[0].clone();
        pages.push(repeated);
        pages.clone()
    };
    let hash = insert_orphan_object(&app, &expected[0]).await;
    let points = crate::locks::pause_points::PausePoints::default();
    let inserted = points.arm("media-objects-inserted");
    let publish = tokio::spawn(crate::catalog::guards::TEST_PAUSE.scope(points, {
        let app = app.clone();
        async move { db::test_counters::scope(ingest_chapter(&app, 8)).await }
    }));
    tokio::time::timeout(Duration::from_secs(5), inserted.reached())
        .await
        .expect("publish did not insert its objects");
    tokio::time::timeout(Duration::from_secs(5), store::prune(&app.pool))
        .await
        .expect("prune did not finish while publish was paused")
        .unwrap();
    let present: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM media_object WHERE sha256=$1)")
            .bind(&hash)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(!present, "prune did not remove the orphan");
    inserted.release();
    let (result, seen) = tokio::time::timeout(Duration::from_secs(15), publish)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap(), Some(1));
    assert_eq!(seen.database.transaction_restarts, 1);
    assert_eq!(seen.database.retries, 1);
    let copy = store::existing_copy_for_chapter(&app.pool, 8)
        .await
        .unwrap()
        .unwrap();
    assert_hashes(&app, &copy, 1, &expected).await;
    let rows: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM media_copy),(SELECT count(*) FROM media_manifest),\
         (SELECT count(*) FROM media_manifest_page)",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(rows, (1, 1, expected.len() as i64));
    let present: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM media_object WHERE sha256=$1)")
            .bind(&hash)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(present, "retry did not restore the object");
}

#[tokio::test]
async fn prune_skips_an_object_held_by_publish_key_share() {
    let (_dir, engine, app, _cover_dir) = fixture(true).await;
    let expected = engine.pages.lock().unwrap().clone();
    let hash = insert_orphan_object(&app, &expected[0]).await;
    insert_orphan_object(&app, &image_bytes(9)).await;
    let points = crate::locks::pause_points::PausePoints::default();
    let locked = points.arm("media-objects-locked");
    let publish = tokio::spawn(crate::catalog::guards::TEST_PAUSE.scope(points, {
        let app = app.clone();
        async move { ingest_chapter(&app, 8).await }
    }));
    tokio::time::timeout(Duration::from_secs(5), locked.reached())
        .await
        .expect("publish did not lock its objects");
    tokio::time::timeout(Duration::from_secs(5), store::prune(&app.pool))
        .await
        .expect("prune did not skip the locked object")
        .unwrap();
    // The published pages are still uncommitted; only the locked orphan survives.
    let objects: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT sha256 FROM media_object ORDER BY sha256")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert_eq!(objects, vec![hash.clone()]);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM media_manifest_page")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    locked.release();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(15), publish)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Some(1)
    );
    let copy = store::existing_copy_for_chapter(&app.pool, 8)
        .await
        .unwrap()
        .unwrap();
    assert_hashes(&app, &copy, 1, &expected).await;
    let present: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM media_object WHERE sha256=$1)")
            .bind(&hash)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert!(present);
}

#[tokio::test]
async fn manifests_off_pages_and_routes_do_not_touch_media_tables() {
    let (dir, _engine, app, _cover_dir) = fixture(false).await;
    sqlx::raw_sql("DROP TABLE media_manifest_page; DROP TABLE media_manifest; DROP TABLE media_object; DROP TABLE media_copy;")
        .execute(&app.pool).await.unwrap();
    assert!(pages(&app)
        .await
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["mediaPath"].is_null()
            && p["mediaSha256"].is_null()
            && p["mediaLength"].is_null()
            && p["mediaMime"].is_null()));
    assert!(!dir.0.join("signing.key").exists());
    for path in [
        "/media/v1/o/mc_invalid/bad",
        "/media/v1/manifest/mc_invalid",
        "/media/v1/keys",
    ] {
        let response = get_media(&app, path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers()[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("no-store"));
    }
    let response = get_media(&app, "/media/v1/ready").await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
}

#[tokio::test]
async fn media_ready_needs_no_state_or_database() {
    let response = Router::new()
        .route("/media/v1/ready", get(serve_media_ready))
        .oneshot(
            axum::http::Request::builder()
                .uri("/media/v1/ready")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
}

#[test]
fn key_file_config_reads_only_the_named_settings() {
    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
    let _env = EnvGuard(
        ["KOMIKA_MEDIA_MANIFESTS", "KOMIKA_MEDIA_SIGNING_KEY_FILE"]
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect(),
    );
    let dir = TempDir::new();
    let path = dir.0.join("absent.key");
    std::env::set_var("KOMIKA_MEDIA_SIGNING_KEY_FILE", &path);
    for flag in [None, Some("off"), Some("ON"), Some("")] {
        match flag {
            Some(flag) => std::env::set_var("KOMIKA_MEDIA_MANIFESTS", flag),
            None => std::env::remove_var("KOMIKA_MEDIA_MANIFESTS"),
        }
        let config = Config::from_env();
        assert!(!config.media_manifests);
        assert_eq!(config.media_signing_key_file, path);
        assert!(media_manifest::MediaState::from_config(&config)
            .unwrap()
            .is_none());
        assert!(!path.exists());
    }
    std::env::set_var("KOMIKA_MEDIA_MANIFESTS", "on");
    assert!(Config::from_env().media_manifests);
    std::env::remove_var("KOMIKA_MEDIA_SIGNING_KEY_FILE");
    assert_eq!(
        Config::from_env().media_signing_key_file,
        std::path::Path::new("media-signing.key")
    );
}

#[tokio::test]
async fn ingest_refuses_a_chapter_id_the_engine_now_answers_with_other_keys() {
    let (_dir, engine, app, _cover_dir) = fixture(true).await;
    // An engine renumbered without a detected change: chapter 8 is now another chapter.
    *engine.chapter_url.lock().unwrap() = "/another-chapter".into();
    let error = ingest_chapter(&app, 8).await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<crate::engine_generation::FenceError>(),
        Some(crate::engine_generation::FenceError::BindingMismatch { .. })
    ));
    // An abandoned pass writes nothing, not even the copy row.
    let rows: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM media_copy),(SELECT count(*) FROM media_manifest),\
         (SELECT count(*) FROM media_object)",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(rows, (0, 0, 0));
}

#[tokio::test]
async fn publish_refuses_a_pass_collected_under_another_generation() {
    let (_dir, _engine, app, _cover_dir) = fixture(true).await;
    let (identity, _) = store::identity_for_chapter(&app.pool, 8)
        .await
        .unwrap()
        .unwrap();
    let page = store::CollectedPage {
        sha256: Sha256::digest(b"page").to_vec(),
        length: 4,
        mime: "image/png".into(),
        width: None,
        height: None,
    };
    let other = Some(uuid::Uuid::new_v4());
    assert!(store::publish(&app.pool, other, &identity, &[page])
        .await
        .is_err());
    let copies: i64 = sqlx::query_scalar("SELECT count(*) FROM media_copy")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(copies, 0);
}

#[tokio::test]
async fn ingest_publishes_nothing_when_the_engine_reorders_during_the_pass() {
    let (_dir, engine, app, _cover_dir) = fixture(true).await;
    engine
        .reorder_on_page
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = ingest_chapter(&app, 8).await.unwrap_err();
    assert_eq!(error.to_string(), "chapter moved during ingest");
    let manifests: i64 = sqlx::query_scalar("SELECT count(*) FROM media_manifest")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(manifests, 0);
}
