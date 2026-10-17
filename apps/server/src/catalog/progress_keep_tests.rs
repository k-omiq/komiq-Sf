//! W3b identity changes preserve each reader's effective entry state.
use super::work_chapter::tests::{
    all_mangadex_source, chapter, entry, linked_entry, make_ready, md_work, source,
};
use super::*;
use crate::catalog::phase4_tests::{pool, wait_for_lock};
use crate::progress::{self, WorkReadState};
use std::collections::HashMap;
use std::time::Duration;

async fn reader(pool: &Database) -> String {
    let id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO users(id, username, email, password_hash, created_at) VALUES ($1, $2, $3, 'hash', now())")
        .bind(id).bind(id.to_string()).bind(format!("{id}@example.test"))
        .execute(pool.write()).await.unwrap();
    id.to_string()
}

async fn pointer(pool: &Database, user: &str, work: &str, entry: &str) {
    sqlx::query("INSERT INTO work_progress(user_id, work_id, through_entry_id, revision, updated_at) VALUES ($1, $2, $3, 7, now())")
        .bind(crate::db_types::uuid(user).unwrap()).bind(work).bind(entry)
        .execute(pool.write()).await.unwrap();
}

async fn mark(pool: &Database, user: &str, entry: &str, read: bool) {
    sqlx::query("INSERT INTO work_chapter_mark(user_id, work_chapter_id, read, created_at) VALUES ($1, $2, $3, now())")
        .bind(crate::db_types::uuid(user).unwrap()).bind(entry).bind(read)
        .execute(pool.write()).await.unwrap();
}

async fn canonical(pool: &Database, user: &str, work: &str, copy: &str) {
    sqlx::query("INSERT INTO canonical_progress(user_id, work_id, chapter_id, read, updated_at) VALUES ($1, $2, $3, true, now())")
        .bind(crate::db_types::uuid(user).unwrap()).bind(work).bind(copy)
        .execute(pool.write()).await.unwrap();
}

async fn numeric(pool: &Database, user: &str, series: &str, copy: &str) {
    sqlx::query("INSERT INTO suwayomi_progress(user_id, series_id, chapter_id, read, updated_at) VALUES ($1, $2, $3, true, now())")
        .bind(crate::db_types::uuid(user).unwrap()).bind(series).bind(copy)
        .execute(pool.write()).await.unwrap();
}

async fn untick(pool: &Database, user: &str, copy: &str) {
    sqlx::query(
        "INSERT INTO progress_untick(user_id, chapter_id, created_at) VALUES ($1, $2, now())",
    )
    .bind(crate::db_types::uuid(user).unwrap())
    .bind(copy)
    .execute(pool.write())
    .await
    .unwrap();
}

async fn reads(pool: &Database, user: &str, work: &str) -> HashMap<String, bool> {
    match progress::work_read_state(pool, Some(user), work).await {
        WorkReadState::Ready { entries, .. } => entries,
        _ => panic!("fixture should be ready: {work}"),
    }
}

#[tokio::test]
async fn merge_keeps_readers_including_numeric_only_progress() {
    let pool = pool().await;
    let loser = md_work(&pool, "Progress loser").await;
    let survivor = md_work(&pool, "Progress survivor").await;
    replace_source_chapters(
        &pool,
        &loser.source,
        &[
            chapter("loser-one", Some("1"), "One"),
            chapter("loser-two", Some("2"), "Two"),
            chapter("loser-three", Some("3"), "Three"),
        ],
    )
    .await
    .unwrap();
    replace_source_chapters(
        &pool,
        &survivor.source,
        &[
            chapter("survivor-one", Some("1"), "One"),
            chapter("survivor-two", Some("2"), "Two"),
        ],
    )
    .await
    .unwrap();
    let sw = source(&pool, &loser.work, "71").await;
    upsert_chapter(&pool, &sw, &chapter("73", Some("3"), "Three"))
        .await
        .unwrap();
    make_ready(&pool, &loser.work).await;
    make_ready(&pool, &survivor.work).await;
    let p_reader = reader(&pool).await;
    let n_reader = reader(&pool).await;
    pointer(
        &pool,
        &p_reader,
        &loser.work,
        &entry(&pool, &loser.work, "200").await.id,
    )
    .await;
    mark(
        &pool,
        &p_reader,
        &entry(&pool, &survivor.work, "100").await.id,
        false,
    )
    .await;
    numeric(&pool, &n_reader, "71", "73").await;
    let discovery = merge_discovery(
        &mut pool.acquire().await.unwrap(),
        &[&loser.work, &survivor.work],
        &[],
    )
    .await
    .unwrap();
    assert!(discovery.users.contains(&n_reader));
    let mut before = Vec::new();
    for user in [&p_reader, &n_reader] {
        before.push((
            reads(&pool, user, &loser.work).await,
            reads(&pool, user, &survivor.work).await,
        ));
    }
    let outcome = merge_works(&pool, &loser.work, &survivor.work)
        .await
        .unwrap();
    for (user, (losing, surviving)) in [&p_reader, &n_reader].into_iter().zip(before) {
        let mut expected = surviving;
        for movement in &outcome.entry_map {
            *expected.entry(movement.to.clone()).or_insert(false) |= losing[&movement.from];
        }
        assert_eq!(reads(&pool, user, &survivor.work).await, expected);
        let orphan: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM work_progress WHERE user_id=$1 AND work_id=$2",
        )
        .bind(crate::db_types::uuid(user).unwrap())
        .bind(&loser.work)
        .fetch_one(pool.read())
        .await
        .unwrap();
        assert_eq!(orphan, 0);
    }
}

