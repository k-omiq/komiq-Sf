use std::sync::{atomic::Ordering, Arc};
use std::time::Duration;

use axum::{
    extract::{Path, RawQuery, State},
    http::{header, StatusCode},
};

use crate::catalog::work_metadata::{self, FieldGroup};
use crate::cover;
use crate::cover_store::{
    self,
    test_support::{legacy_pool, store, store_with_legacy, TestDir},
    CopyCursor, CoverStore, Kind,
};
use crate::database::TestPoolOptions;
use crate::db::Database;
use crate::locks::pause_points::PausePoints;
use crate::mangadex::MangaDexClient;
use crate::source_budget::SourceBudgets;
use crate::suwayomi::{testsrv, SuwayomiClient};

async fn pools() -> (Database, CoverStore, TestDir) {
    let main = TestPoolOptions::new()
        .max_connections(4)
        .connect("sqlite::memory:")
        .await
        .unwrap()
        .foreground();
    sqlx::migrate!("./migrations_pg")
        .run(main.write())
        .await
        .unwrap();
    let dir = TestDir::new();
    let covers = store(&dir);
    (main, covers, dir)
}

async fn work(main: &Database, id: &str) {
    sqlx::query("INSERT INTO work (id, cover_file_name, created_at, updated_at) VALUES ($1, 'cover.jpg', now(), now())")
        .bind(id).execute(main).await.unwrap();
}

async fn mapping(
    main: &Database,
    work: &str,
    id: &str,
    kind: &str,
    source: Option<&str>,
    key: &str,
    created: &str,
) {
    sqlx::query("INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) VALUES ($1, $2, $3, $4, $5, $6::text::timestamptz)")
        .bind(id).bind(work).bind(kind).bind(source).bind(key).bind(created).execute(main).await.unwrap();
}

async fn series(main: &Database, id: i64) {
    // Deliberately different from the mapping's origin: budgets use this value,
    // but trust and provenance must use COALESCE(source_series.source_id, source_type).
    sqlx::query("INSERT INTO suwayomi_series (id, title, status, source_id, in_library, thumbnail_url, updated_at) VALUES ($1, 'Cover', 'ONGOING', 'budget-source', true, $2, now())")
        .bind(id).bind(format!("/api/v1/manga/{id}/thumbnail")).execute(main).await.unwrap();
}

async fn bar(main: &Database, source: &str) {
    work_metadata::set_bar(main, source, FieldGroup::Cover, true)
        .await
        .unwrap();
}

async fn provenance(main: &Database, work: &str) -> (Option<i64>, Option<String>) {
    sqlx::query_as("SELECT cover_cached_version, cover_source FROM work WHERE id = $1")
        .bind(work)
        .fetch_one(main)
        .await
        .unwrap()
}

async fn crawl(main: &Database, covers: &CoverStore, md: &MangaDexClient, suw: &SuwayomiClient) {
    cover::crawl_uncached_covers(
        main,
        covers,
        md,
        suw,
        Some(10),
        &SourceBudgets::default(),
        &tokio::sync::watch::channel(false).1,
    )
    .await;
}

