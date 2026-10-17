//! M5 S5: policy epoch, takedowns, the in-memory denied set and the signed lease
//! (design section 11, "Policy expiry"; the verifier's own expiry rules are unit tests
//! in `media_verify`).

use super::*;

use crate::media_manifest::store::{self, CollectedPage};
use crate::media_manifest_tests::TempDir;
use crate::media_policy::{self, Kind};
use crate::media_verify::{key_id, verify_lease, CopyIdentity, PinnedKeys, PolicyGate};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::Duration;
use tower::ServiceExt;

fn image(value: u8) -> Vec<u8> {
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        2,
        3,
        image::Rgba([value, 0, 0, 255]),
    ));
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// Page 0 of every chapter is the same credits page; page 1 is the chapter's own.
fn page_bytes(order: i64, page: usize) -> Vec<u8> {
    if page == 0 {
        image(200)
    } else {
        image(order as u8 + 1)
    }
}

/// A mock engine with manga 7 and two chapters: 8 at index 0, 9 at index 1.
struct Engine {
    url: String,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Engine {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Engine {
    async fn spawn() -> Self {
        let routes = Router::new()
            .route(
                "/api/graphql",
                post(|Json(request): Json<Value>| async move {
                    let query = request["query"].as_str().unwrap();
                    let id = request["variables"]["id"].as_i64().unwrap_or(8);
                    let order = id - 8;
                    let data = if query.contains("fetchChapterPages") {
                        json!({"fetchChapterPages":{"pages":(0..2).map(|p| format!("/api/v1/manga/7/chapter/{order}/page/{p}")).collect::<Vec<_>>()}})
                    } else if query.starts_with("query CO(") {
                        json!({"chapters":{"nodes":[{"mangaId":7,"sourceOrder":order,"url":format!("/chapter-{id}"),"realUrl":null,
                            "manga":{"url":"/manga","realUrl":null,"title":"Fixture","sourceId":"source"}}]}})
                    } else if query.starts_with("query CM(") {
                        json!({"chapters":{"nodes":[{"mangaId":7}]}})
                    } else {
                        panic!("unexpected mock engine query: {query}");
                    };
                    Json(json!({ "data": data }))
                }),
            )
            .route(
                "/api/v1/manga/{m}/chapter/{c}/page/{p}",
                get(
                    |UrlPath((m, c, p)): UrlPath<(i64, i64, usize)>| async move {
                        assert_eq!(m, 7);
                        if p > 1 {
                            return StatusCode::NOT_FOUND.into_response();
                        }
                        ([(header::CONTENT_TYPE, "image/png")], page_bytes(c, p)).into_response()
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        Self { url, server }
    }
}

fn identity(chapter: i64) -> CopyIdentity {
    CopyIdentity {
        source_id: "source".into(),
        manga_key: "/manga".into(),
        chapter_key: format!("/chapter-{chapter}"),
    }
}

fn collected(bytes: &[u8]) -> CollectedPage {
    CollectedPage {
        sha256: Sha256::digest(bytes).to_vec(),
        length: bytes.len() as i64,
        mime: "image/png".into(),
        width: Some(2),
        height: Some(3),
    }
}

async fn app(pool: db::Database, engine: &Engine, key: &std::path::Path) -> Arc<AppState> {
    let mut config = Config::from_env();
    config.media_manifests = true;
    config.media_signing_key_file = key.to_path_buf();
    Arc::new(AppState {
        pool,
        cover_pool: db::init_covers("sqlite::memory:").await.unwrap(),
        suwayomi: SuwayomiClient::new(engine.url.clone(), None, None),
        mangadex: Arc::new(
            mangadex::MangaDexClient::new("media-policy-test", 100.0, 100.0)
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
        media_manifests: media_manifest::MediaState::from_config(&config).unwrap(),
        policy: media_policy::Policy::new(true),
    })
}

async fn pool() -> db::Database {
    db::Database::connect("sqlite::memory:")
        .await
        .unwrap()
        .foreground()
}

/// Engine keys for manga 7 (work-less) and its chapters 8 and 9, and one published
/// revision per chapter: the shared credits page, then the chapter's own page.
async fn two_copies(pool: &db::Database) -> (String, String) {
    let generation = engine_keys::adopt(pool).await.unwrap();
    sqlx::query("INSERT INTO engine_manga_key(generation,manga_id,source_id,manga_key,updated_at) VALUES($1,7,'source','/manga',now())")
        .bind(generation).execute(pool).await.unwrap();
    for chapter in [8_i64, 9] {
        sqlx::query("INSERT INTO engine_chapter_key(generation,chapter_id,manga_id,source_order,chapter_key,updated_at) VALUES($1,$2,7,$3,$4,now())")
            .bind(generation).bind(chapter).bind((chapter - 8) as i32).bind(format!("/chapter-{chapter}"))
            .execute(pool).await.unwrap();
    }
    let mut copies = Vec::new();
    for chapter in [8_i64, 9] {
        let order = chapter - 8;
        let pages = [
            collected(&page_bytes(order, 0)),
            collected(&page_bytes(order, 1)),
        ];
        let (copy, revision) = store::publish(pool, Some(generation), &identity(chapter), &pages)
            .await
            .unwrap();
        assert_eq!(revision, Some(1));
        copies.push(copy);
    }
    (copies[0].clone(), copies[1].clone())
}

fn router(app: Arc<AppState>) -> Router {
    Router::new()
        .route("/media/v1/o/{copy_id}/{sha256}", get(serve_media_object))
        .route("/media/v1/manifest/{copy_id}", get(serve_media_manifest))
        .route("/media/v1/policy", get(serve_media_policy))
        .route("/media/v1/c/{chapterId}/{page}", get(serve_chapter_page))
        .with_state(app)
}

async fn get_path(app: &Arc<AppState>, path: &str) -> axum::response::Response {
    tokio::time::timeout(
        Duration::from_secs(5),
        router(app.clone()).oneshot(
            axum::http::Request::builder()
                .uri(path)
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

fn cache_control(response: &axum::response::Response) -> String {
    response.headers()[header::CACHE_CONTROL]
        .to_str()
        .unwrap()
        .to_string()
}

fn pins(app: &AppState) -> PinnedKeys {
    let entry = app.media_manifests.as_ref().unwrap().signer.public_entry();
    PinnedKeys::new([(entry.kid, entry.public_key)]).unwrap()
}

async fn lease(app: &Arc<AppState>) -> (Vec<u8>, crate::media_verify::LeaseBody) {
    let response = get_path(app, "/media/v1/policy").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(cache_control(&response), "public, max-age=60");
    let bytes = body(response).await;
    let lease = verify_lease(&bytes, &pins(app)).unwrap();
    (bytes, lease)
}

async fn epoch(pool: &db::Database) -> i64 {
    media_policy::epoch(pool).await.unwrap()
}

async fn take_down(app: &AppState, kind: Kind, subject: &str) -> (String, bool) {
    let outcome = media_policy::take_down(
        &app.pool,
        &app.cover_pool,
        kind,
        subject,
        "rights request",
        Some("test"),
    )
    .await
    .unwrap();
    app.policy.invalidate();
    outcome
}

#[tokio::test]
async fn policy_snapshot_reloads_site_rules_on_replace_and_clear() {
    let pool = pool().await;
    let policy = media_policy::Policy::new(false);
    let empty = policy.current(&pool).await.unwrap();
    assert!(empty.site_rules.is_empty());

    let rule = crate::block::BlockRule::new(["action"], Vec::<&str>::new()).unwrap();
    crate::block::replace(&pool, crate::block::Scope::Site, &[rule.clone()])
        .await
        .unwrap();
    policy.invalidate();
    let blocked = policy.current(&pool).await.unwrap();
    assert!(blocked.epoch > empty.epoch);
    assert_eq!(blocked.site_rules, crate::block::BlockSet::new(vec![rule]));

    crate::block::replace(&pool, crate::block::Scope::Site, &[])
        .await
        .unwrap();
    policy.invalidate();
    let cleared = policy.current(&pool).await.unwrap();
    assert!(cleared.epoch > blocked.epoch);
    assert!(cleared.site_rules.is_empty());
}

#[tokio::test]
async fn policy_epoch_moves_in_the_same_transaction_as_each_change() {
    let pool = pool().await;
    let dir = TempDir::new();
    let engine = Engine::spawn().await;
    let app = app(pool.clone(), &engine, &dir.0.join("signing.key")).await;
    let (copy, _) = two_copies(&pool).await;
    assert_eq!(epoch(&pool).await, 0);

    // A site-wide rule is policy.
    crate::block::replace(
        &pool,
        crate::block::Scope::Site,
        &[crate::block::BlockRule::new(["action"], Vec::<&str>::new()).unwrap()],
    )
    .await
    .unwrap();
    let after_rule = epoch(&pool).await;
    assert!(after_rule > 0);

    // A takedown moves it, and records its own epoch; a repeat changes nothing.
    assert_eq!(take_down(&app, Kind::Copy, &copy).await, (copy.clone(), true));
    let after_takedown = epoch(&pool).await;
    assert!(after_takedown > after_rule);
    let rows = media_policy::list(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].epoch, after_takedown);
    assert_eq!(rows[0].created_by.as_deref(), Some("test"));
    assert_eq!(take_down(&app, Kind::Copy, &copy).await, (copy.clone(), false));
    assert_eq!(epoch(&pool).await, after_takedown);

    // A refused takedown moves nothing.
    for (kind, subject) in [(Kind::Copy, "mc_unknown"), (Kind::Work, "w_missing")] {
        assert!(media_policy::take_down(&pool, &app.cover_pool, kind, subject, "x", None)
            .await
            .is_err());
    }
    assert!(
        media_policy::take_down(&pool, &app.cover_pool, Kind::Copy, &copy, "  ", None)
            .await
            .is_err()
    );
    assert_eq!(epoch(&pool).await, after_takedown);

    // A failed transaction leaves the epoch where it was: the bump is in the caller's.
    let mut tx = pool.begin().await.unwrap();
    media_policy::bump(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(epoch(&pool).await, after_takedown);

    // Only an admin can read or change takedowns.
    let schema = graphql::build_schema(app.clone(), false);
    for document in [
        "{ mediaTakedowns { subjectId } }".to_string(),
        format!("mutation {{ takeDownMedia(kind: COPY, subjectId: \"{copy}\", reason: \"x\") }}"),
        format!("mutation {{ liftMediaTakedown(kind: COPY, subjectId: \"{copy}\") }}"),
    ] {
        assert!(!schema.execute(document).await.errors.is_empty());
    }
    assert_eq!(epoch(&pool).await, after_takedown);

    assert!(media_policy::lift(&pool, Kind::Copy, &copy).await.unwrap());
    let after_lift = epoch(&pool).await;
    assert!(after_lift > after_takedown);
    assert!(!media_policy::lift(&pool, Kind::Copy, &copy).await.unwrap());
    assert_eq!(epoch(&pool).await, after_lift);
}

#[tokio::test]
async fn shared_bytes_denying_copy_a_never_refuses_copy_b() {
    let pool = pool().await;
    let dir = TempDir::new();
    let engine = Engine::spawn().await;
    let app = app(pool.clone(), &engine, &dir.0.join("signing.key")).await;
    let (a, b) = two_copies(&pool).await;
    let shared = hex::encode(Sha256::digest(page_bytes(0, 0)));
    let own_a = hex::encode(Sha256::digest(page_bytes(0, 1)));

    for copy in [&a, &b] {
        let response = get_path(&app, &format!("/media/v1/o/{copy}/{shared}")).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    take_down(&app, Kind::Copy, &a).await;

    // Copy A is refused under its own path, for the shared page and its own.
    for hash in [&shared, &own_a] {
        let response = get_path(&app, &format!("/media/v1/o/{a}/{hash}")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(cache_control(&response), "no-store, must-revalidate");
    }
    // The same bytes under copy B still serve, with s-maxage = the lease's remaining life.
    let response = get_path(&app, &format!("/media/v1/o/{b}/{shared}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let cache = cache_control(&response);
    let s_maxage: i64 = cache
        .rsplit("s-maxage=")
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(cache.starts_with("public, max-age=31536000, immutable, s-maxage="));
    assert!((1..=media_policy::LEASE_SECS).contains(&s_maxage), "{cache}");
    assert_eq!(body(response).await, page_bytes(1, 0));

    // Manifests, the chapter-id page route and the lease agree.
    assert_eq!(
        get_path(&app, &format!("/media/v1/manifest/{a}")).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_path(&app, &format!("/media/v1/manifest/{b}")).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        get_path(&app, "/media/v1/c/8/0").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(get_path(&app, "/media/v1/c/9/0").await.status(), StatusCode::OK);
    let (_, body) = lease(&app).await;
    assert_eq!(body.denied_copies, vec![a.clone()]);
    assert!(body.denied_works.is_empty());

    // `pages` refuses the taken-down chapter only.
    let schema = graphql::build_schema(app.clone(), false);
    let refused = schema
        .execute("{ pages(chapterId: \"8\") { index } }")
        .await;
    assert_eq!(refused.errors[0].message, "No such chapter");
    let served = schema
        .execute("{ pages(chapterId: \"9\") { index mediaPath } }")
        .await;
    assert!(served.errors.is_empty(), "{:?}", served.errors);

    // Lifting restores serving.
    assert!(media_policy::lift(&pool, Kind::Copy, &a).await.unwrap());
    app.policy.invalidate();
    assert_eq!(
        get_path(&app, &format!("/media/v1/o/{a}/{shared}")).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn lease_is_signed_memoized_per_epoch_and_verifiable_by_a_cache() {
    let pool = pool().await;
    let dir = TempDir::new();
    let engine = Engine::spawn().await;
    let app = app(pool.clone(), &engine, &dir.0.join("signing.key")).await;
    let (a, _) = two_copies(&pool).await;

    let (first, lease_1) = lease(&app).await;
    let (again, _) = lease(&app).await;
    assert_eq!(first, again, "one lease per epoch and minute");
    assert_eq!(lease_1.v, 1);
    assert_eq!(lease_1.epoch, 0);
    assert_eq!(lease_1.expires_at - lease_1.issued_at, media_policy::LEASE_SECS);
    assert_eq!(lease_1.issued_at % 60, 0);
    assert_eq!(
        lease_1.kid,
        app.media_manifests.as_ref().unwrap().signer.kid
    );

    take_down(&app, Kind::Copy, &a).await;
    let (second, lease_2) = lease(&app).await;
    assert_ne!(first, second);
    assert_eq!(lease_2.epoch, epoch(&pool).await);
    assert!(lease_2.epoch > lease_1.epoch);

    // A serving cache holding the leases in order: the new one wins, the old one is
    // refused as a replay, and copy A is denied.
    let mut gate = PolicyGate::default();
    let now = chrono::Utc::now().timestamp();
    gate.accept(lease_1.clone(), now).unwrap();
    gate.accept(lease_2.clone(), now).unwrap();
    assert_eq!(
        gate.accept(lease_1, now),
        Err(crate::media_verify::VerifyError::StaleLease)
    );
    assert_eq!(
        gate.object(&a, lease_2.epoch, now),
        Err(crate::media_verify::VerifyError::Denied)
    );
    // A manifest signed after this lease's epoch is not vouched for by it.
    assert_eq!(
        gate.object("mc_other", lease_2.epoch + 1, now),
        Err(crate::media_verify::VerifyError::EpochBehind)
    );

    // An unpinned key fails.
    let other = TempDir::new();
    let signer = media_manifest::Signer::load(&other.0.join("other.key")).unwrap();
    let entry = signer.public_entry();
    assert_ne!(entry.kid, key_id(&[0; 32]));
    assert_eq!(
        verify_lease(&second, &PinnedKeys::new([(entry.kid, entry.public_key)]).unwrap()),
        Err(crate::media_verify::VerifyError::UnknownKey)
    );
}

#[tokio::test]
async fn lease_route_is_absent_with_exact_media_off() {
    let pool = pool().await;
    let engine = Engine::spawn().await;
    let dir = TempDir::new();
    let mut app = Arc::try_unwrap(app(pool, &engine, &dir.0.join("signing.key")).await)
        .unwrap_or_else(|_| unreachable!());
    app.media_manifests = None;
    let app = Arc::new(app);
    let response = get_path(&app, "/media/v1/policy").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(cache_control(&response), "no-store, must-revalidate");
}

/// Works A (engine series 500) and B (engine series 501, 502).
async fn two_works(pool: &db::Database) -> (String, String) {
    let mut works = Vec::new();
    for (title, keys) in [("Alpha", &[500_i64][..]), ("Beta", &[501, 502][..])] {
        let work = crate::catalog::create_work(
            pool,
            &crate::catalog::WorkInput {
                primary_title: Some(title.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for key in keys {
            sqlx::query("INSERT INTO suwayomi_series (id, title, status, source_id, lang, in_library, updated_at) VALUES ($1, $2, 'ONGOING', 'src', 'en', TRUE, '2026-01-01T00:00:00Z')")
                .bind(key).bind(format!("Series {key}")).execute(pool).await.unwrap();
            crate::catalog::upsert_source_series(
                pool,
                &work,
                "suwayomi",
                "src",
                &key.to_string(),
                None,
                false,
            )
            .await
            .unwrap();
        }
        works.push(work);
    }
    (works[0].clone(), works[1].clone())
}

/// One copy (`mc_000…0<manga>`) of one chapter of each engine series, in the active
/// generation.
async fn copies_of(pool: &db::Database, manga: &[i64]) {
    let generation = engine_keys::adopt(pool).await.unwrap();
    for id in manga {
        sqlx::query("INSERT INTO engine_manga_key(generation,manga_id,source_id,manga_key,updated_at) VALUES($1,$2,'source',$3,now())")
            .bind(generation).bind(id).bind(format!("/m{id}")).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO media_copy(id,source_id,manga_key,chapter_key,created_at) VALUES($1,'source',$2,'/c1',now())")
            .bind(copy_of(*id)).bind(format!("/m{id}")).execute(pool).await.unwrap();
    }
}

fn copy_of(manga: i64) -> String {
    format!("mc_{manga:032x}")
}

/// (copy, via_work) of every copy row.
async fn copy_rows(pool: &db::Database) -> Vec<(String, Option<String>)> {
    let mut rows: Vec<_> = media_policy::list(pool)
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.subject_kind == "copy")
        .map(|t| (t.subject_id, t.via_work))
        .collect();
    rows.sort();
    rows
}

async fn work_takedowns(pool: &db::Database) -> Vec<String> {
    let mut works: Vec<String> = media_policy::list(pool)
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.subject_kind == "work")
        .map(|t| t.subject_id)
        .collect();
    works.sort();
    works
}

#[tokio::test]
async fn a_copy_published_after_a_work_takedown_is_denied_for_good() {
    let pool = pool().await;
    let dir = TempDir::new();
    let engine = Engine::spawn().await;
    let app = app(pool.clone(), &engine, &dir.0.join("signing.key")).await;
    let (work, _) = two_works(&pool).await;
    copies_of(&pool, &[500]).await;
    let generation = engine_keys::adopt(&pool).await.unwrap();
    sqlx::query("INSERT INTO engine_chapter_key(generation,chapter_id,manga_id,source_order,chapter_key,updated_at) VALUES($1,503,500,1,'/c2',now())")
        .bind(generation).execute(&pool).await.unwrap();

    take_down(&app, Kind::Work, &work).await;
    let before_publish = epoch(&pool).await;
    let identity = CopyIdentity {
        source_id: "source".into(),
        manga_key: "/m500".into(),
        chapter_key: "/c2".into(),
    };
    let pages = [collected(&page_bytes(1, 1))];
    let (copy, revision) = store::publish(&pool, Some(generation), &identity, &pages)
        .await
        .unwrap();
    assert_eq!(revision, Some(1));
    let rows = media_policy::list(&pool).await.unwrap();
    let row = rows
        .iter()
        .find(|t| t.subject_kind == "copy" && t.subject_id == copy)
        .unwrap();
    assert_eq!(row.via_work.as_deref(), Some(work.as_str()));
    assert_eq!(row.reason, "rights request");
    assert_eq!(row.created_by.as_deref(), Some("test"));
    assert!(row.epoch > before_publish);
    let manifest = store::latest(&pool, &copy).await.unwrap().unwrap();
    assert_eq!(manifest.body.policy_epoch, row.epoch);
    assert_eq!(epoch(&pool).await, row.epoch);

    // An unchanged pass preserves the row without moving the epoch again.
    assert_eq!(
        store::publish(&pool, Some(generation), &identity, &pages).await.unwrap(),
        (copy.clone(), None)
    );
    assert_eq!(epoch(&pool).await, row.epoch);
    app.policy.invalidate();
    assert!(app.policy.current(&pool).await.unwrap().denies_copy(&copy));

    assert!(media_policy::lift(&pool, Kind::Work, &work).await.unwrap());
    assert!(copy_rows(&pool).await.is_empty());
    app.policy.invalidate();
    assert!(!app.policy.current(&pool).await.unwrap().denies_copy(&copy));
}

#[tokio::test]
async fn the_cover_warmer_skips_a_taken_down_work() {
    let pool = pool().await;
    let dir = TempDir::new();
    let engine = Engine::spawn().await;
    let app = app(pool.clone(), &engine, &dir.0.join("signing.key")).await;
    let (work, _) = two_works(&pool).await;
    sqlx::query("UPDATE suwayomi_series SET thumbnail_url = '/api/v1/manga/500/thumbnail' WHERE id = 500")
        .execute(&pool).await.unwrap();
    assert_eq!(
        cover::pending_suwayomi_cover_count(&pool, &app.cover_pool).await.unwrap(),
        1
    );
    let webp = cover::process_cover(&image(50)).unwrap();
    assert!(cover::publish_suwayomi_cover_if_missing(&pool, &app.cover_pool, 500, &webp)
        .await
        .unwrap());

    take_down(&app, Kind::Work, &work).await;
    assert!(!cover::publish_suwayomi_cover_if_missing(&pool, &app.cover_pool, 500, &webp)
        .await
        .unwrap());
    cover::publish_suwayomi_cover(&pool, &app.cover_pool, 500, &webp).await.unwrap();
    let blobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM suwayomi_cover_blob WHERE manga_id = 500")
        .fetch_one(&app.cover_pool).await.unwrap();
    assert_eq!(blobs, 0);
    assert_eq!(
        cover::pending_suwayomi_cover_count(&pool, &app.cover_pool).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn takedowns_survive_merge_and_split_and_cover_redirected_ids() {
    let pool = pool().await;
    let dir = TempDir::new();
    let engine = Engine::spawn().await;
    let app = app(pool.clone(), &engine, &dir.0.join("signing.key")).await;
    let (a, b) = two_works(&pool).await;
    copies_of(&pool, &[500, 501, 502]).await;

    take_down(&app, Kind::Work, &a).await;
    assert_eq!(copy_rows(&pool).await, vec![(copy_of(500), Some(a.clone()))]);
    let before_merge = epoch(&pool).await;
    let snapshot = app.policy.current(&pool).await.unwrap();
    assert!(snapshot.denies_work(&a) && !snapshot.denies_work(&b));
    assert!(snapshot.denies_pages(500) && snapshot.denies_engine_cover(500));
    assert!(!snapshot.denies_pages(501));

    // Merge A into B: the takedown moves onto the survivor, in the merge's transaction,
    // and A (now a redirect) stays denied.
    crate::catalog::merge_works(&pool, &a, &b).await.unwrap();
    assert!(epoch(&pool).await > before_merge);
    assert_eq!(work_takedowns(&pool).await, vec![b.clone()]);
    // Every copy the survivor now has is recorded under its takedown.
    assert_eq!(
        copy_rows(&pool).await,
        [500, 501, 502].map(|m| (copy_of(m), Some(b.clone()))).to_vec()
    );
    app.policy.invalidate();
    let snapshot = app.policy.current(&pool).await.unwrap();
    assert!(snapshot.denies_work(&a) && snapshot.denies_work(&b));
    assert!(snapshot.denies_pages(500) && snapshot.denies_pages(501));
    let (_, body) = lease(&app).await;
    let mut both = vec![a.clone(), b.clone()];
    both.sort();
    assert_eq!(body.denied_works, both);

    // The by-id doors refuse it for everyone, before the engine is asked.
    let schema = graphql::build_schema(app.clone(), false);
    let refused = schema
        .execute(format!(
            "{{ canonicalSeries(workId: \"{a}\", includeFederated: true) {{ id }} }}"
        ))
        .await;
    assert_eq!(refused.errors[0].message, "No such work", "an old id opens nothing");
    let refused = schema.execute("{ series(id: \"501\") { id } }").await;
    assert_eq!(refused.errors[0].message, "No such series");
    // Every list gate carries the takedown clause while a work is taken down.
    let gate = crate::block::BlockSet::default().with_takedowns(Some(1));
    assert!(!gate.is_empty());
    assert_ne!(gate.key(), gate.clone().with_takedowns(Some(2)).key());
    let blocked = crate::block::blocked_series_ids(
        &pool,
        &gate,
        &[b.clone(), "501".to_string(), "999".to_string()],
    )
    .await
    .unwrap();
    assert_eq!(blocked, [b.clone(), "501".to_string()].into_iter().collect());

    // Split one source off B: the new work inherits the takedown.
    let source_502: String = sqlx::query_scalar(
        "SELECT id FROM source_series WHERE source_type = 'suwayomi' AND source_key = '502'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let before_split = epoch(&pool).await;
    let split = crate::catalog::split_source_series(&pool, &b, &[source_502], None)
        .await
        .unwrap();
    assert!(epoch(&pool).await > before_split);
    let mut expected = vec![b.clone(), split.new_work_id.clone()];
    expected.sort();
    assert_eq!(work_takedowns(&pool).await, expected);
    // The copy that moved belongs to the new work's takedown now.
    assert_eq!(
        copy_rows(&pool).await,
        vec![
            (copy_of(500), Some(b.clone())),
            (copy_of(501), Some(b.clone())),
            (copy_of(502), Some(split.new_work_id.clone())),
        ]
    );

    // Lifting B lifts B only: the carried takedown is lifted by hand.
    assert!(media_policy::lift(&pool, Kind::Work, &b).await.unwrap());
    assert_eq!(
        copy_rows(&pool).await,
        vec![(copy_of(502), Some(split.new_work_id.clone()))]
    );
    app.policy.invalidate();
    let snapshot = app.policy.current(&pool).await.unwrap();
    assert!(!snapshot.denies_work(&b) && !snapshot.denies_work(&a));
    assert!(snapshot.denies_copy(&copy_of(502)) && !snapshot.denies_copy(&copy_of(501)));
    assert!(snapshot.denies_work(&split.new_work_id));
    assert!(snapshot.denies_pages(502) && !snapshot.denies_pages(501));
}

#[tokio::test]
async fn a_cover_warmed_before_a_merge_is_denied_once_the_survivor_is_taken_down() {
    let pool = pool().await;
    let dir = TempDir::new();
    let engine = Engine::spawn().await;
    let app = app(pool.clone(), &engine, &dir.0.join("signing.key")).await;
    let (a, b) = two_works(&pool).await;
    let covers = app.cover_pool.clone();
    // A's cover is in the cover file, as it was served before the merge.
    let webp = cover::process_cover(&image(50)).unwrap();
    cover::put_work_cover(&pool, &covers, &a, &webp).await.unwrap();
    sqlx::query("INSERT INTO suwayomi_cover_blob (manga_id, webp, updated_at) VALUES (501, $1, '2026-01-01')")
        .bind(&webp)
        .execute(&covers)
        .await
        .unwrap();
    crate::catalog::merge_works(&pool, &a, &b).await.unwrap();
    cover::put_work_cover(&pool, &covers, &b, &webp).await.unwrap();
    // The merge may leave A's blob behind (it is reclaimed best effort).
    sqlx::query("INSERT OR IGNORE INTO work_cover_blob (work_id, webp, version, updated_at) VALUES ($1, $2, 1, '2026-01-01')")
        .bind(&a)
        .bind(&webp)
        .execute(&covers)
        .await
        .unwrap();

    let cover = |id: String| {
        let app = app.clone();
        let covers = covers.clone();
        async move {
            serve_cover(
                State(app),
                State(CoverDb(covers)),
                UrlPath(format!("{id}.webp")),
                RawQuery(None),
            )
            .await
        }
    };
    assert_ne!(cover(a.clone()).await.status(), StatusCode::NOT_FOUND);

    take_down(&app, Kind::Work, &b).await;
    for id in [&a, &b] {
        let response = cover(id.clone()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{id}");
        assert_eq!(cache_control(&response), "no-store");
    }
    let thumbnail = serve_suwayomi_cover(&app, &covers, 501, None).await;
    assert_eq!(thumbnail.status(), StatusCode::NOT_FOUND);

    // The owner keeps no bytes: the blobs are gone, and the pointer is cleared.
    let blobs: i64 = sqlx::query_scalar(
        "SELECT (SELECT COUNT(*) FROM work_cover_blob) + (SELECT COUNT(*) FROM suwayomi_cover_blob)",
    )
    .fetch_one(&covers)
    .await
    .unwrap();
    assert_eq!(blobs, 0);
    let pointer: Option<i64> =
        sqlx::query_scalar("SELECT cover_cached_version FROM work WHERE id = $1")
            .bind(&b)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pointer, None);

    // A serving cache keyed by the old id refuses it under the new lease.
    let (_, body) = lease(&app).await;
    let mut gate = PolicyGate::default();
    let now = chrono::Utc::now().timestamp();
    gate.accept(body, now).unwrap();
    assert_eq!(
        gate.work(&a, now),
        Err(crate::media_verify::VerifyError::Denied)
    );
}
