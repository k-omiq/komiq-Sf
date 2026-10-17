use super::tests::{
    drain_work_chapter_fixture, exec, fixture_entry_id, seed_work_chapter_graphql_fixture,
    setup_full,
};
use super::*;
use serde_json::{json, Value};

async fn fixture() -> (ApiSchema, Database, String) {
    let (schema, pool) = setup_full(100).await;
    let work = seed_work_chapter_graphql_fixture(&pool).await;
    drain_work_chapter_fixture(&pool).await;
    (schema, pool, work)
}

async fn chapter_work(schema: &ApiSchema, id: &str, token: Option<&str>) -> Value {
    let response = exec(
        schema,
        &format!(r#"{{ chapterWork(chapterId:"{id}") {{ workId entryId }} }}"#),
        token,
        "chapter-work",
    )
    .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    response.data.into_json().unwrap()["chapterWork"].clone()
}

#[tokio::test]
async fn canonical_and_numeric_copies_return_their_work_and_entry_anonymously() {
    let (schema, pool, work) = fixture().await;
    let entry = fixture_entry_id(&pool, &work, "100").await;
    for id in ["entry-one", "9201"] {
        let before = pool.statements_sent();
        assert_eq!(
            chapter_work(&schema, id, None).await,
            json!({"workId":work,"entryId":entry})
        );
        assert_eq!(pool.statements_sent() - before, 1);
    }
}

#[tokio::test]
async fn unlinked_copies_return_their_work_without_an_entry() {
    let (schema, pool, work) = fixture().await;
    sqlx::query(
        "DELETE FROM work_chapter_link WHERE chapter_id IN \
         (SELECT id FROM chapter WHERE external_id IN ('entry-one', '9201'))",
    )
    .execute(&pool)
    .await
    .unwrap();
    for id in ["entry-one", "9201"] {
        assert_eq!(
            chapter_work(&schema, id, None).await,
            json!({"workId":work,"entryId":null})
        );
    }
}

#[tokio::test]
async fn unknown_ids_and_engine_copies_without_a_work_return_null() {
    let (schema, pool, work) = fixture().await;
    for id in [
        "unknown-copy",
        "987654321",
        "",
        "999999999999999999999999999",
    ] {
        assert!(chapter_work(&schema, id, None).await.is_null(), "{id}");
    }
    sqlx::query("DELETE FROM source_series WHERE work_id = $1 AND source_type = 'suwayomi'")
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();
    assert!(chapter_work(&schema, "9201", None).await.is_null());
}

#[tokio::test]
async fn engine_copies_without_a_mirror_resolve_through_their_series() {
    let (schema, pool, work) = fixture().await;
    sqlx::query("DELETE FROM chapter WHERE external_id = '9201'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        chapter_work(&schema, "9201", None).await,
        json!({"workId":work,"entryId":null})
    );
}

#[tokio::test]
async fn numeric_mirrors_still_resolve_when_the_engine_copy_is_not_cached() {
    let (schema, pool, work) = fixture().await;
    let entry = fixture_entry_id(&pool, &work, "100").await;
    sqlx::query("DELETE FROM suwayomi_chapter WHERE id = 9201")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        chapter_work(&schema, "9201", None).await,
        json!({"workId":work,"entryId":entry})
    );
}

