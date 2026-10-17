use super::*;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use std::sync::atomic::AtomicUsize;

// ---- a fake regional engine -----------------------------------------------------------

pub(crate) const FAKE_USER: &str = "komika";
pub(crate) const FAKE_PASSWORD: &str = "test-only-password";

#[derive(Clone, Debug)]
pub(crate) struct FakeManga {
    pub id: i64,
    pub source_id: String,
    pub url: String,
    pub title: String,
}

#[derive(Clone, Debug)]
pub(crate) struct FakeChapter {
    pub id: i64,
    pub manga_id: i64,
    pub url: String,
    pub order: i32,
    pub pages: usize,
}

#[derive(Default)]
pub(crate) struct FakeState {
    /// Mangas in the engine's own database.
    pub mangas: Vec<FakeManga>,
    /// Mangas only a source search finds (they enter `mangas` when found).
    pub searchable: Vec<FakeManga>,
    /// Chapters in the engine's database.
    pub chapters: Vec<FakeChapter>,
    /// Chapters only a chapter-list fetch finds.
    pub listable: Vec<FakeChapter>,
    /// Installed: (pkg, versionCode, source ids)
    pub extensions: Vec<(String, i64, Vec<String>)>,
    /// What the repository offers now: (pkg, versionCode, source ids)
    pub available: Vec<(String, i64, Vec<String>)>,
    pub stores: Vec<String>,
    pub down: bool,
    pub page_override: Option<Vec<u8>>,
    pub fail_page: Option<usize>,
    pub page_pause: Option<crate::locks::pause_points::PausePoints>,
    pub chapters_after_page: Option<(usize, Vec<FakeChapter>)>,
    pub cover_hits: Vec<i64>,
}