#[tokio::test]
async fn dialog_split_moves_pointer_and_counts_pointer_and_mark_only_readers() {
    let pool = pool().await;
    let md = md_work(&pool, "Split progress").await;
    replace_source_chapters(
        &pool,
        &md.source,
        &[
            chapter("moving-one", Some("1"), "One"),
            chapter("moving-two", Some("2"), "Two"),
            chapter("moving-three", Some("3"), "Three"),
        ],
    )
    .await
    .unwrap();
    let sw = source(&pool, &md.work, "81").await;
    replace_source_chapters(
        &pool,
        &sw,
        &[
            chapter("82", Some("1"), "One"),
            chapter("84", Some("3"), "Three"),
        ],
    )
    .await
    .unwrap();
    make_ready(&pool, &md.work).await;
    let p_reader = reader(&pool).await;
    let c_reader = reader(&pool).await;
    let m_reader = reader(&pool).await;
    let old_p = entry(&pool, &md.work, "200").await.id;
    pointer(&pool, &p_reader, &md.work, &old_p).await;
    canonical(&pool, &c_reader, &md.work, "moving-three").await;
    mark(
        &pool,
        &m_reader,
        &entry(&pool, &md.work, "300").await.id,
        true,
    )
    .await;
    let users = [&p_reader, &c_reader, &m_reader];
    let mut before = Vec::new();
    for user in users {
        before.push(reads(&pool, user, &md.work).await);
    }
    let split = split_source_series(&pool, &md.work, &[md.source.clone()], None)
        .await
        .unwrap();
    assert_eq!(split.progress_readers_moved, 3);
    make_ready(&pool, &split.new_work_id).await;
    for (user, old) in users.into_iter().zip(before) {
        let from = reads(&pool, user, &md.work).await;
        let to = reads(&pool, user, &split.new_work_id).await;
        for (id, value) in &from {
            assert_eq!(*value, old[id]);
        }
        for movement in &split.entry_map {
            assert_eq!(to[&movement.to], old[&movement.from]);
        }
    }
    let state = progress::work_read_state(&pool, Some(&p_reader), &split.new_work_id).await;
    assert_eq!(state.viewer().unwrap().1, Some(old_p));
    let owner: String = sqlx::query_scalar(
        "SELECT work_id FROM canonical_progress WHERE user_id=$1 AND chapter_id='moving-three'",
    )
    .bind(crate::db_types::uuid(&c_reader).unwrap())
    .fetch_one(pool.read())
    .await
    .unwrap();
    assert_eq!(owner, split.new_work_id);
}

