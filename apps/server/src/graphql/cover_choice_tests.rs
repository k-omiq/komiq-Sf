//! Admin cover choice through the GraphQL API: series defaults, the cover ranking and
//! the unpicked-cover prune.

use super::sources_tests::{err, fixture, mock_engine, ok, run};
use super::*;
use serde_json::json;

const DEFAULTS: &str = "workId covers { key sourceId sourceName current isPrimary volume } \
     pickedCover uploadedCover coverSource titles { title sourceId sourceName } \
     sourceTitle titleOverride notice";

async fn seed(pool: &Database) {
    for sql in [
        "INSERT INTO work (id, primary_title, cover_file_name, created_at, updated_at) \
         VALUES ('w_def', 'Source Title', 'main.jpg', now(), now())",
        "INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
         VALUES ('ss_md', 'w_def', 'mangadex', 'mangadex', 'md-1', '2026-01-01T00:00:00Z'), \
                ('ss_a', 'w_def', 'suwayomi', 'src-a', '7', '2026-01-02T00:00:00Z')",
        "INSERT INTO work_cover (work_id, cover_file_name, volume, is_primary) \
         VALUES ('w_def', 'main.jpg', '1', true), ('w_def', 'v2.jpg', '2', false)",
        "INSERT INTO work_metadata_contribution \
           (source_series_id, field_group, slot, value, attribution, observed_at) \
         VALUES ('ss_md', 'title', 'primary', 'Source Title', 'observed', now()), \
                ('ss_md', 'title', 'alias', 'Alt Title', 'observed', now()), \
                ('ss_a', 'title', 'primary', 'Engine Title', 'observed', now())",
        "INSERT INTO work_alias (id, work_id, raw_title, normalized_title) \
         VALUES ('wa_def', 'w_def', 'Admin Alias', 'admin alias')",
    ] {
        sqlx::query(sql).execute(pool).await.unwrap();
    }
}

#[tokio::test]
async fn cover_choice_queries_and_mutations_require_an_admin() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    seed(&st.pool).await;
    for q in [
        r#"{ seriesDefaults(workId: "w_def") { workId } }"#,
        "{ coverSourceRank }",
        "{ coverPrune { enabled } }",
        r#"mutation { setSeriesDefaultCover(workId: "w_def", key: null) { workId } }"#,
        r#"mutation { setCoverSourceRank(sourceIds: ["mangadex"]) }"#,
        "mutation { setCoverPrune(enabled: true) { enabled } }",
    ] {
        assert_eq!(err(&run(&s, q, None).await), "Not authenticated", "{q}");
        assert_eq!(
            err(&run(&s, q, Some("bobtok")).await),
            "Admin access required",
            "{q}"
        );
    }
    let ranked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cover_source_rank")
        .fetch_one(&st.pool)
        .await
        .unwrap();
    assert_eq!(ranked, 0);
    assert!(!crate::cover_choice::prune_enabled(&st.pool).await);
}

#[tokio::test]
async fn series_defaults_list_the_gallery_and_titles_and_refuse_unknown_covers() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    seed(&st.pool).await;
    let q = format!(r#"{{ seriesDefaults(workId: "w_def") {{ {DEFAULTS} }} }}"#);
    let d = ok(run(&s, &q, Some("admintok")).await)["seriesDefaults"].clone();
    assert_eq!(d["workId"], "w_def");
    let keys: Vec<&str> = d["covers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["md:main.jpg", "md:v2.jpg", "src:ss_a"]);
    assert_eq!(d["covers"][0]["sourceName"], "MangaDex");
    assert_eq!(d["covers"][0]["isPrimary"], true);
    // No stored cover yet: nothing is marked current.
    assert!(d["covers"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["current"] == false));
    assert_eq!(d["pickedCover"], json!(null));
    assert_eq!(d["uploadedCover"], false);
    let titles: Vec<&str> = d["titles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        ["Source Title", "Engine Title", "Alt Title", "Admin Alias"]
    );
    assert_eq!(d["titles"][0]["sourceId"], "mangadex");
    assert_eq!(d["titles"][3]["sourceId"], json!(null));
    assert_eq!(d["sourceTitle"], "Source Title");
    assert_eq!(d["titleOverride"], json!(null));

    // The default title is the existing admin override.
    ok(run(
        &s,
        r#"mutation { updateSeriesMetadata(input: { seriesId: "w_def", title: "Engine Title" }) { title } }"#,
        Some("admintok"),
    )
    .await);
    let d = ok(run(&s, &q, Some("admintok")).await)["seriesDefaults"].clone();
    assert_eq!(d["titleOverride"], "Engine Title");

    // Automatic with no source answering: the change holds and says the cover stays.
    let auto = ok(run(
        &s,
        r#"mutation { setSeriesDefaultCover(workId: "w_def", key: null) { pickedCover notice } }"#,
        Some("admintok"),
    )
    .await);
    assert_eq!(auto["setSeriesDefaultCover"]["pickedCover"], json!(null));
    assert!(auto["setSeriesDefaultCover"]["notice"]
        .as_str()
        .unwrap()
        .starts_with("Automatic is on."));
    assert_eq!(d["notice"], json!(null));

    let pick =
        r#"mutation { setSeriesDefaultCover(workId: "w_def", key: "md:gone.jpg") { workId } }"#;
    assert_eq!(
        err(&run(&s, pick, Some("admintok")).await),
        "That cover is not offered for this series."
    );
    assert_eq!(
        err(&run(
            &s,
            r#"{ seriesDefaults(workId: "w_missing") { workId } }"#,
            Some("admintok")
        )
        .await),
        "No such work"
    );
}

#[tokio::test]
async fn the_cover_ranking_and_prune_round_trip() {
    let (engine, _) = mock_engine().await;
    let (s, st) = fixture(&engine).await;
    let set = |ids: &str| format!("mutation {{ setCoverSourceRank(sourceIds: {ids}) }}");
    let saved = ok(run(&s, &set(r#"[" src-a ", "mangadex"]"#), Some("admintok")).await);
    assert_eq!(saved["setCoverSourceRank"], json!(["src-a", "mangadex"]));
    assert_eq!(
        ok(run(&s, "{ coverSourceRank }", Some("admintok")).await)["coverSourceRank"],
        json!(["src-a", "mangadex"])
    );
    assert_eq!(
        err(&run(&s, &set(r#"["a", "a"]"#), Some("admintok")).await),
        "A source is ranked twice."
    );
    assert!(err(&run(&s, &set(r#"["bad id"]"#), Some("admintok")).await)
        .starts_with("Invalid source id"));
    let cleared = ok(run(&s, &set("[]"), Some("admintok")).await);
    assert_eq!(cleared["setCoverSourceRank"], json!([]));

    let status = "enabled covers bytes sources { sourceId covers bytes }";
    let off = ok(run(
        &s,
        &format!("{{ coverPrune {{ {status} }} }}"),
        Some("admintok"),
    )
    .await);
    assert_eq!(
        off["coverPrune"],
        json!({"enabled": false, "covers": 0, "bytes": 0.0, "sources": []})
    );
    let on = ok(run(
        &s,
        &format!("mutation {{ setCoverPrune(enabled: true) {{ {status} }} }}"),
        Some("admintok"),
    )
    .await);
    assert_eq!(on["setCoverPrune"]["enabled"], true);
    assert!(crate::cover_choice::prune_enabled(&st.pool).await);
    let off = ok(run(
        &s,
        &format!("mutation {{ setCoverPrune(enabled: false) {{ {status} }} }}"),
        Some("admintok"),
    )
    .await);
    assert_eq!(off["setCoverPrune"]["enabled"], false);
}