pub(crate) struct FakeRegional {
    pub url: String,
    pub state: Arc<Mutex<FakeState>>,
    pub calls: Arc<Mutex<HashMap<&'static str, usize>>>,
    pub page_hits: Arc<AtomicUsize>,
    pub unauthorized: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FakeRegional {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The bytes the fake serves for one page: a small PNG whose colour names the chapter
/// url and page, so a test can tell which chapter answered.
pub(crate) fn fake_page(chapter_url: &str, page: usize) -> Vec<u8> {
    let seed = chapter_url
        .bytes()
        .fold(page as u8, |acc, b| acc.wrapping_mul(31).wrapping_add(b));
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        2,
        2,
        image::Rgba([seed, page as u8, 7, 255]),
    ));
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[derive(Clone)]
struct Shared {
    state: Arc<Mutex<FakeState>>,
    calls: Arc<Mutex<HashMap<&'static str, usize>>>,
    page_hits: Arc<AtomicUsize>,
    unauthorized: Arc<AtomicUsize>,
}

fn authorized(shared: &Shared, headers: &HeaderMap) -> bool {
    let want = basic_authorization(&format!("{FAKE_USER}:{FAKE_PASSWORD}")).unwrap();
    let ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some(want.as_str());
    if !ok {
        shared.unauthorized.fetch_add(1, Ordering::SeqCst);
    }
    ok
}

async fn fake_graphql(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> axum::response::Response {
    if !authorized(&shared, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let query = request["query"].as_str().unwrap_or_default().to_string();
    let vars = &request["variables"];
    let mut state = shared.state.lock().unwrap();
    if state.down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let op: &'static str;
    let data = if query.contains("mangas(condition") {
        op = "mangas";
        let (sid, url) = (vars["sid"].as_str().unwrap(), vars["url"].as_str().unwrap());
        let nodes: Vec<Value> = state
            .mangas
            .iter()
            .filter(|m| m.source_id == sid && m.url == url)
            .map(|m| json!({ "id": m.id }))
            .collect();
        json!({ "mangas": { "nodes": nodes } })
    } else if query.contains("fetchSourceManga") {
        op = "search";
        let (sid, q) = (
            vars["source"].as_str().unwrap().to_string(),
            vars["query"].as_str().unwrap_or_default().to_string(),
        );
        let found: Vec<FakeManga> = state
            .searchable
            .iter()
            .filter(|m| m.source_id == sid && m.title.contains(&q))
            .cloned()
            .collect();
        for manga in &found {
            if !state.mangas.iter().any(|m| m.id == manga.id) {
                state.mangas.push(manga.clone());
            }
        }
        let mangas: Vec<Value> = found
            .iter()
            .map(|m| json!({ "id": m.id, "url": m.url }))
            .collect();
        json!({ "fetchSourceManga": { "mangas": mangas } })
    } else if query.contains("chapters(condition") {
        op = "chapters";
        let (mid, url) = (vars["mid"].as_i64().unwrap(), vars["url"].as_str().unwrap());
        let nodes: Vec<Value> = state
            .chapters
            .iter()
            .filter(|c| c.manga_id == mid && c.url == url)
            .map(|c| json!({ "id": c.id }))
            .collect();
        json!({ "chapters": { "nodes": nodes } })
    } else if query.contains("fetchMangaAndChapters") {
        op = "chapter_list";
        let mid = vars["id"].as_i64().unwrap();
        let listed: Vec<FakeChapter> = state
            .listable
            .iter()
            .filter(|c| c.manga_id == mid)
            .cloned()
            .collect();
        for chapter in listed {
            state.chapters.retain(|c| c.id != chapter.id);
            state.chapters.push(chapter);
        }
        let chapters: Vec<Value> = state
            .chapters
            .iter()
            .filter(|c| c.manga_id == mid)
            .map(|c| json!({ "id": c.id, "url": c.url }))
            .collect();
        json!({ "fetchMangaAndChapters": { "chapters": chapters } })
    } else if query.contains("fetchChapterPages") {
        op = "page_list";
        let id = vars["id"].as_i64().unwrap();
        let Some(chapter) = state.chapters.iter().find(|c| c.id == id).cloned() else {
            return Json(json!({ "errors": [{ "message": "chapter not found" }] }))
                .into_response();
        };
        let pages: Vec<String> = (0..chapter.pages)
            .map(|p| {
                format!(
                    "/api/v1/manga/{}/chapter/{}/page/{p}",
                    chapter.manga_id, chapter.order
                )
            })
            .collect();
        json!({ "fetchChapterPages": { "pages": pages } })
    } else if query.contains("extensions(condition") {
        op = "extensions";
        let nodes: Vec<Value> = state
            .extensions
            .iter()
            .map(|(pkg, version, sources)| {
                json!({ "pkgName": pkg, "versionCode": version, "isNsfw": false,
                    "source": { "nodes": sources.iter().map(|s| json!({ "id": s })).collect::<Vec<_>>() } })
            })
            .collect();
        json!({ "extensions": { "nodes": nodes } })
    } else if query.contains("addExtensionStore") {
        op = "add_store";
        state.stores.push(vars["url"].as_str().unwrap().to_string());
        json!({ "addExtensionStore": { "extensionStore": { "name": "Repo" } } })
    } else if query.contains("fetchExtensions(") {
        op = "refresh_extensions";
        let all: Vec<Value> = state
            .available
            .iter()
            .map(|(pkg, _, _)| json!({ "pkgName": pkg }))
            .collect();
        json!({ "fetchExtensions": { "extensions": all } })
    } else if query.contains("updateExtension(") {
        let pkg = vars["id"].as_str().unwrap().to_string();
        let patch = &vars["patch"];
        op = if patch["install"] == json!(true) { "install" } else { "update" };
        assert!(
            state.stores.len() == 1 || op == "update",
            "install before its repository was added"
        );
        assert!(
            shared.calls.lock().unwrap().get("refresh_extensions").copied().unwrap_or(0) > 0,
            "extension changed before its index was refreshed"
        );
        let offered = state.available.iter().find(|(p, _, _)| *p == pkg).cloned().unwrap();
        state.extensions.retain(|(p, _, _)| *p != pkg);
        state.extensions.push(offered);
        json!({ "updateExtension": { "extension": fake_extension_entry(&state, &pkg) } })
    } else if query.contains("extension(pkgName") {
        op = "extension";
        let pkg = vars["pkg"].as_str().unwrap().to_string();
        json!({ "extension": fake_extension_entry(&state, &pkg) })
    } else {
        panic!("unexpected fake regional query: {query}");
    };
    drop(state);
    *shared.calls.lock().unwrap().entry(op).or_default() += 1;
    Json(json!({ "data": data })).into_response()
}

fn fake_extension_entry(state: &FakeState, pkg: &str) -> Value {
    let installed = state.extensions.iter().find(|(p, _, _)| p == pkg);
    let offered = state.available.iter().find(|(p, _, _)| p == pkg);
    json!({
        "pkgName": pkg, "name": pkg, "lang": "en",
        "versionName": installed.map(|(_, v, _)| v.to_string()).unwrap_or_default(),
        "isInstalled": installed.is_some(),
        "hasUpdate": matches!((installed, offered), (Some((_, a, _)), Some((_, b, _))) if b > a),
        "isNsfw": false, "iconUrl": null, "repo": null,
    })
}

async fn fake_page_route(
    State(shared): State<Shared>,
    headers: HeaderMap,
    UrlPath((manga, order, page)): UrlPath<(i64, i32, usize)>,
) -> axum::response::Response {
    if !authorized(&shared, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let pause = shared.state.lock().unwrap().page_pause.take();
    if let Some(points) = pause {
        points.hit("page-fetch").await;
    }
    let mut state = shared.state.lock().unwrap();
    if state.down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if state.fail_page == Some(page) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Some(chapter) = state
        .chapters
        .iter()
        .find(|c| c.manga_id == manga && c.order == order && page < c.pages)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    shared.page_hits.fetch_add(1, Ordering::SeqCst);
    let bytes = state.page_override.clone().unwrap_or_else(|| fake_page(&chapter.url, page));
    if state.chapters_after_page.as_ref().is_some_and(|(at, _)| *at == page) {
        state.chapters = state.chapters_after_page.take().unwrap().1;
    }
    (
        [(header::CONTENT_TYPE, "image/png")],
        bytes,
    )
        .into_response()
}

async fn fake_cover_route(
    State(shared): State<Shared>,
    headers: HeaderMap,
    UrlPath(manga): UrlPath<i64>,
) -> axum::response::Response {
    if !authorized(&shared, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut state = shared.state.lock().unwrap();
    if state.down {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !state.mangas.iter().any(|m| m.id == manga) {
        return StatusCode::NOT_FOUND.into_response();
    }
    state.cover_hits.push(manga);
    *shared.calls.lock().unwrap().entry("cover").or_default() += 1;
    ([(header::CONTENT_TYPE, "image/png")], fake_page("/cover", 0)).into_response()
}

impl FakeRegional {
    pub(crate) async fn spawn(state: FakeState) -> Self {
        let shared = Shared {
            state: Arc::new(Mutex::new(state)),
            calls: Arc::default(),
            page_hits: Arc::default(),
            unauthorized: Arc::default(),
        };
        let routes = Router::new()
            .route("/api/graphql", post(fake_graphql))
            .route("/api/v1/manga/{m}/thumbnail", get(fake_cover_route))
            .route(
                "/api/v1/manga/{m}/chapter/{c}/page/{p}",
                get(fake_page_route),
            )
            .with_state(shared.clone());
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        Self {
            url,
            state: shared.state,
            calls: shared.calls,
            page_hits: shared.page_hits,
            unauthorized: shared.unauthorized,
            server,
        }
    }

    pub(crate) fn calls(&self, op: &str) -> usize {
        self.calls.lock().unwrap().get(op).copied().unwrap_or(0)
    }

    pub(crate) fn spec(&self, id: &str) -> EngineSpec {
        spec_with_password(id, &self.url, FAKE_PASSWORD)
    }
}

pub(crate) fn spec_with_password(id: &str, url: &str, password: &str) -> EngineSpec {
    EngineSpec {
        id: id.to_string(),
        base_url: url.to_string(),
        authorization: basic_authorization(&format!("{FAKE_USER}:{password}")).unwrap(),
    }
}

/// The standard fixture: source `src` with manga `/m/1` (regional id 501) and chapter
/// `/c/1` (regional id 9001, 3 pages), extension `pkg.src` at version 7.
pub(crate) fn fixture_state() -> FakeState {
    FakeState {
        mangas: vec![FakeManga {
            id: 501,
            source_id: "src".into(),
            url: "/m/1".into(),
            title: "Fixture One".into(),
        }],
        chapters: vec![FakeChapter {
            id: 9001,
            manga_id: 501,
            url: "/c/1".into(),
            order: 0,
            pages: 3,
        }],
        extensions: vec![("pkg.src".into(), 7, vec!["src".into()])],
        ..FakeState::default()
    }
}

pub(crate) fn keys(manga: &str, chapter: &str, title: &str) -> SourceKeys {
    SourceKeys {
        source_id: "src".into(),
        manga_key: manga.into(),
        chapter_key: chapter.into(),
        title: Some(title.into()),
    }
}

fn short(spec: &EngineSpec, ttl: Duration) -> RegionalEngine {
    RegionalEngine::with_ttls(spec, ttl, ttl, ttl)
}

// ---- configuration --------------------------------------------------------------------

fn parse(raw: &str, files: &[(&str, &str)]) -> std::result::Result<Vec<EngineSpec>, String> {
    let env: HashMap<String, String> = files
        .iter()
        .map(|(id, _)| (auth_file_env(id), format!("/secrets/{id}")))
        .collect();
    let contents: HashMap<String, String> = files
        .iter()
        .map(|(id, body)| (format!("/secrets/{id}"), body.to_string()))
        .collect();
    parse_engines(
        Some(raw),
        |name| env.get(name).cloned(),
        |path| {
            contents
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::other("no such file"))
        },
    )
}

#[test]
fn engines_parse_ids_urls_and_auth_files() {
    assert_eq!(parse_engines(None, |_| None, |_| unreachable!()), Ok(vec![]));
    assert_eq!(parse_engines(Some("  "), |_| None, |_| unreachable!()), Ok(vec![]));
    let engines = parse(
        "sg=http://10.8.0.2:4567/, eu-2=https://engine.internal:4567",
        &[("sg", "komika:secret-one\n"), ("eu-2", "user:pass:with:colons")],
    )
    .unwrap();
    assert_eq!(engines.len(), 2);
    assert_eq!(engines[0].id, "sg");
    assert_eq!(engines[0].base_url, "http://10.8.0.2:4567");
    assert_eq!(engines[1].base_url, "https://engine.internal:4567");
    assert_eq!(auth_file_env("eu-2"), "KOMIKA_ENGINE_EU_2_AUTH_FILE");
    assert_eq!(
        engines[0].authorization,
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("komika:secret-one")
        )
    );
    let printed = format!("{engines:?}");
    assert!(!printed.contains("secret-one") && !printed.contains("Basic "));
}

#[test]
fn engines_refuse_bad_entries() {
    let ok_auth = [("sg", "u:p")];
    for (raw, files, want) in [
        ("sg", &ok_auth[..], "is not `id=url`"),
        ("owner=http://a:1", &[("owner", "u:p")][..], "not `owner`"),
        ("SG=http://a:1", &[("SG", "u:p")][..], "must be 1-32"),
        ("sg=http://a:1,sg=http://b:1", &ok_auth[..], "listed twice"),
        ("sg=ftp://a:1", &ok_auth[..], "http or https"),
        ("sg=http://u:p@a:1", &ok_auth[..], "not in the URL"),
        ("sg=http://user:secret@komiq.example.com:badport", &ok_auth[..], "invalid URL"),
        ("http://user:secret@komiq.example.com:badport", &ok_auth[..], "is not `id=url`"),
        ("sg=http://a:1/api", &ok_auth[..], "without a path"),
        ("sg=http://a:1", &[][..], "KOMIKA_ENGINE_SG_AUTH_FILE"),
        ("sg=http://a:1", &[("sg", "no-colon")][..], "one `user:password` line"),
        ("sg=http://a:1", &[("sg", ":p")][..], "one `user:password` line"),
        ("sg=http://a:1", &[("sg", "u:p\nv:q")][..], "one `user:password` line"),
    ] {
        let error = parse(raw, files).unwrap_err();
        assert!(error.contains(want), "{raw}: {error}");
        assert!(!error.contains("u:p"), "{raw}: credential in error {error}");
        assert!(!error.contains("secret"), "credential in error {error}");
    }
}

#[test]
fn engine_ids() {
    for good in ["sg", "eu-2", "0", "a-b-c"] {
        assert!(valid_engine_id(good), "{good}");
    }
    for bad in ["", "owner", "-sg", "Sg", "s g", "s_g", &"a".repeat(33)] {
        assert!(!valid_engine_id(bad), "{bad}");
    }
}

#[test]
fn page_paths_stay_on_the_page_route() {
    assert_eq!(
        page_path("/api/v1/manga/1/chapter/0/page/2").unwrap(),
        "/api/v1/manga/1/chapter/0/page/2"
    );
    assert_eq!(
        page_path("http://10.8.0.2:4567/api/v1/manga/1/chapter/0/page/2?x=1").unwrap(),
        "/api/v1/manga/1/chapter/0/page/2?x=1"
    );
    for bad in [
        "/api/v1/extension/icon/x",
        "/api/v1/manga/../settings",
        "http://h/graphql",
    ] {
        assert!(page_path(bad).is_err(), "{bad}");
    }
}

// ---- client and resolver --------------------------------------------------------------

#[tokio::test]
async fn regional_client_sends_basic_auth() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let wrong = RegionalEngine::new(&spec_with_password("sg", &fake.url, "wrong"));
    assert!(wrong.manga_id(&keys("/m/1", "/c/1", "Fixture One")).await.is_err());
    assert!(fake.unauthorized.load(Ordering::SeqCst) >= 1);
    let engine = RegionalEngine::new(&fake.spec("sg"));
    assert_eq!(
        engine.manga_id(&keys("/m/1", "/c/1", "Fixture One")).await.unwrap(),
        Some(501)
    );
}

#[tokio::test]
async fn resolver_finds_ids_by_url_and_memoizes_them() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    let k = keys("/m/1", "/c/1", "Fixture One");
    let chapter = engine.chapter(&k).await.unwrap().unwrap();
    assert_eq!(chapter.chapter_id, 9001);
    assert_eq!(chapter.pages.len(), 3);
    assert_eq!(chapter.pages[2], "/api/v1/manga/501/chapter/0/page/2");
    // Memoized: no second lookup or page list.
    engine.chapter(&k).await.unwrap().unwrap();
    assert_eq!(engine.manga_id(&k).await.unwrap(), Some(501));
    assert_eq!(fake.calls("mangas"), 1);
    assert_eq!(fake.calls("chapters"), 1);
    assert_eq!(fake.calls("page_list"), 1);
    // A known manga and chapter cost no search and no chapter-list request.
    assert_eq!(fake.calls("search"), 0);
    assert_eq!(fake.calls("chapter_list"), 0);
}

#[tokio::test]
async fn resolver_memo_expires() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engine = short(&fake.spec("sg"), Duration::from_millis(80));
    let k = keys("/m/1", "/c/1", "Fixture One");
    engine.chapter(&k).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    engine.chapter(&k).await.unwrap().unwrap();
    assert_eq!(fake.calls("mangas"), 2);
    assert_eq!(fake.calls("page_list"), 2);
}

#[tokio::test]
async fn resolver_searches_the_source_for_an_unseen_manga() {
    let mut state = fixture_state();
    state.searchable.push(FakeManga {
        id: 777,
        source_id: "src".into(),
        url: "/m/2".into(),
        title: "Second Story".into(),
    });
    // Another manga with the same title must not match: the url decides.
    state.searchable.push(FakeManga {
        id: 778,
        source_id: "src".into(),
        url: "/m/2-other".into(),
        title: "Second Story".into(),
    });
    state.listable.push(FakeChapter {
        id: 9100,
        manga_id: 777,
        url: "/c/2".into(),
        order: 4,
        pages: 2,
    });
    let fake = FakeRegional::spawn(state).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    let k = keys("/m/2", "/c/2", "Second Story");
    let chapter = engine.chapter(&k).await.unwrap().unwrap();
    assert_eq!(chapter.chapter_id, 9100);
    assert_eq!(chapter.pages[1], "/api/v1/manga/777/chapter/4/page/1");
    assert_eq!(fake.calls("search"), 1);
    assert_eq!(fake.calls("chapter_list"), 1);
    assert_eq!(engine.counters.snapshot().source_requests, 3);
}

#[tokio::test]
async fn resolver_remembers_a_missing_manga_for_a_while() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engine = short(&fake.spec("sg"), Duration::from_millis(80));
    let k = keys("/m/none", "/c/none", "Nowhere");
    assert!(engine.chapter(&k).await.unwrap().is_none());
    assert!(engine.chapter(&k).await.unwrap().is_none());
    assert_eq!(fake.calls("mangas"), 1);
    assert_eq!(fake.calls("search"), 1);
    assert!(matches!(
        engine.fetch_page(&k, 0).await.unwrap(),
        RegionalPage::Missing
    ));
    assert_eq!(fake.calls("mangas"), 1);
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(engine.chapter(&k).await.unwrap().is_none());
    assert_eq!(fake.calls("mangas"), 2);
}

