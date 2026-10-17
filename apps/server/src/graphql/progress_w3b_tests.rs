use super::tests::{
    bob_progress_row, drain_work_chapter_fixture, exec, fixture_entry_id,
    seed_work_chapter_graphql_fixture, setup_full,
};
use super::*;
use serde_json::{json, Value};

const BOB: &str = "ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c";

async fn fixture(ready: bool) -> (ApiSchema, Database, String) {
    let (schema, pool) = setup_full(100).await;
    let work = seed_work_chapter_graphql_fixture(&pool).await;
    if ready {
        drain_work_chapter_fixture(&pool).await;
    }
    (schema, pool, work)
}

async fn query(schema: &ApiSchema, text: &str, token: Option<&str>) -> Value {
    let response = exec(schema, text, token, "w3b-progress").await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    response.data.into_json().unwrap()
}

async fn bob(schema: &ApiSchema, text: &str) -> Value {
    query(schema, text, Some("bobtok")).await
}

async fn save(
    schema: &ApiSchema,
    work: &str,
    id: &str,
    page: i32,
    read: bool,
    revision: i64,
) -> Value {
    bob(schema, &format!(
        r#"mutation {{ saveProgress(chapterId:"{id}",workId:"{work}",lastPageRead:{page},read:{read},revision:{revision}) {{ status revision workId }} }}"#,
    )).await["saveProgress"].clone()
}

async fn tick(schema: &ApiSchema, work: &str, id: &str, read: bool, revision: i64) -> Value {
    bob(schema, &format!(
        r#"mutation {{ saveChapterRead(chapterId:"{id}",workId:"{work}",read:{read},revision:{revision}) {{ status revision workId }} }}"#,
    )).await["saveChapterRead"].clone()
}

async fn viewer(schema: &ApiSchema, work: &str) -> Value {
    bob(
        schema,
        &format!(r#"{{ viewerProgress(workId:"{work}") {{ revision throughEntryId }} }}"#),
    )
    .await["viewerProgress"]
        .clone()
}

async fn lists(schema: &ApiSchema, work: &str) -> Value {
    bob(
        schema,
        &format!(
            r#"{{
        canonicalChapters(workId:"{work}") {{ id read lastPageRead }}
        chapters(seriesId:"779") {{ id read lastPageRead }}
        aggregatedChapters(workId:"{work}") {{ key entryId readAnywhere }}
        seriesPage(workId:"{work}") {{
            viewerProgress {{ revision throughEntryId }}
            spineChapters {{ id read lastPageRead }}
            sources {{ chapters {{ id read lastPageRead }} }}
            aggregated {{ key entryId readAnywhere }}
        }}
    }}"#
        ),
    )
    .await
}

fn copy<'a>(rows: &'a Value, id: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap()
}

fn row<'a>(rows: &'a Value, key: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["key"] == key)
        .unwrap()
}