#[tokio::test]
async fn barred_mangadex_anchor_is_not_chosen_by_crawl_or_repair() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_md").await;
    mapping(
        &main,
        "w_md",
        "ss_md",
        "mangadex",
        None,
        "md-id",
        "2026-01-01",
    )
    .await;
    let origin = testsrv::spawn(testsrv::jpeg(32, 48), Duration::ZERO).await;
    let md = MangaDexClient::new("cover-bar", 100.0, 100.0)
        .with_bases(&origin.base_url, &origin.base_url);
    md.set_enabled(true);
    let suw = SuwayomiClient::new(origin.base_url.clone(), None, None);
    bar(&main, "mangadex").await;
    assert!(cover::mangadex_cover_anchor(&main, "w_md").await.is_none());
    assert_eq!(cover::pending_cover_count(&main).await.unwrap(), 0);
    crawl(&main, &covers, &md, &suw).await;
    assert!(!cover::retry_one_cover(&main, &covers, &md, &suw, "w_md")
        .await
        .unwrap());
    assert_eq!(origin.hits.load(Ordering::SeqCst), 0);
    assert_eq!(provenance(&main, "w_md").await, (None, None));
    assert!(cover_store::row(&main, "w_md", Kind::Master)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn barred_oldest_suwayomi_mapping_is_skipped_for_the_next_allowed_mapping() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_suw").await;
    mapping(
        &main,
        "w_suw",
        "ss_old",
        "suwayomi",
        Some("old-source"),
        "7",
        "2026-01-01",
    )
    .await;
    mapping(
        &main,
        "w_suw",
        "ss_next",
        "suwayomi",
        Some("next-source"),
        "8",
        "2026-01-02",
    )
    .await;
    for id in [7, 8] {
        series(&main, id).await;
    }
    let origin = testsrv::spawn(testsrv::jpeg(32, 48), Duration::ZERO).await;
    let md = MangaDexClient::new("cover-bar", 100.0, 100.0);
    let suw = SuwayomiClient::new(origin.base_url.clone(), None, None);
    bar(&main, "old-source").await;
    assert_eq!(cover::pending_cover_count(&main).await.unwrap(), 1);
    crawl(&main, &covers, &md, &suw).await;
    assert_eq!(
        provenance(&main, "w_suw").await.1.as_deref(),
        Some("next-source")
    );
    assert_eq!(origin.image_hits.load(Ordering::SeqCst), 1);
    assert!(cover::retry_one_cover(&main, &covers, &md, &suw, "w_suw")
        .await
        .unwrap());
    assert_eq!(
        provenance(&main, "w_suw").await.1.as_deref(),
        Some("next-source")
    );
    assert_eq!(origin.image_hits.load(Ordering::SeqCst), 2);
    bar(&main, "next-source").await;
    assert_eq!(cover::pending_cover_count(&main).await.unwrap(), 0);
}

#[tokio::test]
async fn source_publication_is_refused_when_a_bar_lands_after_selection() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_race").await;
    mapping(
        &main,
        "w_race",
        "ss_md",
        "mangadex",
        None,
        "md-id",
        "2026-01-01",
    )
    .await;
    let anchor = cover::mangadex_cover_anchor(&main, "w_race").await.unwrap();
    let points = PausePoints::default();
    let armed = points.arm("cover-before-lock");
    let writer = main.clone();
    let cache = covers.clone();
    let publication = tokio::spawn(cover::COVER_PUBLICATION_PAUSE.scope(points, async move {
        cover::put_work_cover_for_missing_anchor(
            &writer,
            &cache,
            "w_race",
            b"refused-art",
            &anchor.0,
            &anchor.1,
        )
        .await
    }));
    tokio::time::timeout(Duration::from_secs(30), armed.reached())
        .await
        .unwrap();
    bar(&main, "mangadex").await;
    armed.release();
    assert!(!publication.await.unwrap().unwrap());
    assert_eq!(provenance(&main, "w_race").await, (None, None));
    assert!(cover_store::row(&main, "w_race", Kind::Master)
        .await
        .unwrap()
        .is_none());
    assert!(!covers
        .object_path(&cover_store::sha256(b"refused-art"))
        .exists());
}