#[tokio::test]
async fn resolver_reports_a_missing_chapter() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    let k = keys("/m/1", "/c/unknown", "Fixture One");
    assert!(engine.chapter(&k).await.unwrap().is_none());
    assert_eq!(fake.calls("chapter_list"), 1);
    assert!(engine.chapter(&k).await.unwrap().is_none());
    assert_eq!(fake.calls("chapter_list"), 1);
}

#[tokio::test]
async fn regional_page_bytes_come_from_the_regional_engine() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    let k = keys("/m/1", "/c/1", "Fixture One");
    let RegionalPage::Page(bytes, mime) = engine.fetch_page(&k, 1).await.unwrap() else {
        panic!("expected a page");
    };
    assert_eq!(bytes.as_ref(), fake_page("/c/1", 1).as_slice());
    assert_eq!(mime, "image/png");
    assert!(matches!(
        engine.fetch_page(&k, 3).await.unwrap(),
        RegionalPage::Missing
    ));
    let counts = engine.counters.snapshot();
    assert_eq!(counts.page_fetches, 1);
    assert_eq!(counts.page_bytes, bytes.len() as u64);
    assert_eq!(fake.page_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_stale_memo_is_dropped_and_resolved_again() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    let k = keys("/m/1", "/c/1", "Fixture One");
    engine.chapter(&k).await.unwrap().unwrap();
    // The regional engine's data is replaced: the same manga and chapter get new ids
    // and a new position, and the old ones stop answering.
    {
        let mut state = fake.state.lock().unwrap();
        state.mangas[0].id = 602;
        state.chapters[0] = FakeChapter {
            id: 9500,
            manga_id: 602,
            url: "/c/1".into(),
            order: 2,
            pages: 3,
        };
    }
    let RegionalPage::Page(bytes, _) = engine.fetch_page(&k, 0).await.unwrap() else {
        panic!("expected a page after re-resolving");
    };
    assert_eq!(bytes.as_ref(), fake_page("/c/1", 0).as_slice());
    assert_eq!(fake.calls("mangas"), 2);
    assert_eq!(engine.chapter(&k).await.unwrap().unwrap().chapter_id, 9500);
    assert_eq!(engine.counters.snapshot().failures, 1);
}