async fn mark(pool: &Database, entry: &str) -> Option<bool> {
    sqlx::query_scalar(
        "SELECT read FROM work_chapter_mark WHERE user_id = $1 AND work_chapter_id = $2",
    )
    .bind(crate::db_types::uuid(BOB).unwrap())
    .bind(entry)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn ready_finish_raises_pointer_across_sources_without_catch_up_and_skips_specials() {
    let (schema, pool, work) = fixture(true).await;
    let entry = fixture_entry_id(&pool, &work, "300").await;
    let result = save(&schema, &work, "entry-three", 20, true, 0).await;
    assert_eq!(
        result,
        json!({"status":"APPLIED", "revision":1, "workId":work})
    );
    assert_eq!(
        viewer(&schema, &work).await,
        json!({"revision":1,"throughEntryId":entry})
    );
    for id in [
        "entry-zero",
        "entry-one",
        "entry-two",
        "9200",
        "9201",
        "9202",
        "9203",
    ] {
        assert!(
            bob_progress_row(&pool, id).await.is_none(),
            "no catch-up row for {id}"
        );
    }
    let data = lists(&schema, &work).await;
    for id in ["entry-zero", "entry-one", "entry-two", "entry-three"] {
        assert_eq!(copy(&data["canonicalChapters"], id)["read"], true);
        assert_eq!(copy(&data["seriesPage"]["spineChapters"], id)["read"], true);
    }
    for id in ["9200", "9201", "9202", "9203"] {
        let c = copy(&data["chapters"], id);
        assert_eq!(c["read"], true);
        assert_eq!(c["lastPageRead"], 0, "resume positions stay per copy");
        assert_eq!(
            copy(&data["seriesPage"]["sources"][1]["chapters"], id)["read"],
            true
        );
    }
    for field in [
        &data["aggregatedChapters"],
        &data["seriesPage"]["aggregated"],
    ] {
        for key in ["0", "100", "200", "300"] {
            assert_eq!(row(field, key)["readAnywhere"], true);
        }
        assert_eq!(row(field, "x:entry-extra")["readAnywhere"], false);
    }
    assert_eq!(
        copy(&data["canonicalChapters"], "entry-extra")["read"],
        false
    );
    assert_eq!(
        data["seriesPage"]["viewerProgress"],
        viewer(&schema, &work).await
    );

    assert_eq!(
        save(&schema, &work, "entry-extra", 8, true, 1).await["revision"],
        2
    );
    let extra = fixture_entry_id(&pool, &work, "x:entry-extra").await;
    assert_eq!(mark(&pool, &extra).await, Some(true));
    assert_eq!(
        viewer(&schema, &work).await["throughEntryId"],
        entry,
        "P never moves backward"
    );
}

#[tokio::test]
async fn ready_tick_and_untick_keep_entry_marks_and_clear_both_stores() {
    let (schema, pool, work) = fixture(true).await;
    let one = fixture_entry_id(&pool, &work, "100").await;
    assert_eq!(
        tick(&schema, &work, "entry-one", true, 0).await["revision"],
        1
    );
    assert_eq!(mark(&pool, &one).await, Some(true));
    assert!(viewer(&schema, &work).await["throughEntryId"].is_null());
    assert_eq!(
        save(&schema, &work, "9201", 4, true, 1).await["revision"],
        2
    );
    assert_eq!(
        tick(&schema, &work, "entry-one", false, 2).await["revision"],
        3
    );
    assert_eq!(mark(&pool, &one).await, Some(false));
    for id in ["entry-one", "9201"] {
        assert!(!bob_progress_row(&pool, id).await.unwrap().1);
    }
    let unticks: i64 =
        sqlx::query_scalar("SELECT count(*) FROM progress_untick WHERE user_id = $1")
            .bind(crate::db_types::uuid(BOB).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unticks, 0);
    // P already implies this entry, so an explicit tick removes its override.
    assert_eq!(tick(&schema, &work, "9201", true, 3).await["revision"], 4);
    assert_eq!(mark(&pool, &one).await, None);
    assert_eq!(
        tick(&schema, &work, "entry-extra", true, 4).await["revision"],
        5
    );
    let extra = fixture_entry_id(&pool, &work, "x:entry-extra").await;
    assert_eq!(
        mark(&pool, &extra).await,
        Some(true),
        "specials always need an explicit tick"
    );
}

#[tokio::test]
async fn untick_beats_stale_true_rows_higher_finishes_and_stale_same_copy_replays() {
    let (schema, pool, work) = fixture(true).await;
    save(&schema, &work, "entry-three", 20, true, 0).await;
    tick(&schema, &work, "entry-one", false, 1).await;
    sqlx::query("INSERT INTO suwayomi_progress (user_id,chapter_id,series_id,read,updated_at) VALUES ($1,'9201','779',true,now())")
        .bind(crate::db_types::uuid(BOB).unwrap()).execute(&pool).await.unwrap();
    let data = lists(&schema, &work).await;
    assert_eq!(copy(&data["chapters"], "9201")["read"], false);
    assert_eq!(copy(&data["canonicalChapters"], "entry-one")["read"], false);
    let old = bob(
        &schema,
        r#"mutation { setProgress(chapterId:"entry-three",lastPageRead:20,read:true) }"#,
    )
    .await;
    assert_eq!(old["setProgress"], true);
    assert_eq!(
        save(&schema, &work, "entry-one", 20, true, 1).await["status"],
        "REFUSED"
    );
    let data = lists(&schema, &work).await;
    assert_eq!(
        row(&data["seriesPage"]["aggregated"], "100")["readAnywhere"],
        false
    );
    assert_eq!(copy(&data["chapters"], "9201")["read"], false);
}

#[tokio::test]
async fn revisions_refuse_stale_finishes_and_allow_only_forward_pages() {
    let (schema, pool, work) = fixture(true).await;
    assert_eq!(
        save(&schema, &work, "entry-one", 3, false, 0).await["revision"],
        1
    );
    let original = bob_progress_row(&pool, "entry-one").await.unwrap();
    let refused = save(&schema, &work, "entry-one", 20, true, 0).await;
    assert_eq!(refused["status"], "REFUSED");
    assert_eq!(refused["revision"], 1);
    assert_eq!(
        bob_progress_row(&pool, "entry-one").await.unwrap(),
        original
    );
    assert_eq!(
        save(&schema, &work, "entry-one", 7, false, 0).await,
        json!({"status":"PAGE_ONLY","revision":1,"workId":work})
    );
    let forward = bob_progress_row(&pool, "entry-one").await.unwrap();
    assert_eq!((forward.0, forward.1), (7, false));
    assert_eq!(
        save(&schema, &work, "entry-one", 2, false, 0).await["status"],
        "REFUSED"
    );
    assert_eq!(bob_progress_row(&pool, "entry-one").await.unwrap(), forward);
    assert_eq!(viewer(&schema, &work).await["revision"], 1);
    assert!(viewer(&schema, &work).await["throughEntryId"].is_null());
    // A page-only insert also leaves the work revision alone.
    assert_eq!(
        save(&schema, &work, "entry-two", 5, false, 0).await["status"],
        "PAGE_ONLY"
    );
    assert_eq!(viewer(&schema, &work).await["revision"], 1);
    // Even a stale forward save of a finished copy cannot clear its read bit.
    save(&schema, &work, "entry-three", 10, true, 1).await;
    assert_eq!(
        save(&schema, &work, "entry-three", 12, false, 1).await["status"],
        "PAGE_ONLY"
    );
    let finished = bob_progress_row(&pool, "entry-three").await.unwrap();
    assert_eq!((finished.0, finished.1), (12, true));
}

#[tokio::test]
async fn two_devices_refresh_the_work_revision_before_retrying() {
    let (schema, pool, work) = fixture(true).await;
    let device_a = viewer(&schema, &work).await["revision"].as_i64().unwrap();
    let device_b = device_a;
    assert_eq!(
        tick(&schema, &work, "entry-one", true, device_a).await["revision"],
        1
    );
    assert_eq!(
        tick(&schema, &work, "entry-two", true, device_b).await["status"],
        "REFUSED"
    );
    assert!(bob_progress_row(&pool, "entry-two").await.is_none());
    let refreshed = viewer(&schema, &work).await["revision"].as_i64().unwrap();
    assert_eq!(
        tick(&schema, &work, "entry-two", true, refreshed).await["revision"],
        2
    );
}

#[tokio::test]
async fn no_op_saves_preserve_copy_and_work_xmin_in_both_stores() {
    let (schema, pool, work) = fixture(true).await;
    for id in ["entry-one", "9201"] {
        let revision = viewer(&schema, &work).await["revision"].as_i64().unwrap();
        let first = save(&schema, &work, id, 6, false, revision).await;
        let revision = first["revision"].as_i64().unwrap();
        let before = bob_progress_row(&pool, id).await.unwrap();
        let work_before: (String, chrono::DateTime<Utc>) = sqlx::query_as(
            "SELECT xmin::text, updated_at FROM work_progress WHERE user_id = $1 AND work_id = $2",
        )
        .bind(crate::db_types::uuid(BOB).unwrap())
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            save(&schema, &work, id, 6, false, revision).await["revision"],
            revision
        );
        let after = bob_progress_row(&pool, id).await.unwrap();
        assert_eq!(
            (after.0, after.1, after.2, after.3),
            (before.0, before.1, before.2, before.3)
        );
        let work_after: (String, chrono::DateTime<Utc>) = sqlx::query_as(
            "SELECT xmin::text, updated_at FROM work_progress WHERE user_id = $1 AND work_id = $2",
        )
        .bind(crate::db_types::uuid(BOB).unwrap())
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(work_before, work_after);
    }
    let revision = viewer(&schema, &work).await["revision"].as_i64().unwrap();
    save(&schema, &work, "entry-three", 20, true, revision).await;
    let revision = revision + 1;
    assert_eq!(
        save(&schema, &work, "entry-three", 20, true, revision).await["revision"],
        revision
    );
}