#[tokio::test]
async fn repair_keeps_the_old_cover_when_publication_is_refused() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_repair").await;
    mapping(
        &main,
        "w_repair",
        "ss_suw",
        "suwayomi",
        Some("source"),
        "7",
        "2026-01-01",
    )
    .await;
    cover::put_uploaded_work_cover(&main, &covers, "w_repair", b"upload")
        .await
        .unwrap();
    let before = provenance(&main, "w_repair").await;
    let origin = testsrv::spawn(testsrv::jpeg(32, 48), Duration::ZERO).await;
    let md = MangaDexClient::new("cover-bar", 100.0, 100.0);
    let suw = SuwayomiClient::new(origin.base_url.clone(), None, None);
    let points = PausePoints::default();
    let armed = points.arm("cover-before-lock");
    let writer = main.clone();
    let cache = covers.clone();
    let repair = tokio::spawn(cover::COVER_PUBLICATION_PAUSE.scope(points, async move {
        cover::retry_one_cover(&writer, &cache, &md, &suw, "w_repair").await
    }));
    tokio::time::timeout(Duration::from_secs(30), armed.reached())
        .await
        .unwrap();
    bar(&main, "source").await;
    armed.release();
    assert!(!repair.await.unwrap().unwrap());
    assert_eq!(provenance(&main, "w_repair").await, before);
    assert_eq!(
        covers
            .read_master(&main, "w_repair", None)
            .await
            .unwrap()
            .unwrap()
            .0
            .bytes,
        b"upload"
    );
}

fn app(main: Database, covers: CoverStore, md: MangaDexClient) -> Arc<crate::AppState> {
    Arc::new(crate::AppState {
        pool: main,
        cover_pool: covers,
        suwayomi: SuwayomiClient::new("http://127.0.0.1:1".into(), None, None),
        mangadex: Arc::new(md),
        admin_users: vec![],
        scan_health: std::sync::Mutex::new(crate::ScanHealth::default()),
        auth_limiter: crate::RateLimiter::new(100, 60),
        comment_limiter: crate::graphql::comment_limiter(),
        rating_limiter: crate::graphql::rating_limiter(),
        password_work: Arc::new(tokio::sync::Semaphore::new(4)),
        federated_limiter: crate::RateLimiter::new(100, 60),
        session_ttl_secs: 3600,
        series_inflight: crate::KeyedLocks::default(),
        chapters_inflight: crate::KeyedLocks::default(),
        source_budgets: Default::default(),
        catalogue_cover_phash: false,
        ext_icons_dir: std::path::PathBuf::new(),
        media_manifests: None,
        policy: Default::default(),
        setup_code: crate::instance::SetupCode::default(),
    })
}

async fn get_cover(app: &Arc<crate::AppState>, work: &str) -> axum::response::Response {
    crate::serve_cover(
        State(app.clone()),
        State(crate::CoverDb(app.cover_pool.clone())),
        Path(format!("{work}.webp")),
        RawQuery(None),
    )
    .await
}