#[tokio::test]
async fn a_down_engine_is_an_error_not_a_page() {
    let mut state = fixture_state();
    state.down = true;
    let fake = FakeRegional::spawn(state).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    assert!(engine.fetch_page(&keys("/m/1", "/c/1", "Fixture One"), 0).await.is_err());
    let health = engine.probe().await;
    assert!(!health.reachable);
    assert!(health.error.is_some());
}

#[tokio::test]
async fn an_out_of_range_page_does_not_reread_a_fresh_page_list() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    let k = keys("/m/1", "/c/1", "Fixture One");
    assert!(matches!(
        engine.fetch_page(&k, 0).await.unwrap(),
        RegionalPage::Page(..)
    ));
    for index in [3, 99, 9999] {
        assert!(matches!(
            engine.fetch_page(&k, index).await.unwrap(),
            RegionalPage::Missing
        ));
    }
    assert_eq!(fake.calls("page_list"), 1, "a fresh list is not read again");
    // A list older than the floor is read once more, then is fresh again.
    let mut engine = RegionalEngine::new(&fake.spec("sg"));
    engine.reread_floor = Duration::from_millis(50);
    assert!(matches!(
        engine.fetch_page(&k, 0).await.unwrap(),
        RegionalPage::Page(..)
    ));
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(matches!(
        engine.fetch_page(&k, 3).await.unwrap(),
        RegionalPage::Missing
    ));
    assert!(matches!(
        engine.fetch_page(&k, 3).await.unwrap(),
        RegionalPage::Missing
    ));
    assert_eq!(fake.calls("page_list"), 3);
}

#[tokio::test]
async fn a_failed_chapter_lookup_drops_the_remembered_manga_and_retries_once() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    // The manga memo outlives the chapter memo, as in production (1 h against 5 min).
    let engine = RegionalEngine::with_ttls(
        &fake.spec("sg"),
        Duration::from_secs(3600),
        Duration::from_millis(60),
        Duration::from_secs(600),
    );
    let k = keys("/m/1", "/c/1", "Fixture One");
    engine.chapter(&k).await.unwrap().unwrap();
    // The regional engine's data is replaced: the remembered manga id 501 is gone.
    {
        let mut state = fake.state.lock().unwrap();
        state.mangas[0].id = 602;
        state.chapters[0] = FakeChapter {
            id: 9500,
            manga_id: 602,
            url: "/c/1".into(),
            order: 2,
            pages: 3,
        };
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let RegionalPage::Page(bytes, _) = engine.fetch_page(&k, 0).await.unwrap() else {
        panic!("expected a page after looking the manga up again");
    };
    assert_eq!(bytes.as_ref(), fake_page("/c/1", 0).as_slice());
    assert_eq!(engine.manga_id(&k).await.unwrap(), Some(602));
}

#[tokio::test]
async fn regional_source_requests_spend_the_source_budget() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engines = Engines::new(&[fake.spec("sg")]);
    let budgets = Arc::new(crate::source_budget::SourceBudgets::default());
    budgets.apply_limits(&crate::scanner::merge_source_rate_limits(
        crate::config::parse_scan_source_rate_limits("src=1/3600@1"),
        Vec::new(),
    ));
    engines.set_budgets(&budgets);
    let engine = engines.engine("sg").unwrap();
    let k = keys("/m/1", "/c/1", "Fixture One");
    // The page list is the one source request here; it takes the hour's only request.
    assert!(matches!(
        engine.fetch_page(&k, 0).await.unwrap(),
        RegionalPage::Page(..)
    ));
    // A memo hit costs nothing.
    assert!(matches!(
        engine.fetch_page(&k, 1).await.unwrap(),
        RegionalPage::Page(..)
    ));
    // A list that must be read again is refused as busy, without a source request.
    engine.chapters.forget(&k.chapter_memo_key());
    assert!(matches!(
        engine.fetch_page(&k, 0).await.unwrap(),
        RegionalPage::Busy
    ));
    assert_eq!(fake.calls("page_list"), 1);
    // A caller that paid itself (ingest, the cover jobs) is not charged again.
    assert!(matches!(
        prepaid(engine.fetch_page(&k, 0)).await.unwrap(),
        RegionalPage::Page(..)
    ));
    assert_eq!(fake.calls("page_list"), 2);
}