#[tokio::test]
async fn moved_copy_returns_current_work_before_considering_revision_or_pages() {
    let (schema, pool, work) = fixture(true).await;
    let entry = fixture_entry_id(&pool, &work, "300").await;
    sqlx::query("INSERT INTO work_progress (user_id,work_id,through_entry_id,revision,updated_at) VALUES ($1,$2,$3,20,now())")
        .bind(crate::db_types::uuid(BOB).unwrap()).bind(&work).bind(&entry).execute(&pool).await.unwrap();
    let source: String = sqlx::query_scalar(
        "SELECT id FROM source_series WHERE work_id=$1 AND source_type='suwayomi'",
    )
    .bind(&work)
    .fetch_one(&pool)
    .await
    .unwrap();
    let split = catalog::split_source_series(&pool, &work, &[source], None)
        .await
        .unwrap();
    let current = viewer(&schema, &split.new_work_id).await["revision"]
        .as_i64()
        .unwrap();
    for result in [
        save(&schema, &work, "9202", 99, false, 20).await,
        tick(&schema, &work, "9202", true, 20).await,
    ] {
        assert_eq!(
            result,
            json!({"status":"MOVED","revision":current,"workId":split.new_work_id})
        );
    }
    assert!(bob_progress_row(&pool, "9202").await.is_none());
}

