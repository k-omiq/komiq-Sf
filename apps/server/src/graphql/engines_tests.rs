use super::sources_tests::{err, ok, run};
use super::*;
use crate::engine_route::tests::{fixture_state, FakeRegional};
use crate::engine_route::{load_routes, Engines};
use std::sync::atomic::Ordering;
use std::sync::Arc;

const ROUTE_FIELDS: &str = "sourceId engineId strict active versionCheck";

async fn fixture(fake: &FakeRegional) -> (ApiSchema, Arc<AppState>) {
    let (schema, mut state) = sources_tests::fixture(&fake.url).await;
    drop(schema);
    let cover_dir = crate::cover_store::test_support::TestDir::new();
    let st = Arc::get_mut(&mut state).unwrap();
    st.cover_pool = crate::cover_store::test_support::store(&cover_dir);
    st.suwayomi.regional = Arc::new(Engines::new(&[fake.spec("sg")]));
    sqlx::query(
        "INSERT INTO source_extension(source_id,pkg_name,repo_url,version_code,updated_at) \
         VALUES('src','pkg.src','https://komiq.example.com/index.min.json',7,now())",
    )
    .execute(&st.pool)
    .await
    .unwrap();
    (
        schema_builder(state.clone(), false)
            .data(cover_dir)
            .finish(),
        state,
    )
}

fn set_route(engine: &str, strict: bool) -> String {
    format!("mutation {{ setSourceEngine(sourceId: \"src\", engineId: {engine}, strict: {strict}) {{ {ROUTE_FIELDS} }} }}")
}

#[tokio::test]
async fn engine_admin_queries_and_mutation_refuse_readers_and_anonymous() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let (schema, st) = fixture(&fake).await;
    for token in [None, Some("bobtok")] {
        for query in [
            "{ engines { id } }".to_string(),
            format!("{{ sourceEngineRoute(sourceId: \"src\") {{ {ROUTE_FIELDS} }} }}"),
            set_route("\"sg\"", true),
        ] {
            assert!(!run(&schema, &query, token).await.errors.is_empty());
        }
    }
    assert!(load_routes(&st.pool).await.unwrap().is_empty());
    assert_eq!(fake.calls("extensions"), 0);
}

#[tokio::test]
async fn engine_version_refusal_writes_nothing() {
    let mut state = fixture_state();
    state.extensions[0].1 = 8;
    let fake = FakeRegional::spawn(state).await;
    let (schema, st) = fixture(&fake).await;
    let response = run(&schema, &set_route("\"sg\"", true), Some("admintok")).await;
    assert!(err(&response).contains("extension versions differ"));
    assert!(load_routes(&st.pool).await.unwrap().is_empty());
    assert!(!st.suwayomi.regional.any_route());
}

#[tokio::test]
async fn engine_route_stores_admin_shows_active_and_null_removes() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let (schema, st) = fixture(&fake).await;
    let before = ok(run(
        &schema,
        "{ engines { id reachable checkedAt } }",
        Some("admintok"),
    )
    .await);
    assert_eq!(before["engines"][0]["id"], "owner");
    assert_eq!(before["engines"][0]["reachable"], true);
    assert_eq!(before["engines"][1]["reachable"], false);
    assert!(before["engines"][1]["checkedAt"].is_null());
    let route = ok(run(&schema, &set_route("\"sg\"", true), Some("admintok")).await);
    assert_eq!(
        route["setSourceEngine"],
        serde_json::json!({
            "sourceId": "src", "engineId": "sg", "strict": true,
            "active": true, "versionCheck": "MATCH",
        })
    );
    let saved: (String, bool, Option<String>) = sqlx::query_as(
        "SELECT engine_id,strict,set_by FROM source_engine_route WHERE source_id='src'",
    )
    .fetch_one(&st.pool)
    .await
    .unwrap();
    assert_eq!(saved, ("sg".into(), true, Some("admin".into())));
    st.suwayomi
        .regional
        .engine("sg")
        .unwrap()
        .counters
        .page_bytes
        .store(u64::MAX, Ordering::Relaxed);
    let status = ok(run(&schema, "{ engines { id reachable checkedAt error counters { lookups sourceRequests pageFetches pageBytes coverFetches failures fallbacks } routedSources { sourceId strict active versionCheck ownerVersion regionalVersion } } sourceEngineRoute(sourceId: \"src\") { engineId active } }", Some("admintok")).await);
    let regional = &status["engines"][1];
    assert_eq!(regional["id"], "sg");
    assert_eq!(regional["reachable"], true);
    assert!(regional["checkedAt"].as_str().is_some());
    assert!(regional["error"].is_null());
    assert_eq!(regional["counters"]["pageBytes"], u64::MAX.to_string());
    assert_eq!(
        regional["routedSources"][0],
        serde_json::json!({
            "sourceId": "src", "strict": true, "active": true,
            "versionCheck": "MATCH", "ownerVersion": "7", "regionalVersion": "7",
        })
    );
    assert_eq!(status["sourceEngineRoute"]["active"], true);
    let removed = ok(run(&schema, &set_route("null", false), Some("admintok")).await);
    assert!(removed["setSourceEngine"].is_null());
    assert!(load_routes(&st.pool).await.unwrap().is_empty());
    assert!(!st.suwayomi.regional.any_route());
    let after = ok(run(&schema, "{ engines { routedSources { sourceId } } sourceEngineRoute(sourceId: \"src\") { engineId } }", Some("admintok")).await);
    assert!(after["sourceEngineRoute"].is_null());
    assert_eq!(after["engines"][1]["routedSources"], serde_json::json!([]));
}

#[tokio::test]
async fn engine_route_owner_alias_removes_and_default_is_not_strict() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let (schema, st) = fixture(&fake).await;
    let saved = ok(run(
        &schema,
        "mutation { setSourceEngine(sourceId: \"src\", engineId: \"sg\") { strict } }",
        Some("admintok"),
    )
    .await);
    assert_eq!(saved["setSourceEngine"]["strict"], false);
    let removed = ok(run(&schema, &set_route("\"owner\"", false), Some("admintok")).await);
    assert!(removed["setSourceEngine"].is_null());
    assert!(load_routes(&st.pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn engine_route_validates_source_id_and_unknown_engine() {
    let fake = FakeRegional::spawn(fixture_state()).await;
    let (schema, st) = fixture(&fake).await;
    for source in ["".to_string(), " ".into(), "x".repeat(65)] {
        let query = format!(
            "mutation {{ setSourceEngine(sourceId: \"{source}\", engineId: null) {{ sourceId }} }}"
        );
        assert!(err(&run(&schema, &query, Some("admintok")).await).contains("sourceId"));
    }
    let unknown = run(&schema, &set_route("\"gone\"", false), Some("admintok")).await;
    assert!(err(&unknown).contains("not configured"));
    assert!(load_routes(&st.pool).await.unwrap().is_empty());
}