#[tokio::test]
async fn numeric_copies_ignore_mirrors_from_a_different_engine_series() {
    let (schema, pool, work) = fixture().await;
    let entry = fixture_entry_id(&pool, &work, "100").await;
    sqlx::query(
        "INSERT INTO source_series \
         (id, work_id, source_type, source_id, source_key, created_at) \
         VALUES ('000-stale-source', 'w_s1', 'suwayomi', 'stale-source', '780', now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    catalog::upsert_chapter(
        &pool,
        "000-stale-source",
        &catalog::ChapterInput {
            external_id: "9201".into(),
            number: Some("1".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        chapter_work(&schema, "9201", None).await,
        json!({"workId":work,"entryId":entry})
    );
    sqlx::query(
        "DELETE FROM chapter WHERE external_id = '9201' AND source_series_id <> '000-stale-source'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        chapter_work(&schema, "9201", None).await,
        json!({"workId":work,"entryId":null})
    );
}

#[tokio::test]
async fn split_copies_report_the_new_work_and_entry() {
    for (source_type, moved, stayed) in [
        ("suwayomi", "9201", "entry-one"),
        ("mangadex", "entry-one", "9201"),
    ] {
        let (schema, pool, work) = fixture().await;
        let original_entry = fixture_entry_id(&pool, &work, "100").await;
        let source: String = sqlx::query_scalar(
            "SELECT id FROM source_series WHERE work_id = $1 AND source_type = $2",
        )
        .bind(&work)
        .bind(source_type)
        .fetch_one(&pool)
        .await
        .unwrap();
        let split = catalog::split_source_series(&pool, &work, &[source], None)
            .await
            .unwrap();
        let entry = fixture_entry_id(&pool, &split.new_work_id, "100").await;
        assert_ne!(entry, original_entry);
        assert_eq!(
            chapter_work(&schema, moved, None).await,
            json!({"workId":split.new_work_id,"entryId":entry})
        );
        assert_eq!(
            chapter_work(&schema, stayed, None).await,
            json!({"workId":work,"entryId":original_entry})
        );
    }
}

#[tokio::test]
async fn merged_copies_report_the_surviving_work_and_entry() {
    let (schema, pool, work) = fixture().await;
    catalog::merge_works(&pool, &work, "w_s1").await.unwrap();
    let entry = fixture_entry_id(&pool, "w_s1", "100").await;
    assert_eq!(
        catalog::redirect_work_id(&pool, &work).await.unwrap(),
        Some("w_s1".into())
    );
    for id in ["entry-one", "9201"] {
        assert_eq!(
            chapter_work(&schema, id, None).await,
            json!({"workId":"w_s1","entryId":entry})
        );
    }
}

#[tokio::test]
async fn nsfw_gate_uses_the_effective_flag_and_the_viewers_preference() {
    let (schema, pool, work) = fixture().await;
    let entry = fixture_entry_id(&pool, &work, "100").await;
    for (base, override_flag) in [(false, Some(true)), (true, None), (true, Some(false))] {
        sqlx::query("UPDATE work SET is_nsfw = $1, is_nsfw_override = $2 WHERE id = $3")
            .bind(base)
            .bind(override_flag)
            .bind(&work)
            .execute(&pool)
            .await
            .unwrap();
        let gated = override_flag.unwrap_or(base);
        for id in ["entry-one", "9201"] {
            for token in [None, Some("bobtok"), Some("admintok")] {
                let result = chapter_work(&schema, id, token).await;
                if gated {
                    assert!(result.is_null());
                } else {
                    assert_eq!(result, json!({"workId":work,"entryId":entry}));
                }
            }
        }
    }
    sqlx::query("UPDATE work SET is_nsfw = false, is_nsfw_override = true WHERE id = $1")
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET show_nsfw = true WHERE username = 'bob'")
        .execute(&pool)
        .await
        .unwrap();
    for id in ["entry-one", "9201"] {
        assert_eq!(
            chapter_work(&schema, id, Some("bobtok")).await,
            json!({"workId":work,"entryId":entry})
        );
        assert!(chapter_work(&schema, id, None).await.is_null());
    }
}

#[tokio::test]
async fn viewer_progress_resolves_a_numeric_series_id_like_saves_do() {
    let (schema, _pool, work) = fixture().await;
    let save = exec(
        &schema,
        &format!(
            r#"mutation {{ saveProgress(chapterId:"entry-one",workId:"{work}",lastPageRead:3,read:true,revision:0) {{ status revision }} }}"#
        ),
        Some("bobtok"),
        "chapter-work",
    )
    .await;
    assert!(save.errors.is_empty(), "{:?}", save.errors);
    assert_eq!(
        save.data.into_json().unwrap()["saveProgress"],
        json!({"status":"APPLIED","revision":1})
    );
    for id in [work.as_str(), "779"] {
        let response = exec(
            &schema,
            &format!(r#"{{ viewerProgress(workId:"{id}") {{ revision }} }}"#),
            Some("bobtok"),
            "chapter-work",
        )
        .await;
        assert!(response.errors.is_empty(), "{:?}", response.errors);
        assert_eq!(
            response.data.into_json().unwrap()["viewerProgress"],
            json!({"revision":1}),
            "{id}"
        );
    }
}