#[tokio::test]
async fn legacy_mutations_still_return_boolean_and_update_ready_pointer_marks_and_revision() {
    let (schema, pool, work) = fixture(true).await;
    let data = bob(
        &schema,
        r#"mutation { setProgress(chapterId:"9203",lastPageRead:20,read:true) }"#,
    )
    .await;
    assert_eq!(data["setProgress"], true);
    let entry = fixture_entry_id(&pool, &work, "300").await;
    assert_eq!(
        viewer(&schema, &work).await,
        json!({"revision":1,"throughEntryId":entry})
    );
    let data = bob(
        &schema,
        r#"mutation { setChapterRead(chapterId:"entry-one",read:false) }"#,
    )
    .await;
    assert_eq!(data["setChapterRead"], true);
    let one = fixture_entry_id(&pool, &work, "100").await;
    assert_eq!(mark(&pool, &one).await, Some(false));
    assert_eq!(viewer(&schema, &work).await["revision"], 2);
    assert!(bob_progress_row(&pool, "entry-zero").await.is_none());
}

#[tokio::test]
async fn not_ready_finish_keeps_legacy_catch_up_and_bundle_read_anywhere() {
    let (schema, pool, work) = fixture(false).await;
    let data = bob(
        &schema,
        r#"mutation { setProgress(chapterId:"entry-three",lastPageRead:20,read:true) }"#,
    )
    .await;
    assert_eq!(data["setProgress"], true);
    for id in ["entry-zero", "entry-one", "entry-two"] {
        assert!(
            bob_progress_row(&pool, id).await.unwrap().1,
            "legacy catch-up {id}"
        );
    }
    let data = lists(&schema, &work).await;
    assert_eq!(
        row(&data["seriesPage"]["aggregated"], "100")["readAnywhere"],
        true
    );
    assert_eq!(copy(&data["chapters"], "9201")["read"], true);
    assert_eq!(
        row(&data["seriesPage"]["aggregated"], "x:entry-extra")["readAnywhere"],
        false
    );
    let pointer = fixture_entry_id(&pool, &work, "300").await;
    assert_eq!(
        data["seriesPage"]["viewerProgress"],
        json!({"revision":1,"throughEntryId":pointer})
    );
    bob(
        &schema,
        r#"mutation { setChapterRead(chapterId:"entry-one",read:false) }"#,
    )
    .await;
    bob(
        &schema,
        r#"mutation { setProgress(chapterId:"entry-three",lastPageRead:20,read:true) }"#,
    )
    .await;
    assert!(
        !bob_progress_row(&pool, "entry-one").await.unwrap().1,
        "legacy untick still protects catch-up"
    );
}