#[tokio::test]
async fn phase_f_split_moves_canonical_rows_and_locks_readers_before_works() {
    let pool = pool().await;
    let wrong = md_work(&pool, "Wrong owner").await;
    let right = md_work(&pool, "Right owner").await;
    upsert_chapter(
        &pool,
        &right.source,
        &chapter("right-one", Some("1"), "One"),
    )
    .await
    .unwrap();
    let moving = all_mangadex_source(&pool, &wrong.work, "91", &right.uuid).await;
    upsert_chapter(&pool, &moving, &chapter("92", Some("1"), "One"))
        .await
        .unwrap();
    make_ready(&pool, &wrong.work).await;
    make_ready(&pool, &right.work).await;
    let user = reader(&pool).await;
    let old_entry = linked_entry(&pool, &moving, "92").await;
    pointer(&pool, &user, &wrong.work, &old_entry).await;
    canonical(&pool, &user, &wrong.work, "92").await;
    let before = reads(&pool, &user, &wrong.work).await;
    let mut blocker = pool.begin().await.unwrap();
    crate::locks::lock_users(&mut blocker, &[&user])
        .await
        .unwrap();
    let task_pool = pool.foreground();
    let split = tokio::spawn(async move {
        crate::phase_f::split_mismatched(&task_pool, crate::phase_f::Apply::Yes).await
    });
    let waited = wait_for_lock(&pool).await;
    // A worker blocked on the first user must not already hold its work lock.
    sqlx::query("SET LOCAL lock_timeout = '1s'")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let work_locks = crate::locks::lock_works(&mut blocker, &[&wrong.work, &right.work]).await;
    blocker.rollback().await.unwrap();
    assert!(waited);
    assert!(
        work_locks.is_ok(),
        "phase F acquired work locks before users"
    );
    let report = tokio::time::timeout(Duration::from_secs(15), split)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(report.acted, 1);
    let owner: String = sqlx::query_scalar(
        "SELECT work_id FROM canonical_progress WHERE user_id=$1 AND chapter_id='92'",
    )
    .bind(crate::db_types::uuid(&user).unwrap())
    .fetch_one(pool.read())
    .await
    .unwrap();
    assert_eq!(owner, right.work);
    let next = linked_entry(&pool, &moving, "92").await;
    assert_eq!(
        reads(&pool, &user, &right.work).await[&next],
        before[&old_entry]
    );
}

#[tokio::test]
async fn vanished_copy_and_redundant_source_preserve_explicit_reads() {
    for redundant in [false, true] {
        let pool = pool().await;
        let md = md_work(&pool, "Keep vanished read").await;
        upsert_chapter(&pool, &md.source, &chapter("mirror-one", Some("1"), "One"))
            .await
            .unwrap();
        let sw = if redundant {
            all_mangadex_source(&pool, &md.work, "101", &md.uuid).await
        } else {
            source(&pool, &md.work, "101").await
        };
        upsert_chapter(&pool, &sw, &chapter("102", Some("1"), "One"))
            .await
            .unwrap();
        make_ready(&pool, &md.work).await;
        let user = reader(&pool).await;
        numeric(&pool, &user, "101", "102").await;
        let before = reads(&pool, &user, &md.work).await;
        if redundant {
            let report = crate::phase_f::delete_redundant(
                &pool,
                crate::phase_f::Apply::Yes,
                Some(crate::phase_f::DELETE_CONFIRM_TOKEN),
                false,
            )
            .await
            .unwrap();
            assert_eq!(report.deleted, 1);
        } else {
            replace_source_chapters(&pool, &sw, &[]).await.unwrap();
        }
        assert_eq!(reads(&pool, &user, &md.work).await, before);
        let kept: bool = sqlx::query_scalar(
            "SELECT read FROM work_chapter_mark WHERE user_id=$1 AND work_chapter_id=$2",
        )
        .bind(crate::db_types::uuid(&user).unwrap())
        .bind(entry(&pool, &md.work, "100").await.id)
        .fetch_one(pool.read())
        .await
        .unwrap();
        assert!(kept);
    }
}

#[tokio::test]
async fn foreign_language_purge_keeps_numeric_read_on_a_surviving_work() {
    let pool = pool().await;
    let md = md_work(&pool, "Purge progress").await;
    upsert_chapter(&pool, &md.source, &chapter("mirror", Some("1"), "One"))
        .await
        .unwrap();
    let sw = source(&pool, &md.work, "111").await;
    upsert_chapter(&pool, &sw, &chapter("112", Some("1"), "One"))
        .await
        .unwrap();
    sqlx::query("INSERT INTO suwayomi_series(id, title, status, source_id, lang, updated_at) VALUES (111, 'Foreign', 'ONGOING', 'source', 'ja', now())")
        .execute(pool.write()).await.unwrap();
    make_ready(&pool, &md.work).await;
    let user = reader(&pool).await;
    numeric(&pool, &user, "111", "112").await;
    let before = reads(&pool, &user, &md.work).await;
    purge_foreign_language_batch(&pool).await.unwrap();
    assert_eq!(reads(&pool, &user, &md.work).await, before);
}

