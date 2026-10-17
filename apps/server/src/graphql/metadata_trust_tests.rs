//! Admin metadata bars and per-work provenance through the GraphQL API.

use super::sources_tests::{err, fixture, mock_engine, ok, run};
use super::*;
use serde_json::{json, Value};

const BARS: &str = "{ sourceMetadataBars { sourceId fieldGroup createdAt } }";
const SOURCE_FIELDS: &str = "fieldGroup state sourceId sourceName sourceSeriesId attribution";

async fn seed_work(pool: &Database, work_id: &str) -> String {
    sqlx::query(
        "INSERT INTO work (id, primary_title, created_at, updated_at) \
         VALUES ($1, 'Original title', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .bind(work_id)
    .execute(pool)
    .await
    .unwrap();
    let mapping = format!("ss_{work_id}");
    sqlx::query(
        "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
         VALUES ($1, $2, 'mangadex', 'mangadex', $2, '2026-01-01T00:00:00Z')",
    )
    .bind(&mapping)
    .bind(work_id)
    .execute(pool)
    .await
    .unwrap();
    mapping
}

fn set_bar_query(source_id: &str, group: &str, barred: bool) -> String {
    let source_id = serde_json::to_string(source_id).unwrap();
    format!(
        "mutation {{ setSourceMetadataBar(sourceId: {source_id}, fieldGroup: {group}, \
         barred: {barred}) {{ sourceId fieldGroup barred works }} }}"
    )
}

async fn set_bar(s: &ApiSchema, source_id: &str, group: &str, barred: bool) -> Value {
    ok(run(
        s,
        &set_bar_query(source_id, group, barred),
        Some("admintok"),
    )
    .await)["setSourceMetadataBar"]
        .clone()
}

async fn sources(s: &ApiSchema, work_id: &str) -> Value {
    let q = format!(r#"{{ workMetadataSources(workId: "{work_id}") {{ {SOURCE_FIELDS} }} }}"#);
    ok(run(s, &q, Some("admintok")).await)["workMetadataSources"].clone()
}

fn group<'a>(rows: &'a Value, field: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["fieldGroup"] == field)
        .unwrap_or_else(|| panic!("missing metadata group {field}: {rows}"))
}

#[tokio::test]
async fn metadata_trust_queries_and_mutation_require_an_admin() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    let mutation = set_bar_query("mangadex", "TITLE", true);
    for q in [
        BARS,
        r#"{ workMetadataSources(workId: "w_missing") { fieldGroup state } }"#,
        &mutation,
    ] {
        assert_eq!(err(&run(&s, q, None).await), "Not authenticated", "{q}");
        assert_eq!(
            err(&run(&s, q, Some("bobtok")).await),
            "Admin access required",
            "{q}"
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_metadata_bar")
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn invalid_source_ids_are_refused_without_writing() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    for source_id in [
        "".to_string(),
        "   ".into(),
        "not a source".into(),
        "source/id".into(),
        "source_id".into(),
        "é".into(),
        "a".repeat(33),
    ] {
        for barred in [true, false] {
            let q = set_bar_query(&source_id, "TITLE", barred);
            assert_eq!(
                err(&run(&s, &q, Some("admintok")).await),
                "Invalid source id: use 1-32 ASCII letters, digits, or hyphens.",
                "{source_id:?}"
            );
        }
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_metadata_bar")
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn a_bar_is_trimmed_listed_repeatable_and_clearable() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    seed_work(&st.pool, "w_bar").await;
    let expected = json!({
        "sourceId": "mangadex", "fieldGroup": "TITLE", "barred": true, "works": 1
    });
    assert_eq!(set_bar(&s, "  mangadex  ", "TITLE", true).await, expected);
    let listed = ok(run(&s, BARS, Some("admintok")).await);
    let bars = listed["sourceMetadataBars"].as_array().unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0]["sourceId"], "mangadex");
    assert_eq!(bars[0]["fieldGroup"], "TITLE");
    chrono::DateTime::parse_from_rfc3339(bars[0]["createdAt"].as_str().unwrap()).unwrap();

    assert_eq!(set_bar(&s, "mangadex", "TITLE", true).await, expected);
    assert_eq!(ok(run(&s, BARS, Some("admintok")).await), listed);
    assert_eq!(
        set_bar(&s, "mangadex", "TITLE", false).await,
        json!({ "sourceId": "mangadex", "fieldGroup": "TITLE", "barred": false, "works": 1 })
    );
    assert_eq!(
        ok(run(&s, BARS, Some("admintok")).await)["sourceMetadataBars"],
        json!([])
    );
}