#[tokio::test]
async fn lazy_mangadex_fill_with_a_cover_bar_returns_no_store_404_without_upstream() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_lazy").await;
    mapping(
        &main,
        "w_lazy",
        "ss_md",
        "mangadex",
        None,
        "md-id",
        "2026-01-01",
    )
    .await;
    let origin = testsrv::spawn(testsrv::jpeg(32, 48), Duration::ZERO).await;
    let md = MangaDexClient::new("cover-bar", 100.0, 100.0)
        .with_bases(&origin.base_url, &origin.base_url);
    md.set_enabled(true);
    bar(&main, "mangadex").await;
    let app = app(main, covers, md);
    let response = get_cover(&app, "w_lazy").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(!response.headers().contains_key(header::LOCATION));
    assert_eq!(origin.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn set_cover_bar_removes_only_matching_source_provenance_and_refreshes_cards() {
    let (main, covers, _dir) = pools().await;
    for id in ["w_source", "w_upload", "w_unknown", "w_other"] {
        work(&main, id).await;
    }
    // No current mapping of this source: removal must use published provenance.
    assert!(
        cover::put_source_work_cover(&main, &covers, "w_source", b"source", "source")
            .await
            .unwrap()
    );
    assert!(
        cover::put_uploaded_work_cover(&main, &covers, "w_upload", b"upload")
            .await
            .unwrap()
    );
    assert!(
        cover::put_source_work_cover(&main, &covers, "w_unknown", b"legacy", "source")
            .await
            .unwrap()
    );
    sqlx::query("UPDATE work SET cover_source = NULL WHERE id = 'w_unknown'")
        .execute(&main)
        .await
        .unwrap();
    assert!(
        cover::put_source_work_cover(&main, &covers, "w_other", b"other", "other")
            .await
            .unwrap()
    );
    let upload = provenance(&main, "w_upload").await;
    let unknown = provenance(&main, "w_unknown").await;
    let other = provenance(&main, "w_other").await;
    for (id, key) in [("w_upload", "7"), ("w_unknown", "8")] {
        mapping(
            &main,
            id,
            &format!("ss_{id}"),
            "suwayomi",
            Some("source"),
            key,
            "2026-01-01",
        )
        .await;
    }
    let master = covers
        .read_master(&main, "w_source", None)
        .await
        .unwrap()
        .unwrap()
        .0;
    cover::put_current_variant(&main, &covers, "w_source", 192, &master, b"derivative")
        .await
        .unwrap();
    sqlx::query("INSERT INTO feed_updates (work_id, title, latest_at, cover_url) VALUES ('w_source', 'Cover', now(), 'old')").execute(&main).await.unwrap();
    sqlx::query("INSERT INTO feed_series_updates (work_id, reader_id, title, released_at, cover_url) VALUES ('w_source', '7', 'Cover', now(), 'old')").execute(&main).await.unwrap();
    sqlx::query("INSERT INTO browse_catalogue (work_id, reader_id, title, created_at, cover_url) VALUES ('w_source', '7', 'Cover', now(), 'old')").execute(&main).await.unwrap();
    assert_eq!(
        work_metadata::set_bar(&main, "source", FieldGroup::Cover, true)
            .await
            .unwrap(),
        3 // Two mapped works re-checked, plus the detached source cover removed.
    );
    assert_eq!(provenance(&main, "w_source").await, (None, None));
    let objects: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cover_object WHERE work_id = 'w_source'")
            .fetch_one(&main)
            .await
            .unwrap();
    assert_eq!(objects, 0);
    for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
        let url: String = sqlx::query_scalar(&format!(
            "SELECT cover_url FROM {table} WHERE work_id = 'w_source'"
        ))
        .fetch_one(&main)
        .await
        .unwrap();
        assert_eq!(url, "");
    }
    for (id, expected, bytes) in [
        ("w_upload", upload, b"upload".as_slice()),
        ("w_unknown", unknown, b"legacy"),
        ("w_other", other, b"other"),
    ] {
        assert_eq!(provenance(&main, id).await, expected);
        assert_eq!(
            covers
                .read_master(&main, id, None)
                .await
                .unwrap()
                .unwrap()
                .0
                .bytes,
            bytes
        );
    }
    assert_eq!(crate::db::reconcile_cover_pointers(&main).await.unwrap(), 0);
    assert_eq!(provenance(&main, "w_source").await, (None, None));
    assert_eq!(
        work_metadata::set_bar(&main, "source", FieldGroup::Cover, false)
            .await
            .unwrap(),
        2
    );
    assert_eq!(provenance(&main, "w_unknown").await.1, None);
}