#[tokio::test]
async fn drain_converts_legacy_unticks_and_restarts_for_new_readers() {
    use crate::locks::pause_points::PausePoints;
    let pool = pool().await;
    let md = md_work(&pool, "Drain unticks").await;
    upsert_chapter(&pool, &md.source, &chapter("drain-one", Some("1"), "One"))
        .await
        .unwrap();
    let sw = source(&pool, &md.work, "121").await;
    upsert_chapter(&pool, &sw, &chapter("122", Some("1"), "One"))
        .await
        .unwrap();
    let first = reader(&pool).await;
    let added = reader(&pool).await;
    let one = entry(&pool, &md.work, "100").await.id;
    mark(&pool, &first, &one, true).await;
    untick(&pool, &first, "drain-one").await;
    let points = PausePoints::default();
    let discovered = points.arm("work-chapter-drain-users");
    let task_pool = pool.foreground();
    let work = md.work.clone();
    let draining = tokio::spawn(guards::TEST_PAUSE.scope(points, async move {
        work_chapter::drain_work_attempt(&task_pool, &work).await
    }));
    tokio::time::timeout(Duration::from_secs(5), discovered.reached())
        .await
        .unwrap();
    untick(&pool, &added, "122").await;
    discovered.release();
    let error = tokio::time::timeout(Duration::from_secs(10), draining)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error
        .downcast_ref::<crate::locks::RestartTransaction>()
        .is_some());
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM progress_untick")
        .fetch_one(pool.read())
        .await
        .unwrap();
    assert_eq!(pending, 2);
    assert!(
        !work_chapter::work_entries(&pool, &md.work)
            .await
            .unwrap()
            .ready
    );
    make_ready(&pool, &md.work).await;
    for user in [&first, &added] {
        assert!(!reads(&pool, user, &md.work).await[&one]);
        let read: bool = sqlx::query_scalar(
            "SELECT read FROM work_chapter_mark WHERE user_id=$1 AND work_chapter_id=$2",
        )
        .bind(crate::db_types::uuid(user).unwrap())
        .bind(&one)
        .fetch_one(pool.read())
        .await
        .unwrap();
        assert!(!read);
    }
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM progress_untick")
        .fetch_one(pool.read())
        .await
        .unwrap();
    assert_eq!(pending, 0);
}

#[tokio::test]
async fn relink_copy_creates_an_independent_entry_and_rejects_other_works() {
    let pool = pool().await;
    let md = md_work(&pool, "Relink").await;
    replace_source_chapters(
        &pool,
        &md.source,
        &[
            chapter("relink-one", Some("1"), "One"),
            chapter("relink-two", Some("2"), "Two"),
            chapter("relink-special", None, "Appendix"),
        ],
    )
    .await
    .unwrap();
    let other = md_work(&pool, "Other work").await;
    upsert_chapter(
        &pool,
        &other.source,
        &chapter("other-one", Some("1"), "One"),
    )
    .await
    .unwrap();
    let foreign = entry(&pool, &other.work, "100").await.id;
    let old = entry(&pool, &md.work, "100").await.id;
    let two = entry(&pool, &md.work, "200").await.id;
    let copy: String = sqlx::query_scalar(
        "SELECT id FROM chapter WHERE source_series_id=$1 AND external_id='relink-one'",
    )
    .bind(&md.source)
    .fetch_one(pool.read())
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    crate::locks::lock_works(&mut tx, &[&md.work, &other.work])
        .await
        .unwrap();
    assert!(
        work_chapter::relink_copy_on(&mut tx, &md.work, &copy, Some(&foreign))
            .await
            .is_err()
    );
    assert_eq!(
        work_chapter::relink_copy_on(&mut tx, &md.work, &copy, Some(&two))
            .await
            .unwrap(),
        (old.clone(), two.clone())
    );
    let (from, new) = work_chapter::relink_copy_on(&mut tx, &md.work, &copy, None)
        .await
        .unwrap();
    assert_eq!(from, two);
    assert_ne!(new, old);
    let row: (Option<String>, String, Option<String>, String, bool, String) = sqlx::query_as(
        "SELECT wc.match_key, wc.sort_key::text, wc.number, wc.label, wc.special, l.linked_by \
         FROM work_chapter wc JOIN work_chapter_link l ON l.work_chapter_id=wc.id WHERE wc.id=$1",
    )
    .bind(&new)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            None,
            "1".into(),
            Some("1".into()),
            "1".into(),
            false,
            "admin".into()
        )
    );
    let special: String = sqlx::query_scalar(
        "SELECT id FROM chapter WHERE source_series_id=$1 AND external_id='relink-special'",
    )
    .bind(&md.source)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let (_, special_entry) = work_chapter::relink_copy_on(&mut tx, &md.work, &special, None)
        .await
        .unwrap();
    let plain: bool = sqlx::query_scalar(
        "SELECT NOT special AND match_key IS NULL FROM work_chapter WHERE id=$1",
    )
    .bind(special_entry)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(plain);
    tx.commit().await.unwrap();
    assert_eq!(linked_entry(&pool, &md.source, "relink-one").await, new);
}