// ---- versions and decisions -----------------------------------------------------------

fn ext(pkg: &str, version: Option<i64>) -> InstalledExtension {
    InstalledExtension {
        pkg_name: pkg.into(),
        version_code: version,
    }
}

#[test]
fn version_comparison() {
    let owner = ext("pkg.src", Some(7));
    assert_eq!(compare_versions(Some(&owner), Some(&owner)), VersionCheck::Match);
    assert_eq!(compare_versions(None, Some(&owner)), VersionCheck::OwnerUnknown);
    assert_eq!(compare_versions(Some(&owner), None), VersionCheck::NotInstalled);
    for other in [ext("pkg.src", Some(8)), ext("pkg.other", Some(7))] {
        assert!(matches!(
            compare_versions(Some(&owner), Some(&other)),
            VersionCheck::Differs { .. }
        ));
    }
    // An unknown owner version never counts as equal.
    let unknown = ext("pkg.src", None);
    assert!(matches!(
        compare_versions(Some(&unknown), Some(&unknown)),
        VersionCheck::Differs { .. }
    ));
}

fn routed(engines: &Engines, source: &str, engine: &str, strict: bool, owner: Option<i64>) {
    let mut routes = (*engines.routes()).clone();
    routes.insert(
        source.into(),
        Route {
            engine_id: engine.into(),
            strict,
            owner_extension: owner.map(|v| ext("pkg.src", Some(v))),
        },
    );
    *engines.routes.write().unwrap() = Arc::new(routes);
}

#[tokio::test]
async fn decisions_follow_health_and_versions() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engines = Engines::new(&[fake.spec("sg")]);
    assert!(matches!(engines.decide("src"), Decision::Owner));
    assert!(!engines.any_route());

    // Routed, but not probed yet: unusable.
    routed(&engines, "src", "sg", false, Some(7));
    assert!(engines.any_route());
    assert!(matches!(engines.decide("src"), Decision::Owner));
    routed(&engines, "src", "sg", true, Some(7));
    assert!(matches!(engines.decide("src"), Decision::Refuse));

    engines.probe_all().await;
    assert!(matches!(
        engines.decide("src"),
        Decision::Regional { strict: true, .. }
    ));
    routed(&engines, "src", "sg", false, Some(7));
    assert!(matches!(
        engines.decide("src"),
        Decision::Regional { strict: false, .. }
    ));

    // The owner moved to version 8: the route turns off (strict refuses).
    routed(&engines, "src", "sg", false, Some(8));
    assert!(matches!(engines.decide("src"), Decision::Owner));
    routed(&engines, "src", "sg", true, Some(8));
    assert!(matches!(engines.decide("src"), Decision::Refuse));

    // Engine goes down.
    routed(&engines, "src", "sg", false, Some(7));
    fake.state.lock().unwrap().down = true;
    engines.probe_all().await;
    assert!(matches!(engines.decide("src"), Decision::Owner));

    // A route to an engine this server does not configure.
    routed(&engines, "src", "gone", false, Some(7));
    assert!(matches!(engines.decide("src"), Decision::Owner));
    routed(&engines, "src", "gone", true, Some(7));
    assert!(matches!(engines.decide("src"), Decision::Refuse));
}

// ---- PostgreSQL -----------------------------------------------------------------------

async fn database() -> Database {
    Database::connect("sqlite::memory:").await.unwrap().foreground()
}