#[tokio::test]
async fn revoked_cover_is_not_restored_by_legacy_copy_reconcile_or_lazy_fallback() {
    let (main, _covers, dir) = pools().await;
    let legacy = legacy_pool().await;
    let covers = store_with_legacy(&dir, legacy.clone(), CopyCursor::default());
    work(&main, "w_revoked").await;
    sqlx::query("INSERT INTO work_cover_blob VALUES ('w_revoked', $1, 7, 'now')")
        .bind(b"legacy".as_slice())
        .execute(&legacy)
        .await
        .unwrap();
    cover::put_source_work_cover(&main, &covers, "w_revoked", b"published", "source")
        .await
        .unwrap();
    bar(&main, "source").await;
    assert!(!crate::cover_copy::copy_asset(&main, &covers, "w_revoked")
        .await
        .unwrap());
    assert_eq!(crate::db::reconcile_cover_pointers(&main).await.unwrap(), 0);
    let app = app(
        main.clone(),
        covers.clone(),
        MangaDexClient::new("cover-bar", 100.0, 100.0),
    );
    let response = get_cover(&app, "w_revoked").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(provenance(&main, "w_revoked").await, (None, None));
    // A later allowed fill sees a genuine gap despite the old SQLite master.
    mapping(
        &main,
        "w_revoked",
        "ss_md",
        "mangadex",
        None,
        "md-id",
        "2026-01-01",
    )
    .await;
    assert!(cover::put_work_cover_for_missing_anchor(
        &main,
        &covers,
        "w_revoked",
        b"allowed",
        "md-id",
        "cover.jpg"
    )
    .await
    .unwrap());
    assert_eq!(
        provenance(&main, "w_revoked").await.1.as_deref(),
        Some("mangadex")
    );
}

#[tokio::test]
async fn recompression_preserves_source_origin_and_upload_replacement_clears_it() {
    let (main, covers, _dir) = pools().await;
    work(&main, "w_origin").await;
    assert!(
        cover::put_source_work_cover(&main, &covers, "w_origin", b"source", "source")
            .await
            .unwrap()
    );
    assert!(cover::put_work_cover_if_current(
        &main,
        &covers,
        "w_origin",
        b"recompressed",
        Some(b"source")
    )
    .await
    .unwrap());
    assert_eq!(
        provenance(&main, "w_origin").await.1.as_deref(),
        Some("source")
    );
    assert!(
        cover::put_uploaded_work_cover(&main, &covers, "w_origin", b"upload")
            .await
            .unwrap()
    );
    let uploaded = provenance(&main, "w_origin").await;
    assert_eq!(uploaded.1, None);
    bar(&main, "source").await;
    assert_eq!(provenance(&main, "w_origin").await, uploaded);
    assert_eq!(
        covers
            .read_master(&main, "w_origin", None)
            .await
            .unwrap()
            .unwrap()
            .0
            .bytes,
        b"upload"
    );
}

#[tokio::test]
async fn cover_bar_removes_provenance_across_chunks_without_current_mappings() {
    let (main, covers, _dir) = pools().await;
    for n in 0..101 {
        let id = format!("w_chunk_{n:03}");
        work(&main, &id).await;
        assert!(
            cover::put_source_work_cover(&main, &covers, &id, b"source", "source")
                .await
                .unwrap()
        );
    }
    assert_eq!(
        work_metadata::set_bar(&main, "source", FieldGroup::Cover, true)
            .await
            .unwrap(),
        101
    );
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM work WHERE cover_source IS NOT NULL OR cover_cached_version IS NOT NULL")
        .fetch_one(&main).await.unwrap();
    assert_eq!(remaining, 0);
    let masters: i64 =
        sqlx::query_scalar("SELECT count(*) FROM cover_object WHERE kind = 'master'")
            .fetch_one(&main)
            .await
            .unwrap();
    assert_eq!(masters, 0);
}

