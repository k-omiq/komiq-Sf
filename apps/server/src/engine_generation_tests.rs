use super::*;
use crate::database::{SqliteTestPoolOptions, TestPoolOptions};
use crate::suwayomi::{testsrv, SuwayomiChapter, SuwayomiManga};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The pinned Suwayomi engine's answer to `meta(key:)` for a key it does not have.
const ENGINE_MISSING_META: &str = "The field at path '/meta' was declared as a non null type, but the code involved in retrieving data has wrongly returned a null value.  The graphql specification requires that the parent field be set to null, or if that is non nullable that it bubble up null to its parent and so on. The non-nullable type is 'GlobalMetaType' within parent type 'Query'";

#[derive(Clone)]
pub(crate) struct MockEngine {
    pub data: Arc<Mutex<MockData>>,
    pub url: String,
    pub fingerprints: Arc<AtomicUsize>,
    server: Arc<tokio::task::JoinHandle<()>>,
}
pub(crate) struct MockData {
    pub marker: Option<String>,
    pub meta: bool,
    pub missing_error: bool,
    pub mangas: Vec<Value>,
    pub chapters: Vec<Value>,
    pub delay: Duration,
    pub fingerprint_delay: Duration,
    pub fail_marker_write: bool,
}
impl Drop for MockEngine {
    fn drop(&mut self) {
        if Arc::strong_count(&self.server) == 1 {
            self.server.abort();
        }
    }
}
impl MockEngine {
    pub async fn spawn() -> Self {
        let data = Arc::new(Mutex::new(MockData {
            marker: None,
            meta: true,
            missing_error: true,
            mangas: vec![manga_json(7)],
            chapters: vec![chapter_json(8, 7)],
            delay: Duration::ZERO,
            fingerprint_delay: Duration::ZERO,
            fail_marker_write: false,
        }));
        let fingerprints = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route("/api/graphql", axum::routing::post({
            let data = data.clone(); let fingerprints = fingerprints.clone();
            move |axum::Json(request): axum::Json<Value>| {
                let data = data.clone(); let fingerprints = fingerprints.clone();
                async move {
                    let delay = data.lock().await.delay;
                    tokio::time::sleep(delay).await;
                    if request["query"].as_str().unwrap().contains("EngineFingerprint") {
                        let delay = data.lock().await.fingerprint_delay;
                        tokio::time::sleep(delay).await;
                    }
                    let mut d = data.lock().await;
                    let query = request["query"].as_str().unwrap();
                    let vars = &request["variables"];
                    let body = if query.contains("EngineMarker") {
                        if !d.meta { json!({"errors":[{"message":"Cannot query field 'meta' on type 'Query'"}]}) }
                        else if query.contains("setGlobalMeta") && d.fail_marker_write { json!({"errors":[{"message":"Internal server error"}]}) }
                        else if query.contains("setGlobalMeta") { d.marker = vars["value"].as_str().map(str::to_owned); json!({"data":{"setGlobalMeta":{"meta":{"key":MARKER,"value":d.marker}}}}) }
                        else if d.marker.is_none() && d.missing_error { json!({"errors":[{"message":ENGINE_MISSING_META,"path":["meta"]}]}) }
                        else { json!({"data":{"meta":d.marker.as_ref().map(|v| json!({"key":MARKER,"value":v}))}}) }
                    } else if query.contains("EngineFingerprint") {
                        fingerprints.fetch_add(1,Ordering::SeqCst);
                        // Return only requested ids, as the engine's `in` filter does.
                        let selected = |field: &str, rows: &[Value]| {
                            let start = query.find(&format!("{field}(filter:")).unwrap();
                            let ids = query[start..].split("in:[").nth(1).unwrap().split(']').next().unwrap();
                            let ids: Vec<i64> = ids.split(',').filter_map(|id| id.parse().ok()).collect();
                            rows.iter().filter(|row| ids.contains(&row["id"].as_i64().unwrap())).cloned().collect::<Vec<_>>()
                        };
                        json!({"data":{"probe":{"nodes":d.mangas.iter().take(1).cloned().collect::<Vec<_>>()},
                            "mangas":{"nodes":selected("mangas",&d.mangas)},"chapters":{"nodes":selected("chapters",&d.chapters)}}})
                    } else if query.contains("fetchMangaAndChapters") {
                        json!({"data":{"fetchMangaAndChapters":{"manga":d.mangas.first(),"chapters":d.chapters}}})
                    } else if query.contains("mangas(condition:") {
                        json!({"data":{"mangas":{"nodes":d.mangas,"pageInfo":{"hasNextPage":false,"endCursor":null}}}})
                    } else if query.contains("fetchChapterPages") { json!({"data":{"fetchChapterPages":{"pages":["/api/v1/manga/7/chapter/0/page/0"]}}}) }
                    else if query.contains("nodes { mangaId }") { json!({"data":{"chapters":{"nodes":[{"mangaId":7}]}}}) }
                    else { json!({"data":{"manga":d.mangas.first(),"chapters":{"nodes":d.chapters}}}) };
                    axum::Json(body)
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = Arc::new(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        Self {
            data,
            url,
            fingerprints,
            server,
        }
    }
}
pub(crate) fn manga_json(id: i64) -> Value {
    json!({"id":id,"title":"Series","url":format!("/manga/{id}"),"realUrl":null,"status":"ONGOING","sourceId":"source","source":{"lang":"en"},"inLibrary":true})
}
pub(crate) fn chapter_json(id: i64, manga: i64) -> Value {
    json!({"id":id,"mangaId":manga,"manga":{"sourceId":"source","url":format!("/manga/{manga}"),"title":"Series","realUrl":null},"url":format!("/chapter/{id}"),"sourceOrder":0,"name":"Chapter","chapterNumber":1.0,"isRead":false,"isBookmarked":false,"isDownloaded":false,"lastPageRead":0,"pageCount":1})
}
async fn setup() -> (
    Database,
    SqliteDatabase,
    MockEngine,
    SuwayomiClient,
    Arc<Fence>,
) {
    let pool = TestPoolOptions::new()
        .max_connections(4)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let covers = SqliteTestPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE suwayomi_cover_blob(manga_id INTEGER PRIMARY KEY,webp BLOB,updated_at TEXT); CREATE TABLE cover_variant(asset_key TEXT,width INTEGER,webp BLOB)").execute(&covers).await.unwrap();
    let engine = MockEngine::spawn().await;
    let mut client = SuwayomiClient::new(engine.url.clone(), None, None);
    let fence = Fence::attach(&pool, &covers, &mut client).await.unwrap();
    (pool, covers, engine, client, fence)
}
async fn bind(pool: &Database, client: &SuwayomiClient) -> (SuwayomiManga, Vec<SuwayomiChapter>) {
    let (m, c) = client.series_and_chapters(7).await.unwrap();
    crate::series_cache::put_series(pool, &m).await.unwrap();
    crate::series_cache::put_chapters_in_generation(
        pool,
        7,
        &c,
        crate::engine_keys::token(m.engine_generation.as_deref()).unwrap(),
    )
    .await
    .unwrap();
    (m, c)
}

#[tokio::test]
async fn first_deploy_adopts_and_reads_back_marker_even_with_existing_references() {
    let (pool, _covers, engine, client, fence) = setup().await;
    assert_eq!(
        engine.data.lock().await.marker,
        fence.generation().map(|id| id.to_string())
    );
    assert_eq!(states(&pool).await.unwrap()[0].state, "active");
    let (m, _) = bind(&pool, &client).await;
    assert_eq!(m.engine_generation, engine.data.lock().await.marker);
    // A first M5 deploy can have numeric references but no generation yet.
    sqlx::query("DELETE FROM engine_chapter_key")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM engine_manga_key")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM engine_generation")
        .execute(&pool)
        .await
        .unwrap();
    engine.data.lock().await.marker = None;
    fence.check().await;
    assert_eq!(states(&pool).await.unwrap()[0].state, "active");
    assert!(fence.wait().await.is_ok());
}

#[tokio::test]
async fn marker_missing_as_null_or_graphql_error_and_no_meta_support() {
    for body in [
        json!({"data":{"meta":null}}),
        json!({"errors":[{"message":ENGINE_MISSING_META,"path":["meta"]}]}),
    ] {
        let origin = testsrv::spawn_status(
            200,
            "application/json",
            body.to_string().into_bytes(),
            Duration::ZERO,
        )
        .await;
        let client = SuwayomiClient::new(origin.base_url.clone(), None, None);
        assert!(matches!(
            read_marker(&client).await.unwrap(),
            Marker::Missing
        ));
    }
    let (pool, _covers, engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    engine.data.lock().await.meta = false;
    fence.check().await;
    assert!(fence.wait().await.is_ok());
    assert!(states(&pool).await.unwrap()[0].unfenced);
    let mut m: SuwayomiManga = serde_json::from_value(manga_json(7)).unwrap();
    m.source_id = "another-source".into();
    assert!(crate::series_cache::put_series(&pool, &m).await.is_err());
    // The write guard forces a full check even without a marker.
    engine.data.lock().await.mangas[0]["sourceId"] = json!("another-source");
    fence.check().await;
    assert!(fence.wait().await.is_err());
}

#[tokio::test]
async fn colliding_ids_and_same_relative_keys_under_other_source_quarantine_and_purge() {
    for change_source in [false, true] {
        let (pool, covers, engine, client, fence) = setup().await;
        bind(&pool, &client).await;
        sqlx::raw_sql("INSERT INTO suwayomi_cover_blob VALUES(7,X'01','now'); INSERT INTO cover_variant VALUES('s:7',192,X'02'); INSERT INTO cover_variant VALUES('w_kept',192,X'03')").execute(&covers).await.unwrap();
        client.pages(8).await.unwrap();
        client.chapter_manga_id(8).await.unwrap();
        assert!(client.reader_pages_cached(8));
        {
            let mut d = engine.data.lock().await;
            if change_source {
                d.mangas[0]["sourceId"] = json!("other");
            } else {
                d.mangas[0]["url"] = json!("/different");
            }
        }
        let before = engine.fingerprints.load(Ordering::SeqCst);
        fence.check().await;
        assert_eq!(
            engine.fingerprints.load(Ordering::SeqCst),
            before + 1,
            "one batched fingerprint request"
        );
        assert_eq!(states(&pool).await.unwrap()[0].state, "quarantined");
        assert!(!client.reader_pages_cached(8));
        assert!(client.pages(8).await.is_err());
        assert!(crate::engine_keys::chapter_origin(&pool, &client, 8)
            .await
            .is_err());
        assert!(crate::series_cache::put_series(
            &pool,
            &serde_json::from_value(manga_json(7)).unwrap()
        )
        .await
        .is_err());
        let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM suwayomi_cover_blob),(SELECT count(*) FROM cover_variant)").fetch_one(&covers).await.unwrap();
        assert_eq!(counts, (0, 1));
        // File-copy restore brings the marker AND original ids back, and resumes.
        engine.data.lock().await.mangas = vec![manga_json(7)];
        fence.check().await;
        assert!(fence.wait().await.is_ok());
    }
}

#[tokio::test]
async fn empty_replacement_adopts_only_without_any_numeric_reference() {
    for referenced in [false, true] {
        let (pool, _covers, engine, client, fence) = setup().await;
        if referenced {
            bind(&pool, &client).await;
        }
        {
            let mut d = engine.data.lock().await;
            d.marker = None;
            d.mangas.clear();
            d.chapters.clear();
        }
        fence.check().await;
        assert_eq!(
            states(&pool).await.unwrap()[0].state,
            if referenced { "quarantined" } else { "active" }
        );
    }
}

#[tokio::test]
async fn removed_sample_is_replaced_without_quarantine() {
    let (pool, _covers, engine, client, fence) = setup().await;
    {
        let mut d = engine.data.lock().await;
        d.chapters = (8..28).map(|id| chapter_json(id, 7)).collect();
    }
    bind(&pool, &client).await;
    engine.data.lock().await.chapters.retain(|c| c["id"] != 8);
    fence.check().await;
    assert!(fence.wait().await.is_ok());
    engine
        .data
        .lock()
        .await
        .chapters
        .iter_mut()
        .find(|c| c["id"] == 24)
        .unwrap()["url"] = json!("/wrong");
    fence.check().await;
    assert_eq!(
        states(&pool).await.unwrap()[0].state,
        "quarantined",
        "the next binding replaced the gone sample"
    );
}

#[tokio::test]
async fn restored_marker_requires_a_verified_binding() {
    let (pool, _covers, engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    let marker = engine.data.lock().await.marker.clone();
    engine.data.lock().await.marker = None;
    fence.check().await;
    assert!(fence.is_blocked());
    {
        let mut d = engine.data.lock().await;
        d.marker = marker;
        d.mangas = vec![manga_json(70)];
        d.chapters = vec![chapter_json(80, 70)];
    }
    fence.check().await;
    assert!(fence.is_blocked());
    let state = states(&pool).await.unwrap().remove(0);
    assert_eq!(state.state, "quarantined");
    assert_eq!(
        state.reason.as_deref(),
        Some("No stored source identity can be verified against the engine")
    );
    // The gone-id memo cannot turn the next check into a successful empty sample.
    fence.check().await;
    assert!(fence.is_blocked());
    {
        let mut d = engine.data.lock().await;
        d.mangas = vec![manga_json(7)];
        d.chapters = vec![chapter_json(8, 7)];
    }
    fence.check().await;
    assert!(!fence.is_blocked(), "the original engine's bindings can return");
}

#[tokio::test]
async fn timed_out_fingerprint_cannot_reopen_gate_without_a_verified_binding() {
    let (pool, _covers, engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    let marker = engine.data.lock().await.marker.take();
    fence.check().await;
    assert!(fence.is_blocked());
    {
        let mut d = engine.data.lock().await;
        d.marker = marker;
        d.mangas = vec![manga_json(70)];
        d.chapters = vec![chapter_json(80, 70)];
        d.fingerprint_delay = Duration::from_millis(1600);
    }
    let before = engine.fingerprints.load(Ordering::SeqCst);
    fence.check().await;
    assert!(fence.is_blocked());
    assert_eq!(engine.fingerprints.load(Ordering::SeqCst), before + 1);
    // The first batch marked every binding gone; the empty follow-up timed out.
    assert!(fence.gone_manga.lock().await.contains(&7));
    assert!(fence.gone_chapter.lock().await.contains(&8));
    engine.data.lock().await.fingerprint_delay = Duration::ZERO;
    fence.check().await;
    assert!(fence.is_blocked());
    let state = states(&pool).await.unwrap().remove(0);
    assert_eq!(state.state, "quarantined");
    assert_eq!(
        state.reason.as_deref(),
        Some("No stored source identity can be verified against the engine")
    );
}

#[tokio::test]
async fn an_engine_that_is_not_up_yet_reads_as_unreachable() {
    // A fresh install answers before Suwayomi has started: admin screens show the
    // plain "not reachable" sentence, not an identity problem.
    let pool = TestPoolOptions::new()
        .max_connections(4)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let covers = SqliteTestPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let mut client = SuwayomiClient::new("http://127.0.0.1:9".into(), None, None);
    let fence = Fence::attach(&pool, &covers, &mut client).await.unwrap();
    let error = fence.wait().await.unwrap_err();
    assert_eq!(error.downcast_ref::<FenceError>(), Some(&FenceError::Unreachable));
    assert_eq!(error.to_string(), crate::graphql::ENGINE_UNREACHABLE);
}

#[tokio::test]
async fn references_without_captured_keys_stay_active_after_adoption() {
    // Right after the first adoption no key row is captured yet while Suwayomi
    // references already exist: that is not a failed verification.
    let (pool, _covers, _engine, _client, fence) = setup().await;
    fence.check().await;
    let generation = fence.generation().unwrap();
    sqlx::raw_sql("INSERT INTO work(id,created_at,updated_at) VALUES('w_a',now(),now()); INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('ss_keyless','w_a','suwayomi','source','7',now())")
        .execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM engine_manga_key").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM engine_chapter_key").execute(&pool).await.unwrap();
    fence.check().await;
    assert!(!fence.is_blocked());
    let state = states(&pool).await.unwrap().remove(0);
    assert_eq!((state.state.as_str(), fence.generation()), ("active", Some(generation)));
}

#[tokio::test]
async fn missing_fingerprint_batch_resamples_other_stored_bindings() {
    let (pool, _covers, engine, _client, fence) = setup().await;
    let generation = fence.generation().unwrap();
    sqlx::query("INSERT INTO engine_manga_key(generation,manga_id,source_id,manga_key,updated_at) SELECT $1,n,'source','/manga/' || n,now() FROM generate_series(1,33) n")
        .bind(generation).execute(&pool).await.unwrap();
    {
        let mut d = engine.data.lock().await;
        d.mangas = vec![manga_json(33)];
        d.chapters.clear();
    }
    let before = engine.fingerprints.load(Ordering::SeqCst);
    fence.check().await;
    assert!(!fence.is_blocked());
    assert_eq!(states(&pool).await.unwrap()[0].state, "active");
    assert_eq!(engine.fingerprints.load(Ordering::SeqCst) - before, 3);
}

#[tokio::test]
async fn reattach_checks_all_bindings_and_moves_keys_without_resetting_progress_epoch() {
    let (pool, _covers, engine, client, fence) = setup().await;
    {
        let mut d = engine.data.lock().await;
        d.chapters = (8..28).map(|id| chapter_json(id, 7)).collect();
    }
    bind(&pool, &client).await;
    let old = fence.generation().unwrap();
    engine.data.lock().await.marker = None;
    fence.check().await;
    engine.data.lock().await.chapters.last_mut().unwrap()["url"] = json!("/wrong");
    assert!(
        fence.reattach().await.is_err(),
        "must check beyond the first 16 bindings"
    );
    engine.data.lock().await.chapters.last_mut().unwrap()["url"] = json!("/chapter/27");
    fence.reattach().await.unwrap();
    assert_ne!(fence.generation().unwrap(), old);
    assert_eq!(numeric_epoch(&pool).await.unwrap(), 0);
    let generations: Vec<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT generation FROM engine_chapter_key")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(generations, vec![fence.generation().unwrap()]);
}

#[tokio::test]
async fn delayed_writers_and_empty_replacements_cannot_cross_generation_change() {
    let (pool, _covers, _engine, client, fence) = setup().await;
    let (m, c) = bind(&pool, &client).await;
    let old = crate::engine_keys::token(m.engine_generation.as_deref()).unwrap();
    fence.start_new_generation(Replace::Force).await.unwrap();
    assert!(crate::series_cache::put_series(&pool, &m).await.is_err());
    bind(&pool, &client).await;
    assert!(
        crate::series_cache::put_chapters_in_generation(&pool, 7, &[], old)
            .await
            .is_err()
    );
    assert!(crate::series_cache::put_chapters(&pool, 7, &c)
        .await
        .is_err());
    assert!(
        crate::cover::publish_suwayomi_cover_if_missing_in_generation(
            &pool,
            &fence.covers,
            7,
            b"old",
            old
        )
        .await
        .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM suwayomi_chapter")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn connection_loss_closes_gate_until_check_passes_but_slow_periodic_read_does_not() {
    let (pool, _covers, engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    engine.data.lock().await.delay = CHECK_TIMEOUT + Duration::from_millis(100);
    fence.check().await;
    assert!(
        !fence.is_blocked(),
        "a timeout alone cannot quarantine a healthy engine"
    );
    // Exercise the real transport classifier, not a fabricated state flag.
    let mut failed = client.clone();
    failed.base_url_for_test("http://127.0.0.1:1");
    assert!(failed
        .identity_query("query { meta(key:\"x\") { value } }", json!({}))
        .await
        .is_err());
    assert!(fence.is_blocked());
    assert!(fence.wait().await.is_err());
    engine.data.lock().await.delay = Duration::ZERO;
    fence.check().await;
    assert!(fence.wait().await.is_ok());
}

#[tokio::test]
async fn known_marker_mismatch_closes_gate_before_a_slow_fingerprint() {
    let (pool, _covers, engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    {
        let mut d = engine.data.lock().await;
        d.marker = Some(Uuid::new_v4().to_string());
        d.fingerprint_delay = CHECK_TIMEOUT + Duration::from_millis(100);
    }
    fence.check().await;
    assert!(fence.is_blocked(), "a replaced engine must not serve old ids");
    assert!(fence.wait().await.is_err());
}

#[tokio::test]
async fn fingerprint_mismatch_closes_gate_before_a_delayed_reference_check() {
    let (pool, _covers, engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    let revision = fence.revision();
    engine.data.lock().await.mangas[0]["url"] = json!("/wrong");
    // This blocks has_references, but neither the marker nor fingerprint reads.
    let mut held = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE suwayomi_progress IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *held)
        .await
        .unwrap();
    fence.check().await;
    assert!(
        fence.is_blocked(),
        "the reference read exceeded the check timeout"
    );
    assert!(
        fence.revision() > revision,
        "in-flight reads must be invalidated"
    );
    assert!(commit_ready(&pool).is_err());
    held.rollback().await.unwrap();
    fence.check().await;
    assert_eq!(states(&pool).await.unwrap()[0].state, "quarantined");
}

#[tokio::test]
async fn numeric_epoch_survives_rolled_back_state() {
    let (pool, _covers, _engine, _client, fence) = setup().await;
    assert_eq!(numeric_epoch(&pool).await.unwrap(), 0);
    let started = chrono::Utc::now().timestamp();
    fence.start_new_generation(Replace::Force).await.unwrap();
    let first = numeric_epoch(&pool).await.unwrap();
    assert!(first >= started);
    fence.start_new_generation(Replace::Force).await.unwrap();
    let second = numeric_epoch(&pool).await.unwrap();
    assert!(
        second > first,
        "rapid changes still advance the stored epoch"
    );
    // A restore happens later in wall-clock time, but brings back the epoch-0 dump.
    while chrono::Utc::now().timestamp() <= second {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    sqlx::query("DELETE FROM engine_numeric_state")
        .execute(&pool)
        .await
        .unwrap();
    fence.start_new_generation(Replace::Force).await.unwrap();
    let restored = numeric_epoch(&pool).await.unwrap();
    for issued in [0, first, second] {
        assert_ne!(restored, issued);
        let mut tx = pool.begin().await.unwrap();
        let error = progress_guard(&mut tx, Some(issued)).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<FenceError>(),
            Some(&FenceError::StaleEpoch)
        );
    }
}

#[tokio::test]
async fn new_generation_removes_numeric_browse_rows_and_keeps_work_rows() {
    let (pool, _covers, _engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    sqlx::raw_sql("INSERT INTO work(id,created_at,updated_at) VALUES('w_old',now(),now()),('w_kept',now(),now()); INSERT INTO browse_catalogue(work_id,reader_id,title,created_at) VALUES('w_old','7','Old series',now()),('w_kept','w_kept','Kept work',now())")
        .execute(&pool).await.unwrap();
    fence.start_new_generation(Replace::Force).await.unwrap();
    let readers: Vec<String> = sqlx::query_scalar("SELECT reader_id FROM browse_catalogue")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(readers, ["w_kept"]);
    let works: i64 = sqlx::query_scalar("SELECT count(*) FROM work")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(works, 2);
}

#[tokio::test]
async fn start_new_generation_requires_the_shown_quarantined_generation() {
    let (pool, _covers, engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    let old = fence.generation().unwrap();
    let marker = engine.data.lock().await.marker.clone();
    // Active: the quarantine the admin saw has ended.
    assert!(fence
        .start_new_generation(Replace::Quarantined(old))
        .await
        .is_err());
    assert_eq!(states(&pool).await.unwrap()[0].state, "active");
    assert_eq!(numeric_epoch(&pool).await.unwrap(), 0);
    assert_eq!(engine.data.lock().await.marker, marker, "marker untouched");
    engine.data.lock().await.mangas[0]["url"] = json!("/wrong");
    fence.check().await;
    assert!(fence
        .start_new_generation(Replace::Quarantined(Uuid::new_v4()))
        .await
        .is_err());
    assert_eq!(numeric_epoch(&pool).await.unwrap(), 0);
    fence
        .start_new_generation(Replace::Quarantined(old))
        .await
        .unwrap();
    assert_ne!(fence.generation(), Some(old));
    assert!(fence.wait().await.is_ok());
}

#[tokio::test]
async fn new_generation_sets_numeric_references_aside_so_an_empty_engine_adopts() {
    let (pool, _covers, engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    sqlx::query("INSERT INTO series_views(series_key,total,updated_at) VALUES('7',3,now())")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO series_admin(series_id,paused_override,updated_at) VALUES('7',true,now())")
        .execute(&pool)
        .await
        .unwrap();
    let old = fence.generation().unwrap();
    {
        let mut d = engine.data.lock().await;
        d.marker = None;
        d.mangas.clear();
        d.chapters.clear();
    }
    fence.check().await;
    assert_eq!(states(&pool).await.unwrap()[0].state, "quarantined");
    fence
        .start_new_generation(Replace::Quarantined(old))
        .await
        .unwrap();
    assert!(!has_references(&pool).await.unwrap());
    fence.check().await;
    let current = states(&pool).await.unwrap();
    let current = current.iter().find(|g| g.state != "detached").unwrap();
    assert_eq!(current.state, "active", "{:?}", current.reason);
    let aside: i64 =
        sqlx::query_scalar("SELECT count(*) FROM engine_detached_row WHERE generation=$1")
            .bind(old)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(aside, 2);
}

#[tokio::test]
async fn reattach_commits_before_the_marker_and_the_next_check_writes_it() {
    let (pool, _covers, engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    let old = fence.generation().unwrap();
    engine.data.lock().await.marker = None;
    fence.check().await;
    engine.data.lock().await.fail_marker_write = true;
    fence.reattach().await.unwrap();
    assert!(fence.wait().await.is_err(), "no marker yet");
    engine.data.lock().await.fail_marker_write = false;
    fence.check().await;
    let new = fence.generation().unwrap();
    assert_ne!(new, old);
    assert_eq!(engine.data.lock().await.marker, Some(new.to_string()));
    assert!(fence.wait().await.is_ok());
}

#[tokio::test]
async fn queued_work_cover_from_a_replaced_generation_is_not_published() {
    let (pool, _covers, _engine, client, fence) = setup().await;
    let covers = crate::db::init_covers("sqlite::memory:").await.unwrap();
    let thumb = "/api/v1/manga/7/thumbnail";
    let anchor = |id: &'static str| {
        format!("INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES('{id}','w_a','suwayomi','source','7',now()); UPDATE suwayomi_series SET thumbnail_url='{thumb}' WHERE id=7")
    };
    bind(&pool, &client).await;
    sqlx::raw_sql("INSERT INTO work(id,created_at,updated_at) VALUES('w_a',now(),now())")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(&anchor("ss_a")).execute(&pool).await.unwrap();
    let queued = current(&pool).await.unwrap();
    assert!(crate::cover::put_work_cover_for_suwayomi_anchor(&pool, &covers, "w_a", b"one", queued, thumb)
        .await
        .unwrap());
    fence.start_new_generation(Replace::Force).await.unwrap();
    // Even with engine B's series 7 enrolled on the same work, the job's bytes came
    // from a number of the replaced generation.
    bind(&pool, &client).await;
    sqlx::raw_sql(&anchor("ss_b")).execute(&pool).await.unwrap();
    assert!(!crate::cover::put_work_cover_for_suwayomi_anchor(&pool, &covers, "w_a", b"two", queued, thumb)
        .await
        .unwrap());
    let other_anchor = crate::cover::put_work_cover_for_suwayomi_anchor(
        &pool,
        &covers,
        "w_a",
        b"three",
        current(&pool).await.unwrap(),
        "/api/v1/manga/9/thumbnail",
    )
    .await
    .unwrap();
    assert!(!other_anchor, "a job for another anchor is not published");
    let stored: Vec<u8> = sqlx::query_scalar("SELECT webp FROM work_cover_blob WHERE work_id='w_a'")
        .fetch_one(&covers)
        .await
        .unwrap();
    assert_eq!(stored, b"one");
}

#[tokio::test]
async fn shared_commit_guard_excludes_quarantine() {
    let (pool, _covers, engine, client, fence) = setup().await;
    bind(&pool, &client).await;
    let mut tx = pool.foreground().begin().await.unwrap();
    shared(&mut tx, fence.generation()).await.unwrap();
    engine.data.lock().await.mangas[0]["url"] = json!("/wrong");
    let check = {
        let fence = fence.clone();
        tokio::spawn(async move { fence.check().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!check.is_finished());
    tx.commit().await.unwrap();
    check.await.unwrap();
    assert_eq!(states(&pool).await.unwrap()[0].state, "quarantined");
}

#[tokio::test]
async fn write_guard_prioritizes_the_unsampled_offending_chapter_and_can_resume() {
    let (pool, _covers, engine, client, fence) = setup().await;
    engine.data.lock().await.chapters = (8..40).map(|id| chapter_json(id, 7)).collect();
    let (_, mut chapters) = bind(&pool, &client).await;
    engine.data.lock().await.chapters.last_mut().unwrap()["url"] = json!("/wrong");
    chapters.last_mut().unwrap().url = Some("/wrong".into());
    assert!(crate::series_cache::put_chapters(&pool, 7, &chapters)
        .await
        .is_err());
    fence.check().await;
    assert!(fence.wait().await.is_err());
    engine.data.lock().await.chapters.last_mut().unwrap()["url"] = json!("/chapter/39");
    fence.check().await;
    assert!(fence.wait().await.is_ok());
}

#[test]
fn numeric_progress_epoch_keeps_old_apps_out_for_thirty_days_and_stale_epochs_out_forever() {
    assert!(epoch_allowed(0, false, None));
    assert!(!epoch_allowed(1, true, None));
    assert!(!epoch_allowed(1, true, Some(0)));
    assert!(epoch_allowed(1, true, Some(1)));
    assert!(epoch_allowed(1, false, None));
    assert!(!epoch_allowed(1, false, Some(0)));
    assert!(!epoch_allowed(1, false, Some(2)));
}

#[test]
fn missing_meta_keys_are_distinct_from_unsupported_meta_read_or_write_fields() {
    for field in ["meta", "setGlobalMeta"] {
        let error = crate::suwayomi::EngineGraphqlError(format!(
            "Validation error (FieldUndefined@[{field}])"
        ))
        .into();
        assert!(meta_unsupported(&error));
    }
    let missing = crate::suwayomi::EngineGraphqlError("meta key not found".into()).into();
    assert!(!meta_unsupported(&missing));
    assert!(missing_meta_key("Key not found"));
    assert!(missing_meta_key("NoSuchElementException: No value present"));
    assert!(missing_meta_key(ENGINE_MISSING_META));
    assert!(!missing_meta_key("Internal server error"));
}

#[tokio::test]
async fn quarantine_cover_file_purge_keeps_work_assets() {
    let covers = SqliteTestPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE suwayomi_cover_blob(manga_id INTEGER PRIMARY KEY,webp BLOB); CREATE TABLE cover_variant(asset_key TEXT,width INTEGER,webp BLOB); CREATE TABLE work_cover_blob(work_id TEXT,webp BLOB); INSERT INTO suwayomi_cover_blob VALUES(7,X'01'); INSERT INTO cover_variant VALUES('s:7',192,X'02'),('s:7@v1',320,X'03'),('w_kept@v1',192,X'04'); INSERT INTO work_cover_blob VALUES('w_kept',X'05')")
        .execute(&covers).await.unwrap();
    purge_covers(&covers).await.unwrap();
    let counts: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM suwayomi_cover_blob),(SELECT count(*) FROM cover_variant),(SELECT count(*) FROM work_cover_blob)")
        .fetch_one(&covers).await.unwrap();
    assert_eq!(counts, (0, 1, 1));
    let asset: String = sqlx::query_scalar("SELECT asset_key FROM cover_variant")
        .fetch_one(&covers)
        .await
        .unwrap();
    assert_eq!(asset, "w_kept@v1");
}