#[tokio::test]
async fn merge_entries_within_moves_links_and_fills_only_missing_overrides() {
    for keep_title in [None, Some("Keep title")] {
        let pool = pool().await;
        let md = md_work(&pool, "Merge entries").await;
        replace_source_chapters(
            &pool,
            &md.source,
            &[
                chapter("keep", Some("1"), "One"),
                chapter("drop", Some("2"), "Two"),
            ],
        )
        .await
        .unwrap();
        let keep = entry(&pool, &md.work, "100").await.id;
        let drop = entry(&pool, &md.work, "200").await.id;
        let mut tx = pool.begin().await.unwrap();
        crate::locks::lock_works(&mut tx, &[&md.work])
            .await
            .unwrap();
        work_chapter::edit_entry_on(
            &mut tx,
            &md.work,
            &keep,
            &work_chapter::EntryEdit {
                title_override: Some(keep_title.map(str::to_owned)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        work_chapter::edit_entry_on(
            &mut tx,
            &md.work,
            &drop,
            &work_chapter::EntryEdit {
                hidden: Some(true),
                title_override: Some(Some("Drop title".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let revision: i64 =
            sqlx::query_scalar("SELECT list_rev FROM work_chapter_state WHERE work_id=$1")
                .bind(&md.work)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert!(
            work_chapter::merge_entries_within_on(&mut tx, &md.work, &keep, &keep)
                .await
                .is_err()
        );
        let moves = work_chapter::merge_entries_within_on(&mut tx, &md.work, &keep, &drop)
            .await
            .unwrap();
        assert_eq!(
            moves,
            [work_chapter::EntryMove {
                from: drop.clone(),
                to: keep.clone()
            }]
        );
        let next: i64 =
            sqlx::query_scalar("SELECT list_rev FROM work_chapter_state WHERE work_id=$1")
                .bind(&md.work)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(next, revision + 1);
        tx.commit().await.unwrap();
        let kept = entry(&pool, &md.work, "100").await;
        // The kept entry's overrides win as a whole; one with none takes the dropped one's.
        assert_eq!(kept.hidden, keep_title.is_none());
        assert_eq!(
            kept.title_override.as_deref(),
            Some(keep_title.unwrap_or("Drop title"))
        );
        assert_eq!(kept.copies.len(), 2);
        assert_eq!(linked_entry(&pool, &md.source, "drop").await, keep);
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM work_chapter WHERE id=$1)")
                .bind(drop)
                .fetch_one(pool.read())
                .await
                .unwrap();
        assert!(!exists);
    }
}

#[tokio::test]
async fn engine_reset_keeps_progress_and_serializes_with_canonical_untick() {
    use crate::engine_generation::{tests::MockEngine, Fence, Replace};
    use crate::graphql::{AppState, KeyedLocks, RateLimiter, RequestUserCache, ScanHealth};
    use crate::locks::pause_points::PausePoints;
    use crate::suwayomi::SuwayomiClient;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Semaphore;

    for concurrent_untick in [false, true] {
        let pool = pool().await;
        // No cover fixtures are needed.
        let cover_dir = crate::cover_store::test_support::TestDir::new();
        let covers = crate::cover_store::test_support::store(&cover_dir);
        let engine = MockEngine::spawn().await;
        let mut client = SuwayomiClient::new(engine.url.clone(), None, None);
        let fence = Fence::attach(&pool, &covers, &mut client).await.unwrap();
        let md = md_work(&pool, "Reset progress").await;
        replace_source_chapters(
            &pool,
            &md.source,
            &[
                chapter("reset-one", Some("1"), "One"),
                chapter("reset-two", Some("2"), "Two"),
                chapter("reset-three", Some("3"), "Three"),
            ],
        )
        .await
        .unwrap();
        let sw = source(&pool, &md.work, "7").await;
        upsert_chapter(&pool, &sw, &chapter("8", Some("3"), "Three"))
            .await
            .unwrap();
        make_ready(&pool, &md.work).await;
        let user = reader(&pool).await;
        let one = entry(&pool, &md.work, "100").await.id;
        let two = entry(&pool, &md.work, "200").await.id;
        let three = entry(&pool, &md.work, "300").await.id;
        pointer(&pool, &user, &md.work, &one).await;
        mark(&pool, &user, &two, false).await;
        canonical(&pool, &user, &md.work, "reset-one").await;
        numeric(&pool, &user, "7", "8").await;
        untick(&pool, &user, "8").await;
        let before = reads(&pool, &user, &md.work).await;
        let progress_before: serde_json::Value = sqlx::query_scalar(
            "SELECT jsonb_build_object('row', to_jsonb(p), 'version', p.xmin::text) FROM work_progress p WHERE user_id=$1 AND work_id=$2",
        )
        .bind(crate::db_types::uuid(&user).unwrap())
        .bind(&md.work)
        .fetch_one(pool.read())
        .await
        .unwrap();
        let mark_before: serde_json::Value = sqlx::query_scalar(
            "SELECT jsonb_build_object('row', to_jsonb(m), 'version', m.xmin::text) FROM work_chapter_mark m WHERE user_id=$1 AND work_chapter_id=$2",
        )
        .bind(crate::db_types::uuid(&user).unwrap())
        .bind(&two)
        .fetch_one(pool.read())
        .await
        .unwrap();

        let points = PausePoints::default();
        let locked = points.arm("engine-reset-users");
        let reset_fence = fence.clone();
        let resetting = tokio::spawn(guards::TEST_PAUSE.scope(points, async move {
            reset_fence.start_new_generation(Replace::Force).await
        }));
        tokio::time::timeout(Duration::from_secs(5), locked.reached())
            .await
            .unwrap();
        let unticking = if concurrent_untick {
            let state = Arc::new(AppState {
                pool: pool.foreground(),
                cover_pool: covers.clone(),
                suwayomi: client.clone(),
                mangadex: Arc::new(crate::mangadex::MangaDexClient::new("test-ua", 5.0, 40.0)),
                admin_users: vec![],
                scan_health: Mutex::new(ScanHealth::default()),
                auth_limiter: RateLimiter::new(100, 60),
                comment_limiter: crate::graphql::comment_limiter(),
                rating_limiter: crate::graphql::rating_limiter(),
                password_work: Arc::new(Semaphore::new(4)),
                federated_limiter: RateLimiter::new(100, 60),
                session_ttl_secs: 86400,
                series_inflight: KeyedLocks::default(),
                chapters_inflight: KeyedLocks::default(),
                source_budgets: Default::default(),
                catalogue_cover_phash: false,
                ext_icons_dir: Default::default(),
                media_manifests: None,
                policy: Default::default(),
                setup_code: Default::default(),
            });
            let schema = crate::graphql::build_schema(state, false);
            let auth: crate::auth::User = sqlx::query_as("SELECT * FROM users WHERE id=$1")
                .bind(crate::db_types::uuid(&user).unwrap())
                .fetch_one(pool.read())
                .await
                .unwrap();
            let cache = RequestUserCache::default();
            cache.0.set(Ok(Some(auth))).unwrap();
            let unticking =
                tokio::spawn(async move {
                    schema.execute(async_graphql::Request::new(
                    "mutation { setChapterRead(chapterId: \"reset-one\", read: false) }",
                ).data(cache)).await
                });
            let waited = wait_for_lock(&pool).await;
            locked.release();
            assert!(
                waited,
                "canonical untick did not wait for the reset's user lock"
            );
            Some(unticking)
        } else {
            locked.release();
            None
        };
        tokio::time::timeout(Duration::from_secs(15), resetting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Some(unticking) = unticking {
            let response = tokio::time::timeout(Duration::from_secs(15), unticking)
                .await
                .unwrap()
                .unwrap();
            assert!(response.errors.is_empty(), "{:?}", response.errors);
            assert!(!reads(&pool, &user, &md.work).await[&one]);
        } else {
            assert_eq!(reads(&pool, &user, &md.work).await, before);
            let after: serde_json::Value = sqlx::query_scalar(
                "SELECT jsonb_build_object('row', to_jsonb(p), 'version', p.xmin::text) FROM work_progress p WHERE user_id=$1 AND work_id=$2",
            )
            .bind(crate::db_types::uuid(&user).unwrap())
            .bind(&md.work)
            .fetch_one(pool.read())
            .await
            .unwrap();
            assert_eq!(after, progress_before);
        }
        let after: serde_json::Value = sqlx::query_scalar(
            "SELECT jsonb_build_object('row', to_jsonb(m), 'version', m.xmin::text) FROM work_chapter_mark m WHERE user_id=$1 AND work_chapter_id=$2",
        )
        .bind(crate::db_types::uuid(&user).unwrap())
        .bind(&two)
        .fetch_one(pool.read())
        .await
        .unwrap();
        assert_eq!(after, mark_before);
        let kept: bool = sqlx::query_scalar(
            "SELECT read FROM work_chapter_mark WHERE user_id=$1 AND work_chapter_id=$2",
        )
        .bind(crate::db_types::uuid(&user).unwrap())
        .bind(&three)
        .fetch_one(pool.read())
        .await
        .unwrap();
        assert!(kept, "numeric explicit read above P needs its own mark");
        let numeric_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM suwayomi_progress")
            .fetch_one(pool.read())
            .await
            .unwrap();
        assert_eq!(numeric_rows, 0);
    }
}

async fn read_at(pool: &Database, user: &str, work: &str, at: &str) {
    sqlx::query(
        "UPDATE work_progress SET updated_at = $3::timestamptz WHERE user_id = $1 AND work_id = $2",
    )
    .bind(crate::db_types::uuid(user).unwrap())
    .bind(work)
    .bind(at)
    .execute(pool.write())
    .await
    .unwrap();
}

async fn progress_row(pool: &Database, user: &str, work: &str) -> Option<(String, i64)> {
    sqlx::query_as(
        "SELECT to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD'), revision \
           FROM work_progress WHERE user_id = $1 AND work_id = $2",
    )
    .bind(crate::db_types::uuid(user).unwrap())
    .bind(work)
    .fetch_optional(pool.read())
    .await
    .unwrap()
}

#[tokio::test]
async fn merge_and_split_keep_each_readers_last_read_time() {
    let pool = pool().await;
    let loser = md_work(&pool, "Last read loser").await;
    let survivor = md_work(&pool, "Last read survivor").await;
    replace_source_chapters(
        &pool,
        &loser.source,
        &[
            chapter("lr-loser-one", Some("1"), "One"),
            chapter("lr-loser-two", Some("2"), "Two"),
            chapter("lr-loser-three", Some("3"), "Three"),
        ],
    )
    .await
    .unwrap();
    replace_source_chapters(
        &pool,
        &survivor.source,
        &[
            chapter("lr-survivor-one", Some("1"), "One"),
            chapter("lr-survivor-two", Some("2"), "Two"),
        ],
    )
    .await
    .unwrap();
    make_ready(&pool, &loser.work).await;
    make_ready(&pool, &survivor.work).await;
    let (l1, l2) = (
        entry(&pool, &loser.work, "100").await.id,
        entry(&pool, &loser.work, "200").await.id,
    );
    let (s1, s2) = (
        entry(&pool, &survivor.work, "100").await.id,
        entry(&pool, &survivor.work, "200").await.id,
    );
    let only_loser = reader(&pool).await;
    let loser_newer = reader(&pool).await;
    let survivor_newer = reader(&pool).await;
    pointer(&pool, &only_loser, &loser.work, &l2).await;
    read_at(&pool, &only_loser, &loser.work, "2026-01-01T00:00:00Z").await;
    pointer(&pool, &loser_newer, &survivor.work, &s1).await;
    read_at(&pool, &loser_newer, &survivor.work, "2026-02-01T00:00:00Z").await;
    pointer(&pool, &loser_newer, &loser.work, &l1).await;
    read_at(&pool, &loser_newer, &loser.work, "2026-03-01T00:00:00Z").await;
    pointer(&pool, &survivor_newer, &survivor.work, &s2).await;
    read_at(
        &pool,
        &survivor_newer,
        &survivor.work,
        "2026-04-01T00:00:00Z",
    )
    .await;
    pointer(&pool, &survivor_newer, &loser.work, &l1).await;
    read_at(&pool, &survivor_newer, &loser.work, "2026-01-15T00:00:00Z").await;

    merge_works(&pool, &loser.work, &survivor.work)
        .await
        .unwrap();
    // The revision moves; the last read is the reader's own, never the merge time.
    assert_eq!(
        progress_row(&pool, &only_loser, &survivor.work).await,
        Some(("2026-01-01".into(), 1))
    );
    assert_eq!(
        progress_row(&pool, &loser_newer, &survivor.work).await,
        Some(("2026-03-01".into(), 8))
    );
    assert_eq!(
        progress_row(&pool, &survivor_newer, &survivor.work).await,
        Some(("2026-04-01".into(), 8))
    );

    let split = split_source_series(&pool, &survivor.work, &[loser.source.clone()], None)
        .await
        .unwrap();
    assert_eq!(
        progress_row(&pool, &only_loser, &survivor.work).await,
        Some(("2026-01-01".into(), 2))
    );
    let moved = progress_row(&pool, &only_loser, &split.new_work_id)
        .await
        .expect("the reader's pointer follows the moving copies");
    assert_eq!(
        moved.0, "2026-01-01",
        "a new row takes the source row's time"
    );
}

#[tokio::test]
async fn merge_keeps_a_ready_survivor_ready_when_every_loser_copy_is_linked() {
    for unlinked in [false, true] {
        let pool = pool().await;
        let loser = md_work(&pool, "Ready loser").await;
        let survivor = md_work(&pool, "Ready survivor").await;
        replace_source_chapters(
            &pool,
            &loser.source,
            &[
                chapter("rl-loser-two", Some("2"), "Two"),
                chapter("rl-loser-three", Some("3"), "Three"),
            ],
        )
        .await
        .unwrap();
        replace_source_chapters(
            &pool,
            &survivor.source,
            &[
                chapter("rl-survivor-one", Some("1"), "One"),
                chapter("rl-survivor-two", Some("2"), "Two"),
                chapter("rl-survivor-three", Some("3"), "Three"),
            ],
        )
        .await
        .unwrap();
        // The loser was linked at ingest but the drain never visited it.
        make_ready(&pool, &survivor.work).await;
        if unlinked {
            sqlx::query(
                "DELETE FROM work_chapter_link WHERE chapter_id = \
                 (SELECT id FROM chapter WHERE source_series_id = $1 AND external_id = 'rl-loser-two')",
            )
            .bind(&loser.source)
            .execute(pool.write())
            .await
            .unwrap();
        }
        let user = reader(&pool).await;
        let s3 = entry(&pool, &survivor.work, "300").await.id;
        pointer(&pool, &user, &survivor.work, &s3).await;
        untick(&pool, &user, "rl-loser-three").await;

        merge_works(&pool, &loser.work, &survivor.work)
            .await
            .unwrap();
        let (ready, _) = work_chapter::admin_entry_list(&pool, &survivor.work)
            .await
            .unwrap();
        assert_eq!(ready, !unlinked);
        if unlinked {
            continue;
        }
        // Pointer-implied reads stay visible right after the merge.
        let state = reads(&pool, &user, &survivor.work).await;
        let s1 = entry(&pool, &survivor.work, "100").await.id;
        let s2 = entry(&pool, &survivor.work, "200").await.id;
        assert_eq!(state[&s1], true);
        assert_eq!(state[&s2], true);
        // The loser's legacy untick was converted, not left behind.
        let unticks: i64 =
            sqlx::query_scalar("SELECT count(*) FROM progress_untick WHERE user_id = $1")
                .bind(crate::db_types::uuid(&user).unwrap())
                .fetch_one(pool.read())
                .await
                .unwrap();
        assert_eq!(unticks, 0);
    }
}