#[tokio::test]
async fn viewer_progress_is_null_for_anonymous_and_follows_redirects() {
    let (schema, pool, work) = fixture(true).await;
    let data = query(&schema, &format!(r#"{{ viewerProgress(workId:"{work}") {{ revision }} seriesPage(workId:"{work}") {{ viewerProgress {{ revision }} }} }}"#), None).await;
    assert!(data["viewerProgress"].is_null());
    assert!(data["seriesPage"]["viewerProgress"].is_null());
    assert_eq!(
        viewer(&schema, &work).await,
        json!({"revision":0,"throughEntryId":null})
    );
    save(&schema, &work, "entry-two", 10, true, 0).await;
    sqlx::query(
        "INSERT INTO work_redirect (old_id,new_id,created_at) VALUES ('w_w3b_retired',$1,now())",
    )
    .bind(&work)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        viewer(&schema, "w_w3b_retired").await,
        viewer(&schema, &work).await
    );
    assert_eq!(
        save(&schema, "w_w3b_retired", "entry-three", 20, true, 1).await["status"],
        "APPLIED"
    );
}

#[tokio::test]
async fn ready_library_progress_counts_pointer_only_reads_and_timestamp() {
    let (schema, pool, work) = fixture(true).await;
    let entry = fixture_entry_id(&pool, &work, "300").await;
    sqlx::query("INSERT INTO user_library (user_id,series_id,created_at) VALUES ($1,$2,now())")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO work_progress (user_id,work_id,through_entry_id,revision,updated_at) VALUES ($1,$2,$3,7,'2026-02-01T12:34:56Z')")
        .bind(crate::db_types::uuid(BOB).unwrap()).bind(&work).bind(&entry).execute(&pool).await.unwrap();
    let data = bob(&schema, "{ libraryProgress { id read total lastReadAt } }").await;
    let progress = copy(&data["libraryProgress"], &work);
    assert_eq!(progress["read"], 4);
    assert_eq!(progress["total"], 5);
    assert_eq!(progress["lastReadAt"], "2026-02-01T12:34:56.000000+00:00");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM canonical_progress WHERE user_id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "all reads came from the pointer");
    let hidden = fixture_entry_id(&pool, &work, "100").await;
    sqlx::query("UPDATE work_chapter SET hidden=true WHERE id=$1")
        .bind(&hidden)
        .execute(&pool)
        .await
        .unwrap();
    tick(&schema, &work, "entry-two", false, 7).await;
    let data = bob(&schema, "{ libraryProgress { id read total lastReadAt } }").await;
    let progress = copy(&data["libraryProgress"], &work);
    assert_eq!(progress["read"], 2);
    assert_eq!(progress["total"], 4);
}