#[tokio::test]
async fn metadata_sources_report_all_six_groups_and_a_barred_mangadex_title() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    let mapping = seed_work(&st.pool, "w_states").await;
    sqlx::query(
        "INSERT INTO work_metadata_contribution \
           (source_series_id, field_group, slot, value, attribution, observed_at) VALUES \
           ($1, 'title', 'primary', 'Original title', 'observed', '2026-01-01T00:00:00Z'), \
           ($1, 'description', 'primary', 'Description', 'inferred', '2026-01-01T00:00:00Z'), \
           ($1, 'credits', '', '', 'unknown', '2026-01-01T00:00:00Z'), \
           ($1, 'status', 'status', 'ongoing', 'observed', '2026-01-01T00:00:00Z')",
    )
    .bind(&mapping)
    .execute(&st.pool)
    .await
    .unwrap();
    let before = sources(&s, "w_states").await;
    assert_eq!(group(&before, "TITLE")["state"], "SOURCE");
    assert_eq!(group(&before, "TITLE")["attribution"], "OBSERVED");
    set_bar(&s, "mangadex", "TITLE", true).await;

    let rows = sources(&s, "w_states").await;
    assert_eq!(rows.as_array().unwrap().len(), 6);
    assert_eq!(
        group(&rows, "TITLE"),
        &json!({
            "fieldGroup": "TITLE", "state": "KEPT_ALL_BARRED", "sourceId": null,
            "sourceName": null, "sourceSeriesId": null, "attribution": null
        })
    );
    for (field, state, attribution) in [
        ("DESCRIPTION", "SOURCE", json!("INFERRED")),
        ("CREDITS", "SOURCE", json!("UNKNOWN")),
        ("TAGS", "KEPT_NOT_OBSERVED", Value::Null),
        ("COVER", "KEPT_NOT_OBSERVED", Value::Null),
        ("STATUS", "SOURCE", json!("OBSERVED")),
    ] {
        assert_eq!(
            group(&rows, field),
            &json!({
                "fieldGroup": field, "state": state, "sourceId": "mangadex",
                "sourceName": "MangaDex", "sourceSeriesId": mapping, "attribution": attribution
            })
        );
    }
    let title: Option<String> =
        sqlx::query_scalar("SELECT primary_title FROM work WHERE id = 'w_states'")
            .fetch_one(&st.pool)
            .await
            .unwrap();
    assert_eq!(title.as_deref(), Some("Original title"));
}

#[tokio::test]
async fn a_barred_title_uses_the_next_source_and_names_installed_sources() {
    let router = axum::Router::new().route(
        "/api/graphql",
        axum::routing::post(|| async {
            axum::Json(json!({ "data": { "sources": { "nodes": [{
                "id": "77", "name": "Internal", "displayName": "Example Scans",
                "lang": "en", "isNsfw": false
            }] } } }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let engine = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (s, st) = fixture(&engine).await;
    seed_work(&st.pool, "w_next").await;
    sqlx::query(
        "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) VALUES \
           ('ss_next77', 'w_next', 'suwayomi', '77', '901', '2025-01-01T00:00:00Z'), \
           ('ss_next88', 'w_next', 'suwayomi', '88', '902', '2025-01-02T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO work_metadata_contribution \
           (source_series_id, field_group, slot, value, attribution, observed_at) \
         VALUES ('ss_next77', 'title', 'primary', 'Next title', 'inferred', '2026-01-01T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();
    // MangaDex ranks ahead of the older engine mappings even before it is observed.
    assert_eq!(
        group(&sources(&s, "w_next").await, "TITLE")["sourceId"],
        "mangadex"
    );
    set_bar(&s, "mangadex", "TITLE", true).await;
    let rows = sources(&s, "w_next").await;
    assert_eq!(rows.as_array().unwrap().len(), 6);
    assert_eq!(
        group(&rows, "TITLE"),
        &json!({
            "fieldGroup": "TITLE", "state": "SOURCE", "sourceId": "77",
            "sourceName": "Example Scans", "sourceSeriesId": "ss_next77", "attribution": "INFERRED"
        })
    );
    set_bar(&s, "77", "TITLE", true).await;
    assert_eq!(
        group(&sources(&s, "w_next").await, "TITLE"),
        &json!({
            "fieldGroup": "TITLE", "state": "KEPT_NOT_OBSERVED", "sourceId": "88",
            "sourceName": null, "sourceSeriesId": "ss_next88", "attribution": null
        })
    );
}

#[tokio::test]
async fn metadata_sources_follow_redirects_distinguish_no_source_and_refuse_unknown_works() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    seed_work(&st.pool, "w_survivor").await;
    sqlx::query(
        "INSERT INTO work_redirect (old_id, new_id, created_at) \
         VALUES ('w_retired', 'w_survivor', '2026-01-01T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();
    assert_eq!(
        sources(&s, "w_retired").await,
        sources(&s, "w_survivor").await
    );
    sqlx::query(
        "INSERT INTO work (id, primary_title, created_at, updated_at) \
         VALUES ('w_no_source', 'No source', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .execute(&st.pool)
    .await
    .unwrap();
    let rows = sources(&s, "w_no_source").await;
    assert_eq!(rows.as_array().unwrap().len(), 6);
    for field in ["TITLE", "DESCRIPTION", "CREDITS", "TAGS", "COVER", "STATUS"] {
        assert_eq!(
            group(&rows, field),
            &json!({
                "fieldGroup": field, "state": "NO_SOURCE", "sourceId": null,
                "sourceName": null, "sourceSeriesId": null, "attribution": null
            })
        );
    }
    let q = r#"{ workMetadataSources(workId: "w_missing") { fieldGroup state } }"#;
    assert_eq!(err(&run(&s, q, Some("admintok")).await), "No such work");
}