#[tokio::test]
async fn routes_round_trip_with_the_owner_extension() {
    let pool = database().await;
    sqlx::query(
        "INSERT INTO source_extension(source_id,pkg_name,repo_url,version_code,updated_at) \
         VALUES('src','pkg.src','https://repo.example/index.min.json',7,now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    store_route(&pool, "src", Some(("sg", true)), Some("admin")).await.unwrap();
    store_route(&pool, "other", Some(("eu-2", false)), None).await.unwrap();
    let routes = load_routes(&pool).await.unwrap();
    assert_eq!(
        routes["src"],
        Route {
            engine_id: "sg".into(),
            strict: true,
            owner_extension: Some(ext("pkg.src", Some(7))),
        }
    );
    assert_eq!(routes["other"].owner_extension, None);
    store_route(&pool, "src", Some(("eu-2", false)), None).await.unwrap();
    assert_eq!(load_routes(&pool).await.unwrap()["src"].engine_id, "eu-2");
    store_route(&pool, "src", None, None).await.unwrap();
    assert!(!load_routes(&pool).await.unwrap().contains_key("src"));
    assert!(store_route(&pool, "src", Some(("owner", false)), None).await.is_err());
    // The table refuses a bad id even past the Rust check.
    assert!(sqlx::query(
        "INSERT INTO source_engine_route(source_id,engine_id,set_at) VALUES('x','owner',now())"
    )
    .execute(&pool)
    .await
    .is_err());
    let engines = Engines::default();
    engines.reload_routes(&pool).await.unwrap();
    assert_eq!(engines.route("other").unwrap().engine_id, "eu-2");
}

#[tokio::test]
async fn stored_keys_answer_for_the_active_generation_only() {
    let pool = database().await;
    let generation = crate::engine_keys::adopt(&pool).await.unwrap();
    sqlx::query("INSERT INTO engine_manga_key(generation,manga_id,source_id,manga_key,title,updated_at) VALUES($1,7,'src','/m/1','Fixture One',now())")
        .bind(generation).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO engine_chapter_key(generation,chapter_id,manga_id,source_order,chapter_key,updated_at) VALUES($1,8,7,3,'/c/1',now())")
        .bind(generation).execute(&pool).await.unwrap();
    let want = keys("/m/1", "/c/1", "Fixture One");
    assert_eq!(keys_for_chapter(&pool, 8).await.unwrap(), Some(want.clone()));
    assert_eq!(keys_for_order(&pool, 7, 3).await.unwrap(), Some(want));
    assert_eq!(keys_for_order(&pool, 7, 4).await.unwrap(), None);
    assert_eq!(
        keys_for_manga(&pool, 7).await.unwrap(),
        Some(("src".into(), "/m/1".into(), Some("Fixture One".into())))
    );
    sqlx::query("UPDATE engine_generation SET state='quarantined' WHERE id=$1")
        .bind(generation)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(keys_for_chapter(&pool, 8).await.unwrap(), None);
    assert_eq!(keys_for_manga(&pool, 7).await.unwrap(), None);
}

/// The route table holds only the route. Regional ids have no column to live in.
#[tokio::test]
async fn the_route_table_has_no_id_column() {
    let pool = database().await;
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT column_name::text, data_type::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name='source_engine_route' \
         ORDER BY ordinal_position",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let names: Vec<&str> = columns.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["source_id", "engine_id", "strict", "set_at", "set_by"]);
    assert!(columns
        .iter()
        .all(|(_, ty)| !matches!(ty.as_str(), "integer" | "bigint" | "smallint")));
}

// ---- provisioning and route refusal ---------------------------------------------------

async fn owner_records(pool: &Database, version: i64) {
    sqlx::query(
        "INSERT INTO source_extension(source_id,pkg_name,repo_url,version_code,updated_at) \
         VALUES('src','pkg.src','https://repo.example/index.min.json',$1,now()) \
         ON CONFLICT (source_id) DO UPDATE SET version_code=EXCLUDED.version_code",
    )
    .bind(version)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn provision_installs_or_updates_to_the_owner_version() {
    let pool = database().await;
    owner_records(&pool, 8).await;
    let offered = ("pkg.src".to_string(), 8, vec!["src".to_string()]);

    // Not installed: repository added, package installed.
    let mut state = fixture_state();
    state.extensions.clear();
    state.available.push(offered.clone());
    let fake = FakeRegional::spawn(state).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    let done = provision(&engine, &pool, &["src".into(), "nothing".into()]).await.unwrap();
    assert_eq!(done[0].check, VersionCheck::Match);
    assert_eq!(done[0].note, "installed");
    assert_eq!(done[1].check, VersionCheck::OwnerUnknown);
    assert_eq!(
        fake.state.lock().unwrap().stores,
        ["https://repo.example/index.min.json"]
    );
    assert_eq!(fake.calls("install"), 1);

    // Older install: updated. Second run: nothing to do.
    let mut state = fixture_state();
    state.available.push(offered.clone());
    let fake = FakeRegional::spawn(state).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    let done = provision(&engine, &pool, &["src".into()]).await.unwrap();
    assert_eq!((done[0].check.clone(), done[0].note.as_str()), (VersionCheck::Match, "updated"));
    let again = provision(&engine, &pool, &["src".into()]).await.unwrap();
    assert_eq!(again[0].note, "already the same version");
    assert_eq!(fake.calls("update"), 1);

    // The repository only has an older version than the owner runs: reported.
    owner_records(&pool, 9).await;
    let done = provision(&engine, &pool, &["src".into()]).await.unwrap();
    assert!(matches!(done[0].check, VersionCheck::Differs { .. }));
    assert!(done[0].note.contains("route stays off"), "{}", done[0].note);

    // The regional engine is newer than the owner.
    owner_records(&pool, 6).await;
    let done = provision(&engine, &pool, &["src".into()]).await.unwrap();
    assert!(done[0].note.contains("update the owner engine"), "{}", done[0].note);
}

#[tokio::test]
async fn a_route_is_refused_until_the_versions_match() {
    let pool = database().await;
    owner_records(&pool, 7).await;
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engines = Engines::new(&[fake.spec("sg")]);
    let refusal = |source: &'static str, engine: &'static str| {
        let (engines, pool) = (&engines, &pool);
        async move { route_refusal(engines, pool, source, engine).await.unwrap() }
    };
    assert_eq!(refusal("src", "sg").await, None);
    assert!(refusal("src", "eu").await.unwrap().contains("not configured"));
    assert!(refusal("other", "sg").await.unwrap().contains("records no extension"));
    owner_records(&pool, 8).await;
    let differs = refusal("src", "sg").await.unwrap();
    assert!(differs.contains("owner pkg.src 8") && differs.contains("pkg.src 7"), "{differs}");
    fake.state.lock().unwrap().extensions.clear();
    assert!(refusal("src", "sg").await.unwrap().contains("engine provision sg src"));
    fake.state.lock().unwrap().down = true;
    assert!(refusal("src", "sg").await.unwrap().contains("not reachable"));
}

#[tokio::test]
async fn the_updater_updates_regional_engines_that_have_the_package() {
    let mut state = fixture_state();
    state.available.push(("pkg.src".into(), 8, vec!["src".into()]));
    let fake = FakeRegional::spawn(state).await;
    let empty = FakeRegional::spawn(FakeState::default()).await;
    let engines = Engines::new(&[fake.spec("sg"), empty.spec("eu")]);
    engines.probe_all().await;
    update_regional(&engines, "pkg.src").await;
    assert_eq!(fake.calls("refresh_extensions"), 1);
    assert_eq!(fake.calls("update"), 1);
    assert_eq!(empty.calls("refresh_extensions"), 0);
    assert_eq!(empty.calls("update"), 0);
    let health = engines.engine("sg").unwrap().health().unwrap();
    assert_eq!(health.installed["src"].version_code, Some(8));
}


#[tokio::test]
async fn unrouted_page_and_cover_skip_a_closed_database_and_regional_engine() {
    let pool = database().await;
    pool.close().await;
    let fake = FakeRegional::spawn(fixture_state()).await;
    let mut owner = SuwayomiClient::new(fake.url.clone(), None, None);
    owner.regional = Arc::new(Engines::new(&[fake.spec("sg")]));
    assert!(matches!(route_page(&owner, &pool, PageTarget::Chapter(8), 0).await, Routed::Owner));
    assert!(matches!(route_page(&owner, &pool, PageTarget::Order { manga_id: 7, source_order: 0 }, 0).await, Routed::Owner));
    assert!(matches!(route_cover(&owner, &pool, CoverTarget::OwnerManga(7), false).await, Routed::Owner));
    assert!(fake.calls.lock().unwrap().is_empty());
    assert_eq!(fake.page_hits.load(Ordering::SeqCst), 0);
}


#[tokio::test]
async fn enrolment_cover_keys_need_no_stored_owner_binding() {
    let pool = database().await;
    pool.close().await;
    let fake = FakeRegional::spawn(fixture_state()).await;
    let mut owner = SuwayomiClient::new(fake.url.clone(), None, None);
    owner.regional = Arc::new(Engines::new(&[fake.spec("sg")]));
    routed(&owner.regional, "src", "sg", true, Some(7));
    owner.regional.probe_all().await;
    let bytes = cover_bytes(&owner, &pool, CoverTarget::Keys {
        source_id: "src".into(), manga_key: "/m/1".into(), title: Some("Fixture One".into()),
    }, None).await.unwrap();
    assert_eq!(bytes, fake_page("/cover", 0));
    assert_eq!(fake.calls("cover"), 1);
    assert_eq!(owner.regional.engine("sg").unwrap().counters.snapshot().cover_fetches, 1);
}

#[tokio::test]
async fn chapter_list_refresh_drops_sibling_page_paths() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    let a = keys("/m/1", "/c/1", "Fixture One");
    engine.chapter(&a).await.unwrap().unwrap();
    {
        let mut state = fake.state.lock().unwrap();
        let mut moved = state.chapters[0].clone();
        moved.order = 1;
        state.listable = vec![moved, FakeChapter {
            id: 9002, manga_id: 501, url: "/c/2".into(), order: 0, pages: 3,
        }];
    }
    engine.chapter(&keys("/m/1", "/c/2", "Fixture One")).await.unwrap().unwrap();
    let RegionalPage::Page(bytes, _) = engine.fetch_page(&a, 0).await.unwrap() else {
        panic!("expected chapter A");
    };
    assert_eq!(bytes.as_ref(), fake_page("/c/1", 0).as_slice());
    assert_eq!(fake.calls("chapter_list"), 1);
    assert_eq!(fake.calls("page_list"), 3);
}

#[tokio::test]
async fn a_read_that_refreshes_the_list_itself_fetches_the_page_once() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engine = RegionalEngine::new(&fake.spec("sg"));
    {
        let mut state = fake.state.lock().unwrap();
        let first = state.chapters[0].clone();
        state.listable = vec![first, FakeChapter {
            id: 9002, manga_id: 501, url: "/c/2".into(), order: 1, pages: 3,
        }];
    }
    let RegionalPage::Page(bytes, _) =
        engine.fetch_page(&keys("/m/1", "/c/2", "Fixture One"), 0).await.unwrap()
    else {
        panic!("expected the new chapter's page");
    };
    assert_eq!(bytes.as_ref(), fake_page("/c/2", 0).as_slice());
    assert_eq!(fake.calls("chapter_list"), 1);
    assert_eq!(fake.calls("page_list"), 1);
    assert_eq!(fake.page_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_page_fetch_discards_bytes_when_a_concurrent_list_refresh_reorders_chapters() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let engine = Arc::new(RegionalEngine::new(&fake.spec("sg")));
    let a = keys("/m/1", "/c/1", "Fixture One");
    engine.chapter(&a).await.unwrap().unwrap();
    let points = crate::locks::pause_points::PausePoints::default();
    let fetched = points.arm("page-fetch");
    {
        let mut state = fake.state.lock().unwrap();
        let mut moved = state.chapters[0].clone();
        moved.order = 1;
        state.listable = vec![moved, FakeChapter {
            id: 9002, manga_id: 501, url: "/c/2".into(), order: 0, pages: 3,
        }];
        state.page_pause = Some(points);
    }
    let request = tokio::spawn({
        let engine = engine.clone();
        let a = a.clone();
        async move { engine.fetch_page(&a, 0).await }
    });
    fetched.reached().await;
    engine.chapter(&keys("/m/1", "/c/2", "Fixture One")).await.unwrap().unwrap();
    fetched.release();
    let RegionalPage::Page(bytes, _) = request.await.unwrap().unwrap() else {
        panic!("expected chapter A after retrying");
    };
    assert_eq!(bytes.as_ref(), fake_page("/c/1", 0).as_slice());
    assert_eq!(fake.calls("chapter_list"), 1);
    assert_eq!(fake.page_hits.load(Ordering::SeqCst), 2);
    let counts = engine.counters.snapshot();
    assert_eq!(counts.page_fetches, 1);
    assert_eq!(counts.page_bytes, bytes.len() as u64);
}

#[tokio::test]
async fn route_page_refuses_when_route_becomes_strict_during_fetch() {
    let pool = database().await;
    let generation = crate::engine_keys::adopt(&pool).await.unwrap();
    sqlx::query("INSERT INTO engine_manga_key(generation,manga_id,source_id,manga_key,title,updated_at) VALUES($1,7,'src','/m/1','Fixture One',now())")
        .bind(generation).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO engine_chapter_key(generation,chapter_id,manga_id,source_order,chapter_key,updated_at) VALUES($1,8,7,0,'/c/1',now())")
        .bind(generation).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO source_extension(source_id,pkg_name,repo_url,version_code,updated_at) VALUES('src','pkg.src','https://repo.example/index.min.json',7,now())")
        .execute(&pool).await.unwrap();
    store_route(&pool, "src", Some(("sg", false)), None).await.unwrap();
    let fake = FakeRegional::spawn(fixture_state()).await;
    let mut owner = SuwayomiClient::new(fake.url.clone(), None, None);
    owner.regional = Arc::new(Engines::new(&[fake.spec("sg")]));
    owner.regional.reload_routes(&pool).await.unwrap();
    owner.regional.probe_all().await;
    let owner = Arc::new(owner);
    let points = crate::locks::pause_points::PausePoints::default();
    let fetched = points.arm("page-fetch");
    fake.state.lock().unwrap().page_pause = Some(points);
    let request = tokio::spawn({
        let owner = owner.clone();
        let pool = pool.clone();
        async move { route_page(&owner, &pool, PageTarget::Chapter(8), 0).await }
    });
    fetched.reached().await;
    store_route(&pool, "src", Some(("elsewhere", true)), None).await.unwrap();
    owner.regional.reload_routes(&pool).await.unwrap();
    fake.state.lock().unwrap().fail_page = Some(0);
    fetched.release();
    assert!(matches!(request.await.unwrap(), Routed::Refuse));
    assert_eq!(owner.regional.engine("sg").unwrap().counters.snapshot().fallbacks, 0);
}

#[tokio::test]
async fn covers_resolve_a_fresh_manga_id_and_keep_absences() {
    let pool = database().await;
    pool.close().await;
    let fake = FakeRegional::spawn(fixture_state()).await;
    let mut owner = SuwayomiClient::new(fake.url.clone(), None, None);
    owner.regional = Arc::new(Engines::new(&[fake.spec("sg")]));
    routed(&owner.regional, "src", "sg", true, Some(7));
    owner.regional.probe_all().await;
    let engine = owner.regional.engine("sg").unwrap();
    let k = keys("/m/1", "", "Fixture One");
    assert_eq!(engine.manga_id(&k).await.unwrap(), Some(501));
    {
        let mut state = fake.state.lock().unwrap();
        let mut replacement = state.mangas[0].clone();
        replacement.id = 602;
        state.mangas[0].url = "/m/other".into();
        state.mangas.push(replacement);
    }
    for _ in 0..2 {
        assert!(matches!(route_cover(&owner, &pool, CoverTarget::Keys {
            source_id: k.source_id.clone(), manga_key: k.manga_key.clone(), title: k.title.clone(),
        }, false).await, Routed::Served(_)));
    }
    assert_eq!(fake.state.lock().unwrap().cover_hits, [602, 602]);
    assert_eq!(fake.calls("mangas"), 3);
    for _ in 0..2 {
        assert!(matches!(route_cover(&owner, &pool, CoverTarget::Keys {
            source_id: "src".into(), manga_key: "/m/missing".into(), title: Some("Missing".into()),
        }, false).await, Routed::Refuse));
    }
    assert_eq!(fake.calls("mangas"), 4);
    assert_eq!(fake.calls("search"), 1);
}

#[tokio::test]
async fn page_order_keys_follow_a_moved_stored_binding() {
    let pool = database().await;
    let generation = crate::engine_keys::adopt(&pool).await.unwrap();
    sqlx::query("INSERT INTO engine_manga_key(generation,manga_id,source_id,manga_key,title,updated_at) VALUES($1,7,'src','/m/1','Fixture One',now())")
        .bind(generation).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO engine_chapter_key(generation,chapter_id,manga_id,source_order,chapter_key,updated_at) VALUES($1,8,7,0,'/c/1',now()),($1,9,7,1,'/c/2',now())")
        .bind(generation).execute(&pool).await.unwrap();
    let owner = SuwayomiClient::new("http://komiq.example.com".into(), None, None);
    let target = PageTarget::Order { manga_id: 7, source_order: 0 };
    assert_eq!(page_keys(&owner, &pool, target).await.unwrap().unwrap().chapter_key, "/c/1");
    assert!(owner.regional.owner_keys.lock().unwrap().is_empty());
    sqlx::query("UPDATE engine_chapter_key SET source_order=1-source_order WHERE generation=$1")
        .bind(generation).execute(&pool).await.unwrap();
    assert_eq!(page_keys(&owner, &pool, target).await.unwrap().unwrap().chapter_key, "/c/2");
    page_keys(&owner, &pool, PageTarget::Chapter(8)).await.unwrap().unwrap();
    assert_eq!(owner.regional.owner_keys.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn regional_pages_cached_requires_live_keys_route_and_chapter() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let mut owner = SuwayomiClient::new(fake.url.clone(), None, None);
    owner.regional = Arc::new(Engines::new(&[fake.spec("sg")]));
    routed(&owner.regional, "src", "sg", true, Some(7));
    owner.regional.probe_all().await;
    let k = keys("/m/1", "/c/1", "Fixture One");
    let memo_key = (owner.read_generation().0, PageTarget::Chapter(8));
    assert!(!regional_pages_cached(&owner, 8));
    owner.regional.owner_keys.lock().unwrap().insert(memo_key, (Instant::now(), k.clone()));
    assert!(!regional_pages_cached(&owner, 8));
    let engine = owner.regional.engine("sg").unwrap();
    engine.chapter(&k).await.unwrap().unwrap();
    let calls = fake.calls.lock().unwrap().clone();
    assert!(regional_pages_cached(&owner, 8));
    assert!(!regional_pages_cached(&owner, 9));
    routed(&owner.regional, "src", "sg", true, Some(8));
    assert!(!regional_pages_cached(&owner, 8));
    routed(&owner.regional, "src", "sg", true, Some(7));
    owner.regional.owner_keys.lock().unwrap().insert(memo_key, (Instant::now() - Duration::from_secs(61), k.clone()));
    assert!(!regional_pages_cached(&owner, 8));
    owner.regional.owner_keys.lock().unwrap().insert(memo_key, (Instant::now(), k.clone()));
    engine.forget(&k);
    assert!(!regional_pages_cached(&owner, 8));
    assert_eq!(*fake.calls.lock().unwrap(), calls);
    let missing = keys("/m/1", "/c/missing", "Fixture One");
    assert!(engine.chapter(&missing).await.unwrap().is_none());
    assert!(engine.chapter_cached(&missing));
    owner.regional.owner_keys.lock().unwrap().insert(memo_key, (Instant::now(), missing));
    let calls = fake.calls.lock().unwrap().clone();
    assert!(!regional_pages_cached(&owner, 8));
    assert_eq!(*fake.calls.lock().unwrap(), calls);
}

#[tokio::test]
async fn reconcile_versions_retries_only_known_older_installed_packages() {
    let mut state = fixture_state();
    state.available.push(("pkg.src".into(), 8, vec!["src".into()]));
    state.extensions.push(("pkg.other".into(), 7, vec!["other".into()]));
    state.available.push(("pkg.other".into(), 8, vec!["other".into()]));
    let fake = FakeRegional::spawn(state).await;
    let engines = Engines::new(&[fake.spec("sg")]);
    engines.probe_all().await;
    routed(&engines, "new-source", "sg", true, Some(8));
    let mut routes = (*engines.routes()).clone();
    routes.insert("other".into(), Route {
        engine_id: "sg".into(), strict: true, owner_extension: Some(ext("pkg.other", Some(8))),
    });
    *engines.routes.write().unwrap() = Arc::new(routes);
    reconcile_versions(&engines).await;
    assert_eq!(fake.calls("refresh_extensions"), 1);
    assert_eq!(fake.calls("update"), 2);
    routed(&engines, "src", "sg", true, Some(8));
    assert!(matches!(engines.decide("src"), Decision::Regional { .. }));
    reconcile_versions(&engines).await;
    routed(&engines, "src", "sg", true, Some(7));
    reconcile_versions(&engines).await;
    routed(&engines, "src", "sg", true, None);
    reconcile_versions(&engines).await;
    let mut routes = (*engines.routes()).clone();
    routes.get_mut("src").unwrap().owner_extension = Some(ext("pkg.src", None));
    *engines.routes.write().unwrap() = Arc::new(routes);
    reconcile_versions(&engines).await;
    let engine = engines.engine("sg").unwrap();
    engine.health.write().unwrap().as_mut().unwrap().installed.get_mut("src").unwrap().version_code = None;
    routed(&engines, "src", "sg", true, Some(9));
    reconcile_versions(&engines).await;
    fake.state.lock().unwrap().extensions.clear();
    engine.probe().await;
    reconcile_versions(&engines).await;
    assert_eq!(fake.calls("refresh_extensions"), 1);
    assert_eq!(fake.calls("update"), 2);
    assert_eq!(fake.calls("install"), 0);
}

#[tokio::test]
async fn route_reload_serializes_the_query_and_publish() {
    let pool = database().await;
    store_route(&pool, "src", Some(("sg", false)), None).await.unwrap();
    let engines = Arc::new(Engines::default());
    let guard = engines.reload.lock().await;
    let reload_engines = engines.clone();
    let reload_pool = pool.clone();
    let mut reload = tokio::spawn(async move { reload_engines.reload_routes(&reload_pool).await });
    assert!(tokio::time::timeout(Duration::from_millis(100), &mut reload).await.is_err());
    store_route(&pool, "src", Some(("sg", true)), None).await.unwrap();
    drop(guard);
    reload.await.unwrap().unwrap();
    assert!(engines.route("src").unwrap().strict);
}