#[tokio::test]
async fn new_mutations_enforce_numeric_epoch_before_revision_checks() {
    let (schema, pool, work) = fixture(true).await;
    sqlx::query(
        "INSERT INTO engine_numeric_state (singleton,epoch,changed_at) VALUES (true,5,now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    for field in [
        "saveProgress(chapterId:\"9201\",lastPageRead:4,read:true,revision:0",
        "saveChapterRead(chapterId:\"9201\",read:true,revision:0",
    ] {
        for epoch in [String::new(), ",numericEpoch:4".into()] {
            let text =
                format!("mutation {{ {field},workId:\"{work}\"{epoch}) {{ status revision }} }}");
            let response = exec(&schema, &text, Some("bobtok"), "w3b-epoch").await;
            assert!(!response.errors.is_empty());
            assert_eq!(
                response.errors[0].extensions.as_ref().unwrap().get("code"),
                Some(&async_graphql::Value::String("STALE_NUMERIC_EPOCH".into()))
            );
        }
    }
    assert!(bob_progress_row(&pool, "9201").await.is_none());
    let result = bob(&schema, &format!(r#"mutation {{ saveProgress(chapterId:"9201",workId:"{work}",lastPageRead:4,read:true,revision:0,numericEpoch:5) {{ status revision }} }}"#)).await;
    assert_eq!(result["saveProgress"]["status"], "APPLIED");
    let result = bob(&schema, &format!(r#"mutation {{ saveChapterRead(chapterId:"9201",workId:"{work}",read:false,revision:1,numericEpoch:5) {{ status revision }} }}"#)).await;
    assert_eq!(result["saveChapterRead"]["revision"], 2);
}

#[tokio::test]
async fn chapter_identity_admin_edits_preserve_reads_and_require_admin() {
    let (schema, pool, work) = fixture(true).await;
    tick(&schema, &work, "entry-one", true, 0).await;
    let two = fixture_entry_id(&pool, &work, "200").await;
    let three = fixture_entry_id(&pool, &work, "300").await;
    let edits = [
        format!(r#"mutation {{ linkChapterCopy(chapterId:"entry-one",entryId:"{two}") }}"#),
        format!(r#"mutation {{ mergeWorkChapters(keepId:"{three}",dropId:"{two}") }}"#),
    ];
    for text in &edits {
        for token in [None, Some("bobtok")] {
            assert!(!exec(&schema, text, token, "w3b-admin")
                .await
                .errors
                .is_empty());
        }
    }
    for text in &edits {
        let result = query(&schema, text, Some("admintok")).await;
        assert_eq!(
            result.as_object().unwrap().values().next(),
            Some(&json!(true))
        );
    }
    let data = lists(&schema, &work).await;
    for id in ["entry-one", "entry-two", "entry-three"] {
        assert_eq!(copy(&data["canonicalChapters"], id)["read"], true, "{id}");
    }
    assert_eq!(
        copy(&data["canonicalChapters"], "entry-zero")["read"],
        false
    );
    assert_eq!(
        copy(&data["chapters"], "9201")["read"],
        true,
        "old entry also keeps its read"
    );
    assert_eq!(viewer(&schema, &work).await["revision"], 3);
    query(
        &schema,
        r#"mutation { linkChapterCopy(chapterId:"entry-one") }"#,
        Some("admintok"),
    )
    .await;
    let data = lists(&schema, &work).await;
    assert_eq!(copy(&data["canonicalChapters"], "entry-one")["read"], true);
}

#[tokio::test]
async fn new_mutations_validate_page_id_and_auth_and_unowned_copies_keep_revision_zero() {
    let (schema, pool) = setup_full(100).await;
    for text in [
        r#"mutation { saveProgress(chapterId:"x",lastPageRead:-1,read:false,revision:0) { status } }"#,
        r#"mutation { saveProgress(chapterId:"",lastPageRead:0,read:false,revision:0) { status } }"#,
        r#"mutation { saveChapterRead(chapterId:"",read:true,revision:0) { status } }"#,
    ] {
        assert!(!exec(&schema, text, Some("bobtok"), "w3b-validation")
            .await
            .errors
            .is_empty());
    }
    for text in [
        r#"mutation { saveProgress(chapterId:"unknown",lastPageRead:1,read:false,revision:0) { status } }"#,
        r#"mutation { saveChapterRead(chapterId:"unknown",read:false,revision:0) { status } }"#,
    ] {
        assert!(!exec(&schema, text, None, "w3b-validation")
            .await
            .errors
            .is_empty());
    }
    let result = save(&schema, "w_missing", "unmirrored-copy", 3, true, 99).await;
    assert_eq!(
        result,
        json!({"status":"APPLIED","revision":0,"workId":null})
    );
    assert!(bob_progress_row(&pool, "unmirrored-copy").await.unwrap().1);
}

#[tokio::test]
async fn ready_unlinked_copy_uses_own_rows_without_pointer_or_sibling_changes() {
    let (schema, pool, work) = fixture(true).await;
    sqlx::query("DELETE FROM work_chapter_link WHERE chapter_id IN (SELECT id FROM chapter WHERE external_id='entry-one')")
        .execute(&pool).await.unwrap();
    assert_eq!(
        save(&schema, &work, "entry-one", 9, true, 0).await["revision"],
        1
    );
    assert!(viewer(&schema, &work).await["throughEntryId"].is_null());
    let data = lists(&schema, &work).await;
    assert_eq!(copy(&data["canonicalChapters"], "entry-one")["read"], true);
    assert_eq!(copy(&data["chapters"], "9201")["read"], false);
    assert_eq!(
        tick(&schema, &work, "entry-one", false, 1).await["revision"],
        2
    );
    assert!(!bob_progress_row(&pool, "entry-one").await.unwrap().1);
    let marks: i64 = sqlx::query_scalar("SELECT count(*) FROM work_chapter_mark WHERE user_id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    let unticks: i64 = sqlx::query_scalar("SELECT count(*) FROM progress_untick WHERE user_id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((marks, unticks), (0, 0));
}

#[tokio::test]
async fn same_sort_pointer_advances_by_id_and_page_saves_repair_copy_owner() {
    let (schema, pool, work) = fixture(true).await;
    let one = fixture_entry_id(&pool, &work, "100").await;
    let two = fixture_entry_id(&pool, &work, "200").await;
    sqlx::query("UPDATE work_chapter SET sort_key=1 WHERE id=$1")
        .bind(&two)
        .execute(&pool)
        .await
        .unwrap();
    let mut entries = [(one, "entry-one"), (two, "entry-two")];
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    save(&schema, &work, entries[0].1, 2, true, 0).await;
    save(&schema, &work, entries[1].1, 2, true, 1).await;
    assert_eq!(viewer(&schema, &work).await["throughEntryId"], entries[1].0);
    assert_eq!(
        save(&schema, &work, entries[0].1, 2, true, 2).await["revision"],
        2
    );
    for (table, owner, id, correct) in [
        ("canonical_progress", "work_id", "entry-one", work.as_str()),
        ("suwayomi_progress", "series_id", "9201", "779"),
    ] {
        let revision = viewer(&schema, &work).await["revision"].as_i64().unwrap();
        save(&schema, &work, id, 5, false, revision).await;
        sqlx::query(&format!(
            "UPDATE {table} SET {owner}='' WHERE user_id=$1 AND chapter_id=$2"
        ))
        .bind(crate::db_types::uuid(BOB).unwrap())
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        let revision = viewer(&schema, &work).await["revision"].as_i64().unwrap();
        assert_eq!(
            save(&schema, &work, id, 5, false, revision).await["revision"],
            revision + 1
        );
        let repaired: String = sqlx::query_scalar(&format!(
            "SELECT {owner} FROM {table} WHERE user_id=$1 AND chapter_id=$2"
        ))
        .bind(crate::db_types::uuid(BOB).unwrap())
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(repaired, correct);
    }
}

#[tokio::test]
async fn not_ready_read_clears_a_kept_unread_mark() {
    let (schema, pool, work) = fixture(false).await;
    let entry = fixture_entry_id(&pool, &work, "300").await;
    // An engine reset kept a removed copy's untick as this mark.
    sqlx::query("INSERT INTO work_chapter_mark (user_id, work_chapter_id, read, created_at) VALUES ($1, $2, false, now())")
        .bind(crate::db_types::uuid(BOB).unwrap()).bind(&entry).execute(&pool).await.unwrap();
    bob(
        &schema,
        r#"mutation { setProgress(chapterId:"entry-three",lastPageRead:20,read:true) }"#,
    )
    .await;
    let marks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM work_chapter_mark WHERE user_id = $1 AND work_chapter_id = $2",
    )
    .bind(crate::db_types::uuid(BOB).unwrap())
    .bind(&entry)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(marks, 0);
}

#[tokio::test]
async fn an_untick_keeps_the_last_read_time_and_a_finish_moves_it() {
    let (schema, pool, work) = fixture(true).await;
    bob(
        &schema,
        r#"mutation { setProgress(chapterId:"9203",lastPageRead:20,read:true) }"#,
    )
    .await;
    sqlx::query("UPDATE work_progress SET updated_at = '2026-01-01T00:00:00Z' WHERE user_id = $1 AND work_id = $2")
        .bind(crate::db_types::uuid(BOB).unwrap()).bind(&work).execute(&pool).await.unwrap();
    let last_read = || async {
        sqlx::query_as::<_, (bool, i64)>(
            "SELECT updated_at = '2026-01-01T00:00:00Z'::timestamptz, revision \
               FROM work_progress WHERE user_id = $1 AND work_id = $2",
        )
        .bind(crate::db_types::uuid(BOB).unwrap())
        .bind(&work)
        .fetch_one(pool.read())
        .await
        .unwrap()
    };
    bob(
        &schema,
        r#"mutation { setChapterRead(chapterId:"entry-one",read:false) }"#,
    )
    .await;
    assert_eq!(last_read().await, (true, 2), "an untick is not a read");
    bob(
        &schema,
        r#"mutation { setProgress(chapterId:"entry-one",lastPageRead:20,read:true) }"#,
    )
    .await;
    assert_eq!(last_read().await, (false, 3), "a finish is a read");
}

async fn save_op(
    schema: &ApiSchema,
    work: &str,
    id: &str,
    page: i32,
    read: bool,
    revision: i64,
    op: &str,
) -> Value {
    bob(schema, &format!(
        r#"mutation {{ saveProgress(chapterId:"{id}",workId:"{work}",lastPageRead:{page},read:{read},revision:{revision},opId:"{op}") {{ status revision workId }} }}"#,
    )).await["saveProgress"].clone()
}

async fn tick_op(
    schema: &ApiSchema,
    work: &str,
    id: &str,
    read: bool,
    revision: i64,
    op: &str,
) -> Value {
    bob(schema, &format!(
        r#"mutation {{ saveChapterRead(chapterId:"{id}",workId:"{work}",read:{read},revision:{revision},opId:"{op}") {{ status revision workId }} }}"#,
    )).await["saveChapterRead"].clone()
}

async fn last_op_id(pool: &Database, work: &str) -> Option<String> {
    sqlx::query_scalar("SELECT last_op_id FROM work_progress WHERE user_id = $1 AND work_id = $2")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .bind(work)
        .fetch_one(pool.read())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_resent_save_with_its_op_id_is_applied_once_and_other_stale_saves_stay_refused() {
    let (schema, pool, work) = fixture(true).await;
    let applied = |revision: i64| json!({"status":"APPLIED","revision":revision,"workId":work});
    // A finish whose answer is lost is resent with the same base revision and id.
    assert_eq!(
        save_op(&schema, &work, "entry-one", 20, true, 0, "op-a").await,
        applied(1)
    );
    assert_eq!(last_op_id(&pool, &work).await.as_deref(), Some("op-a"));
    let row = bob_progress_row(&pool, "entry-one").await;
    let progress = viewer(&schema, &work).await;
    assert_eq!(
        save_op(&schema, &work, "entry-one", 20, true, 0, "op-a").await,
        applied(1)
    );
    assert_eq!(
        viewer(&schema, &work).await,
        progress,
        "no second revision bump"
    );
    assert_eq!(bob_progress_row(&pool, "entry-one").await, row);
    // The same for a tick.
    assert_eq!(
        tick_op(&schema, &work, "entry-two", true, 1, "op-b").await,
        applied(2)
    );
    assert_eq!(
        tick_op(&schema, &work, "entry-two", true, 1, "op-b").await,
        applied(2)
    );
    // Another id with a stale base is still refused, and so is a stale save with none.
    assert_eq!(
        save_op(&schema, &work, "entry-three", 20, true, 0, "op-c").await["status"],
        "REFUSED"
    );
    assert_eq!(
        save(&schema, &work, "entry-three", 20, true, 0).await["status"],
        "REFUSED"
    );
    assert!(bob_progress_row(&pool, "entry-three").await.is_none());
    // A save with no id behaves as before and stores none, so op-b is no longer the last
    // change: its resend is judged by its revision again.
    assert_eq!(
        save(&schema, &work, "entry-three", 20, true, 2).await,
        applied(3)
    );
    assert_eq!(last_op_id(&pool, &work).await, None);
    assert_eq!(
        tick_op(&schema, &work, "entry-two", true, 1, "op-b").await["status"],
        "REFUSED"
    );
    // An old mutation's change clears the id too.
    assert_eq!(
        tick_op(&schema, &work, "entry-one", false, 3, "op-d").await,
        applied(4)
    );
    let data = bob(
        &schema,
        r#"mutation { setChapterRead(chapterId:"entry-one",read:true) }"#,
    )
    .await;
    assert_eq!(data["setChapterRead"], true);
    assert_eq!(viewer(&schema, &work).await["revision"], 5);
    assert_eq!(last_op_id(&pool, &work).await, None);
    assert_eq!(
        tick_op(&schema, &work, "entry-one", false, 3, "op-d").await["status"],
        "REFUSED"
    );
    // The id is bounded.
    let long = "x".repeat(65);
    for op in ["", long.as_str()] {
        for text in [
            format!(
                r#"mutation {{ saveProgress(chapterId:"entry-one",workId:"{work}",lastPageRead:1,read:false,revision:5,opId:"{op}") {{ status }} }}"#
            ),
            format!(
                r#"mutation {{ saveChapterRead(chapterId:"entry-one",workId:"{work}",read:true,revision:5,opId:"{op}") {{ status }} }}"#
            ),
        ] {
            assert!(!exec(&schema, &text, Some("bobtok"), "w3b-op-id")
                .await
                .errors
                .is_empty());
        }
    }
    assert_eq!(
        save_op(&schema, &work, "entry-one", 1, false, 5, &"y".repeat(64)).await["status"],
        "APPLIED"
    );
}
