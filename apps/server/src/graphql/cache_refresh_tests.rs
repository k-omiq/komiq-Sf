use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

async fn fixture() -> (AppState, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let pool = crate::database::TestPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations_pg")
        .run(pool.write())
        .await
        .unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let router = axum::Router::new().route(
        "/api/graphql",
        axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(400)).await;
                let id = body["variables"]["id"].as_i64().unwrap();
                axum::Json(serde_json::json!({"data": {"fetchMangaAndChapters": {
                    "manga": {"id": id, "title": "Refreshed", "status": "ONGOING",
                        "inLibrary": true, "sourceId": "src", "genre": []},
                    "chapters": [{"id": id * 10, "mangaId": id, "name": "New chapter",
                        "chapterNumber": 2.0, "isRead": false, "isBookmarked": false,
                        "isDownloaded": false, "lastPageRead": 0, "pageCount": 12}]
                }}}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let state = AppState {
        pool: pool.clone(),
        cover_pool: crate::database::SqliteDatabase::connect("sqlite::memory:")
            .await
            .unwrap(),
        suwayomi: SuwayomiClient::new(format!("http://{addr}"), None, None),
        mangadex: Arc::new(crate::mangadex::MangaDexClient::new("test", 5.0, 40.0)),
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
    };
    (state, requests, server)
}

#[tokio::test]
async fn stale_metadata_and_chapters_return_before_refresh_and_do_not_stampede() {
    let (state, requests, server) = fixture().await;
    let state = Arc::new(state);
    let id = 88001;
    sqlx::query("INSERT INTO suwayomi_series (id, title, status, source_id, updated_at, series_fetched_at) VALUES ($1, 'Cached', 'ONGOING', 'src', '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')")
        .bind(id).execute(&state.pool).await.unwrap();
    sqlx::query("INSERT INTO suwayomi_chapter (id, manga_id, name, chapter_number, updated_at) VALUES ($1, $2, 'Cached chapter', 1, '2020-01-01T00:00:00Z')")
        .bind(id * 10).bind(id).execute(&state.pool).await.unwrap();
    // Each source takes 400ms. Cached reads, including repeated navigations while
    // refreshing, must complete inside 100ms and return the existing data.
    for _ in 0..5 {
        let manga = tokio::time::timeout(
            Duration::from_millis(100),
            resolve_series_cached(&state, id),
        )
        .await
        .expect("metadata waited for source")
        .unwrap();
        let chapters = tokio::time::timeout(
            Duration::from_millis(100),
            resolve_chapters_cached(&state, id),
        )
        .await
        .expect("chapters waited for source")
        .unwrap();
        assert_eq!(manga.title, "Cached");
        assert_eq!(chapters[0].name, "Cached chapter");
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let m = crate::series_cache::get_series(&state.pool, id)
                .await
                .unwrap()
                .unwrap();
            let ch = crate::series_cache::get_chapters(&state.pool, id)
                .await
                .unwrap();
            if m.title == "Refreshed" && ch[0].name == "New chapter" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background refresh did not persist");
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "one metadata and one chapter refresh"
    );
    // Fresh viewer state remains independent of the source cache, including after
    // a refresh returned global unread flags from Suwayomi.
    sqlx::query(r#"INSERT INTO users (id, username, email, password_hash, created_at) VALUES ('6e325f9f-8b73-46d2-a34d-cfd95e86edc4', 'u', 'u@x', 'h', '2020-01-01T00:00:00Z')"#)
        .execute(&state.pool).await.unwrap();
    sqlx::query(r#"INSERT INTO sessions (token, user_id, created_at, expires_at) VALUES ($1, '6e325f9f-8b73-46d2-a34d-cfd95e86edc4', '2026-01-01T00:00:00Z', '2999-01-01T00:00:00Z')"#)
        .bind(auth::hash_token("tok")).execute(&state.pool).await.unwrap();
    sqlx::query(r#"INSERT INTO user_library (user_id, series_id, created_at, is_favorite) VALUES ('6e325f9f-8b73-46d2-a34d-cfd95e86edc4', $1, '2020-01-01T00:00:00Z', TRUE)"#)
        .bind(id.to_string()).execute(&state.pool).await.unwrap();
    sqlx::query(r#"INSERT INTO suwayomi_progress (user_id, chapter_id, series_id, last_page_read, read, updated_at) VALUES ('6e325f9f-8b73-46d2-a34d-cfd95e86edc4', $1, $2, 9, TRUE, '2020-01-01T00:00:00Z')"#)
        .bind((id * 10).to_string()).bind(id.to_string()).execute(&state.pool).await.unwrap();
    let schema = build_schema(state.clone(), false);
    let query = format!(
        r#"{{ series(id: "{id}") {{ isFavorite }} chapters(seriesId: "{id}") {{ read lastPageRead }} }}"#
    );
    let execute = || {
        schema.execute(
            async_graphql::Request::new(&query)
                .data(RequestAuth(Some("tok".into())))
                .data(RequestUserCache::default())
                .data(RequestLibraryCache::default()),
        )
    };
    let response = execute().await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = response.data.into_json().unwrap();
    assert_eq!(data["series"]["isFavorite"], true);
    assert_eq!(data["chapters"][0]["read"], true);
    assert_eq!(data["chapters"][0]["lastPageRead"], 9);
    sqlx::query(r#"UPDATE user_library SET is_favorite = FALSE WHERE user_id = '6e325f9f-8b73-46d2-a34d-cfd95e86edc4'"#)
        .execute(&state.pool)
        .await
        .unwrap();
    let response = execute().await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data.into_json().unwrap()["series"]["isFavorite"],
        false
    );
    server.abort();
}

#[tokio::test]
async fn failed_refresh_retains_stale_data_and_true_misses_still_error() {
    let (mut state, _, server) = fixture().await;
    server.abort();
    state.suwayomi = SuwayomiClient::new("http://127.0.0.1:1".into(), None, None);
    sqlx::query("INSERT INTO suwayomi_series (id, title, status, source_id, updated_at) VALUES (88003, 'Cached', 'ONGOING', 'src', '2020-01-01T00:00:00Z')")
        .execute(&state.pool).await.unwrap();
    assert_eq!(
        resolve_series_cached(&state, 88003).await.unwrap().title,
        "Cached"
    );
    // Wait on the refresh's existing lock, then verify it preserved the cached
    // row and left the cooldown in place after its network failure.
    let handle = state.series_inflight.lock_handle(88003);
    let guard = handle.lock().await;
    drop(guard);
    assert!(state
        .series_inflight
        .try_background_refresh(88003)
        .is_none());
    assert_eq!(
        resolve_series_cached(&state, 88003).await.unwrap().title,
        "Cached"
    );
    assert!(resolve_series_cached(&state, 88004).await.is_err());
}

#[tokio::test]
async fn cold_cache_misses_still_fetch_and_persist_data() {
    let (state, requests, server) = fixture().await;
    assert_eq!(
        resolve_series_cached(&state, 88002).await.unwrap().title,
        "Refreshed"
    );
    assert_eq!(
        resolve_chapters_cached(&state, 88002).await.unwrap()[0].name,
        "New chapter"
    );
    assert!(crate::series_cache::get_series(&state.pool, 88002)
        .await
        .unwrap()
        .is_some());
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn cold_cache_reads_return_while_the_writer_is_held() {
    let (state, _, server) = fixture().await;
    let writer = state.pool.begin().await.unwrap();
    let manga = tokio::time::timeout(Duration::from_secs(2), resolve_series_cached(&state, 88005))
        .await
        .expect("metadata miss waited for its cache write")
        .unwrap();
    assert_eq!(manga.title, "Refreshed");
    let chapters = tokio::time::timeout(
        Duration::from_secs(2),
        resolve_chapters_cached(&state, 88005),
    )
    .await
    .expect("chapter miss waited for its cache write")
    .unwrap();
    assert_eq!(chapters[0].name, "New chapter");
    writer.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let manga = crate::series_cache::get_series(&state.pool, 88005)
                .await
                .unwrap();
            let chapters = crate::series_cache::get_chapters(&state.pool, 88005)
                .await
                .unwrap();
            if manga.is_some() && !chapters.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("derived cache writes did not finish");
    state.pool.close().await;
    server.abort();
}

#[tokio::test]
async fn background_refresh_is_bounded_and_keeps_retry_backoff_after_completion() {
    let locks = KeyedLocks::default();
    let first = locks.try_background_refresh(1).unwrap();
    assert!(
        locks.try_background_refresh(1).is_none(),
        "same source is single-flight"
    );
    let second = locks.try_background_refresh(2).unwrap();
    assert!(
        locks.try_background_refresh(3).is_none(),
        "no waiting tasks beyond the bound"
    );
    drop(first);
    assert!(
        locks.try_background_refresh(1).is_none(),
        "completed/failed refresh waits before retrying"
    );
    assert!(locks.try_background_refresh(3).is_some());
    drop(second);
}