#[tokio::test]
async fn without_bars_crawl_and_repair_keep_mangadex_first_then_oldest_suwayomi() {
    let (main, covers, _dir) = pools().await;
    for id in ["w_both", "w_suw"] {
        work(&main, id).await;
    }
    mapping(
        &main,
        "w_both",
        "ss_md",
        "mangadex",
        None,
        "md-id",
        "2026-01-03",
    )
    .await;
    for (work, prefix, first, second) in [("w_both", "both", "7", "8"), ("w_suw", "suw", "9", "10")]
    {
        // Same creation time: id breaks the tie, as in the existing repair shape.
        mapping(
            &main,
            work,
            &format!("{prefix}_a"),
            "suwayomi",
            Some("old-source"),
            first,
            "2026-01-01",
        )
        .await;
        mapping(
            &main,
            work,
            &format!("{prefix}_b"),
            "suwayomi",
            Some("new-source"),
            second,
            "2026-01-01",
        )
        .await;
    }
    for id in [7, 8, 9, 10] {
        series(&main, id).await;
    }
    let md_origin = testsrv::spawn(testsrv::jpeg(32, 48), Duration::ZERO).await;
    let suw_origin = testsrv::spawn(testsrv::jpeg(40, 60), Duration::ZERO).await;
    let md = MangaDexClient::new("cover-bar", 100.0, 100.0)
        .with_bases(&md_origin.base_url, &md_origin.base_url);
    md.set_enabled(true);
    let suw = SuwayomiClient::new(suw_origin.base_url.clone(), None, None);
    assert_eq!(cover::pending_cover_count(&main).await.unwrap(), 2);
    crawl(&main, &covers, &md, &suw).await;
    assert_eq!(
        provenance(&main, "w_both").await.1.as_deref(),
        Some("mangadex")
    );
    assert_eq!(
        provenance(&main, "w_suw").await.1.as_deref(),
        Some("old-source")
    );
    assert_eq!(md_origin.hits.load(Ordering::SeqCst), 1);
    assert_eq!(suw_origin.image_hits.load(Ordering::SeqCst), 1);
    for id in ["w_both", "w_suw"] {
        assert!(cover::retry_one_cover(&main, &covers, &md, &suw, id)
            .await
            .unwrap());
    }
    assert_eq!(
        provenance(&main, "w_both").await.1.as_deref(),
        Some("mangadex")
    );
    assert_eq!(
        provenance(&main, "w_suw").await.1.as_deref(),
        Some("old-source")
    );
    assert_eq!(md_origin.hits.load(Ordering::SeqCst), 2);
    assert_eq!(suw_origin.image_hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn lost_source_master_clears_its_origin_so_a_later_bar_keeps_the_legacy_copy() {
    let (main, _covers, dir) = pools().await;
    let legacy = legacy_pool().await;
    let covers = store_with_legacy(&dir, legacy.clone(), CopyCursor::default());
    work(&main, "w_lost").await;
    sqlx::query("INSERT INTO work_cover_blob VALUES ('w_lost', $1, 7, 'now')")
        .bind(b"legacy".as_slice())
        .execute(&legacy)
        .await
        .unwrap();
    assert!(
        cover::put_source_work_cover(&main, &covers, "w_lost", b"published", "A")
            .await
            .unwrap()
    );
    assert_eq!(provenance(&main, "w_lost").await.1.as_deref(), Some("A"));
    // The index lost the master: reconcile clears the pointer and its origin together.
    sqlx::query("DELETE FROM cover_object WHERE work_id = 'w_lost'")
        .execute(&main)
        .await
        .unwrap();
    assert_eq!(crate::db::reconcile_cover_pointers(&main).await.unwrap(), 1);
    assert_eq!(provenance(&main, "w_lost").await, (None, None));
    // The legacy copy refills the gap with a master of unknown origin.
    assert!(crate::cover_copy::copy_asset(&main, &covers, "w_lost")
        .await
        .unwrap());
    assert_eq!(provenance(&main, "w_lost").await, (Some(7), None));
    work_metadata::set_bar(&main, "A", FieldGroup::Cover, true)
        .await
        .unwrap();
    assert_eq!(provenance(&main, "w_lost").await, (Some(7), None));
    assert_eq!(
        covers
            .read_master(&main, "w_lost", None)
            .await
            .unwrap()
            .unwrap()
            .0
            .bytes,
        b"legacy"
    );
    let revoked: i64 = sqlx::query_scalar("SELECT count(*) FROM work_cover_revocation")
        .fetch_one(&main)
        .await
        .unwrap();
    assert_eq!(revoked, 0);
}
