//! Regression tests use two independent writer pools on one file-backed WAL DB.
use super::*;
use std::sync::Arc;
use tokio::sync::Barrier;

async fn database_pair() -> (Database, Database) {
    // Database's test memory URL allocates a temporary file, held by `first`.
    let first = Database::connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!("./migrations")
        .run(first.write())
        .await
        .unwrap();
    let options = (*first.write().connect_options()).clone();
    assert_ne!(options.get_filename(), std::path::Path::new(":memory:"));
    let second = Database::connect_with(options, 2).await.unwrap();
    assert_eq!(
        first.write().connect_options().get_filename(),
        second.write().connect_options().get_filename()
    );
    (first, second)
}

async fn work(pool: &Database, title: &str) -> String {
    create_work(
        pool,
        &WorkInput {
            primary_title: Some(title.to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn disjoint_concurrent_splits_keep_one_original_source() {
    let (first, second) = database_pair().await;
    let original = work(&first, "Split original").await;
    let left = upsert_source_series(&first, &original, "suwayomi", "left", "101", None, false)
        .await
        .unwrap();
    let right = upsert_source_series(&first, &original, "suwayomi", "right", "102", None, false)
        .await
        .unwrap();

    // Occupy the writer while both requests begin. The former implementation read
    // the same two-source set before it queued for this lock, so both splits passed
    // the last-source check. The corrected implementation reads only after BEGIN.
    let writer = second.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let ready = Arc::new(Barrier::new(3));
    let a = {
        let db = first.clone();
        let id = original.clone();
        let ready = ready.clone();
        tokio::spawn(async move {
            ready.wait().await;
            split_source_series(&db, &id, &[left], None).await
        })
    };
    let b = {
        let db = second.clone();
        let id = original.clone();
        let ready = ready.clone();
        tokio::spawn(async move {
            ready.wait().await;
            split_source_series(&db, &id, &[right], None).await
        })
    };
    ready.wait().await;
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    writer.commit().await.unwrap();
    let (a, b) = tokio::join!(a, b);
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let rejected = a.err().or_else(|| b.err()).unwrap();
    assert!(rejected.to_string().contains("at least one source"));
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_series WHERE work_id = ?")
        .bind(&original)
        .fetch_one(&first)
        .await
        .unwrap();
    assert_eq!(remaining, 1);
    let works: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
        .fetch_one(&second)
        .await
        .unwrap();
    assert_eq!(works, 2, "one successful split creates exactly one work");
    second.close().await;
    first.close().await;
}

#[tokio::test]
async fn pending_review_refuses_source_moved_since_owner_snapshot() {
    let (first, second) = database_pair().await;
    let original = work(&first, "Review source").await;
    let target = work(&first, "Review target").await;
    let other = work(&first, "Concurrent split destination").await;
    let source = upsert_source_series(&first, &original, "suwayomi", "source", "201", None, false)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO merge_candidate \
             (id, source_series_id, candidate_work_id, score, method, created_at) \
         VALUES ('stale-owner-review', ?, ?, 1.0, 'title_exact', '2026-01-01')",
    )
    .bind(&source)
    .bind(&target)
    .execute(&first)
    .await
    .unwrap();
    let expected_source = load_match_data(&first, &original)
        .await
        .unwrap()
        .unwrap()
        .identity(None);
    let expected_target = load_match_data(&first, &target)
        .await
        .unwrap()
        .unwrap()
        .identity(None);
    // A second database handle moves only the source mapping. Metadata identities
    // still match, so only the source-owner clause can prevent merging the old work.
    sqlx::query("UPDATE source_series SET work_id = ? WHERE id = ?")
        .bind(&other)
        .bind(&source)
        .execute(&second)
        .await
        .unwrap();
    let error = merge_pending_candidate_checked(
        &first,
        None,
        &original,
        &target,
        (&expected_source, &expected_target),
        "stale-owner-review",
    )
    .await
    .expect_err("a changed source owner must invalidate the review acceptance");
    assert!(error.to_string().starts_with("merge precondition failed"));
    let status: String =
        sqlx::query_scalar("SELECT status FROM merge_candidate WHERE id = 'stale-owner-review'")
            .fetch_one(&second)
            .await
            .unwrap();
    assert_eq!(status, "pending");
    let current: String = sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = ?")
        .bind(&source)
        .fetch_one(&second)
        .await
        .unwrap();
    assert_eq!(current, other);
    let works: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work")
        .fetch_one(&second)
        .await
        .unwrap();
    assert_eq!(works, 3);
    second.close().await;
    first.close().await;
}

#[tokio::test]
async fn source_nsfw_pin_rolls_back_when_work_override_fails() {
    let (first, second) = database_pair().await;
    let id = work(&first, "Source pin rollback").await;
    upsert_source_series(
        &first,
        &id,
        "suwayomi",
        "rollback-source",
        "301",
        None,
        false,
    )
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_nsfw_override BEFORE UPDATE OF is_nsfw_override ON work \
         BEGIN SELECT RAISE(ABORT, 'injected work override failure'); END",
    )
    .execute(&second)
    .await
    .unwrap();
    let error = set_source_nsfw_pin(&first, "rollback-source", true)
        .await
        .expect_err("the failed work write must fail the complete operation");
    assert!(error.to_string().contains("injected work override failure"));
    assert!(!source_nsfw_pinned(&second, "rollback-source")
        .await
        .unwrap());
    let flag: Option<i64> = sqlx::query_scalar("SELECT is_nsfw_override FROM work WHERE id = ?")
        .bind(&id)
        .fetch_one(&second)
        .await
        .unwrap();
    assert_eq!(flag, None);
    second.close().await;
    first.close().await;
}

#[tokio::test]
async fn concurrent_source_nsfw_choices_keep_pin_and_work_flags_together() {
    let (first, second) = database_pair().await;
    for (key, title) in [("401", "Pin first"), ("402", "Pin second")] {
        let id = work(&first, title).await;
        upsert_source_series(&first, &id, "suwayomi", "race-source", key, None, false)
            .await
            .unwrap();
    }
    for _ in 0..8 {
        let (pin, unpin) = tokio::join!(
            set_source_nsfw_pin(&first, "race-source", true),
            set_source_nsfw_pin(&second, "race-source", false),
        );
        pin.unwrap();
        unpin.unwrap();
        let pinned = source_nsfw_pinned(&second, "race-source").await.unwrap();
        let flags: Vec<Option<i64>> = sqlx::query_scalar(
            "SELECT is_nsfw_override FROM work \
             WHERE id IN (SELECT work_id FROM source_series WHERE source_id = 'race-source')",
        )
        .fetch_all(&first)
        .await
        .unwrap();
        assert_eq!(flags, vec![pinned.then_some(1_i64); 2]);
    }
    // Also prove the public catalog operation updates existing works by itself.
    set_source_nsfw_pin(&first, "race-source", true)
        .await
        .unwrap();
    let adult: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work WHERE is_nsfw_override = 1")
        .fetch_one(&second)
        .await
        .unwrap();
    assert_eq!(adult, 2);
    second.close().await;
    first.close().await;
}
