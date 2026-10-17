//! Progress rules and identity preservation against real PostgreSQL.
use super::*;
use crate::catalog::phase4_tests::pool;
use crate::catalog::work_chapter::{self, drain_once};
use crate::catalog::{self, ChapterInput, WorkInput};

const ALICE: &str = "00000000-0000-4000-8000-000000000001";
const BOB: &str = "00000000-0000-4000-8000-000000000002";
const CAROL: &str = "00000000-0000-4000-8000-000000000003";

struct Work {
    id: String,
    md: String,
}

async fn work(pool: &Database, title: &str) -> Work {
    let key = Uuid::new_v4().to_string();
    let id = catalog::upsert_work_from_mangadex(
        pool,
        &key,
        &WorkInput {
            primary_title: Some(title.into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (md, _) = catalog::find_source_series_by_key(pool, "mangadex", &key)
        .await
        .unwrap()
        .unwrap();
    Work { id, md }
}

async fn users(pool: &Database, ids: &[&str]) {
    for id in ids {
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, created_at) VALUES ($1, $2, $3, 'hash', now())",
        ).bind(crate::db_types::uuid(id).unwrap()).bind(id).bind(format!("{id}@example.test"))
            .execute(pool).await.unwrap();
    }
}

async fn source(pool: &Database, work: &str, key: &str) -> String {
    catalog::upsert_source_series(pool, work, "suwayomi", "source", key, None, false)
        .await
        .unwrap()
}

async fn copy(
    pool: &Database,
    source: &str,
    id: &str,
    number: &str,
    title: &str,
) -> (String, String) {
    catalog::upsert_chapter(
        pool,
        source,
        &ChapterInput {
            external_id: id.into(),
            number: Some(number.into()),
            title: Some(title.into()),
            lang: Some("en".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query_as(
        "SELECT c.id, l.work_chapter_id FROM chapter c JOIN work_chapter_link l ON l.chapter_id = c.id \
          WHERE c.source_series_id = $1 AND c.external_id = $2",
    ).bind(source).bind(id).fetch_one(pool.read()).await.unwrap()
}

async fn ready(pool: &Database, work: &str) {
    for _ in 0..10 {
        drain_once(pool, None).await.unwrap();
        let done: bool = sqlx::query_scalar(
            "SELECT COALESCE((SELECT ready FROM work_chapter_state WHERE work_id = $1), false)",
        )
        .bind(work)
        .fetch_one(pool.read())
        .await
        .unwrap();
        if done {
            return;
        }
    }
    panic!("fixture did not drain");
}

async fn pointer(pool: &Database, user: &str, work: &str, entry: &str, revision: i64) {
    sqlx::query(
        "INSERT INTO work_progress (user_id, work_id, through_entry_id, revision, updated_at) \
         VALUES ($1, $2, $3, $4, '2026-01-02T03:04:05Z') \
         ON CONFLICT (user_id, work_id) DO UPDATE SET through_entry_id = EXCLUDED.through_entry_id, revision = EXCLUDED.revision",
    ).bind(crate::db_types::uuid(user).unwrap()).bind(work).bind(entry).bind(revision)
        .execute(pool).await.unwrap();
}

async fn mark(pool: &Database, user: &str, entry: &str, read: bool) {
    sqlx::query(
        "INSERT INTO work_chapter_mark (user_id, work_chapter_id, read, created_at) VALUES ($1, $2, $3, now()) \
         ON CONFLICT (user_id, work_chapter_id) DO UPDATE SET read = EXCLUDED.read",
    ).bind(crate::db_types::uuid(user).unwrap()).bind(entry).bind(read).execute(pool).await.unwrap();
}

async fn md_read(pool: &Database, user: &str, work: &str, chapter: &str) {
    sqlx::query(
        "INSERT INTO canonical_progress (user_id, work_id, chapter_id, read, updated_at) \
         VALUES ($1, $2, $3, true, '2026-01-01T00:00:00Z')",
    )
    .bind(crate::db_types::uuid(user).unwrap())
    .bind(work)
    .bind(chapter)
    .execute(pool)
    .await
    .unwrap();
}

async fn sw_read(pool: &Database, user: &str, series: &str, chapter: &str) {
    sqlx::query(
        "INSERT INTO suwayomi_progress (user_id, series_id, chapter_id, read, updated_at) \
         VALUES ($1, $2, $3, true, '2026-01-03T00:00:00Z')",
    )
    .bind(crate::db_types::uuid(user).unwrap())
    .bind(series)
    .bind(chapter)
    .execute(pool)
    .await
    .unwrap();
}

async fn stored_mark(pool: &Database, user: &str, entry: &str) -> Option<bool> {
    sqlx::query_scalar(
        "SELECT read FROM work_chapter_mark WHERE user_id = $1 AND work_chapter_id = $2",
    )
    .bind(crate::db_types::uuid(user).unwrap())
    .bind(entry)
    .fetch_optional(pool.read())
    .await
    .unwrap()
}

async fn snapshot(pool: &Database, readers: &[String], works: &[String]) -> Before {
    let mut tx = pool.begin().await.unwrap();
    let before = keep_state_before(&mut tx, readers, works).await.unwrap();
    tx.rollback().await.unwrap();
    before
}

async fn lock_identity(
    conn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    works: &[String],
) -> Vec<String> {
    let readers = affected_users(conn, works).await.unwrap();
    crate::locks::lock_users(conn, &readers).await.unwrap();
    crate::locks::lock_works(conn, works).await.unwrap();
    assert_eq!(affected_users(conn, works).await.unwrap(), readers);
    readers
}

#[test]
fn pure_read_helpers_and_numeric_order() {
    let state = WorkReadState::Ready {
        entries: HashMap::from([("one".into(), false), ("two".into(), true)]),
        copies: HashMap::from([(("mangadex".into(), "copy".into()), false)]),
        revision: 9,
        through: Some("two".into()),
    };
    assert!(!state.copy_read("mangadex", "copy", "100", true));
    assert!(state.copy_read("suwayomi", "unlinked", "100", true));
    assert!(!state.copy_read("suwayomi", "unlinked", "100", false));
    assert!(state.row_read(Some("two"), "irrelevant"));
    assert!(!state.row_read(None, "100"));
    assert_eq!(state.viewer(), Some((9, Some("two".into()))));
    assert!(WorkReadState::Empty.copy_read("mangadex", "c", "k", true));
    assert_eq!(WorkReadState::Empty.viewer(), None);
    let ordered = [
        "-Infinity",
        "-100",
        "-1.01",
        "-1.001",
        "0",
        "0.0001",
        "1",
        "1.000000000000000000001",
        "1.01",
        "10",
        "Infinity",
        "NaN",
    ];
    for (i, a) in ordered.iter().enumerate() {
        for (j, b) in ordered.iter().enumerate() {
            assert_eq!(compare_numeric(a, b), i.cmp(&j), "{a} versus {b}");
        }
    }
    assert_eq!(compare_numeric("-0.00", "+000"), Ordering::Equal);
    assert_eq!(compare_numeric("001.2300", "1.23"), Ordering::Equal);
}

#[tokio::test]
async fn read_rule_matrix_and_statement_budget() {
    let pool = pool().await;
    users(&pool, &[ALICE]).await;
    let w = work(&pool, "Read matrix").await;
    let sw = source(&pool, &w.id, "7").await;
    let (_, one) = copy(&pool, &w.md, "md-one", "1", "One").await;
    let (_, two) = copy(&pool, &w.md, "md-two", "2", "Two").await;
    let (_, three) = copy(&pool, &w.md, "md-three", "3", "Three").await;
    let (_, four) = copy(&pool, &w.md, "md-four", "4", "Four").await;
    let (_, special) = copy(&pool, &w.md, "md-special", "1.5", "Extra").await;
    let (unlinked, _) = copy(&pool, &w.md, "md-unlinked", "5", "Five").await;
    copy(&pool, &sw, "101", "1", "One").await;
    copy(&pool, &sw, "102", "2", "Two").await;
    copy(&pool, &sw, "104", "4", "Four").await;
    ready(&pool, &w.id).await;
    pointer(&pool, ALICE, &w.id, &two, 7).await;
    md_read(&pool, ALICE, &w.id, "md-one").await;
    sw_read(&pool, ALICE, "7", "104").await;
    mark(&pool, ALICE, &one, false).await;
    mark(&pool, ALICE, &three, true).await;
    sqlx::query("DELETE FROM work_chapter_link WHERE chapter_id = $1")
        .bind(&unlinked)
        .execute(&pool)
        .await
        .unwrap();
    // A copy first seen after the pointer was saved still inherits the entry rule.
    copy(&pool, &sw, "100", "0.5", "Late lower entry").await;
    copy(&pool, &sw, "199", "2", "Late copy of two").await;
    let sent = pool.statements_sent();
    let state = work_read_state(&pool, Some(ALICE), &w.id).await;
    assert_eq!(pool.statements_sent() - sent, 1);
    assert_eq!(state.viewer(), Some((7, Some(two.clone()))));
    assert!(!state.copy_read("mangadex", "md-one", "100", true));
    assert!(!state.copy_read("suwayomi", "101", "100", false));
    assert!(state.copy_read("mangadex", "md-two", "200", false));
    assert!(state.copy_read("suwayomi", "102", "200", false));
    assert!(state.copy_read("suwayomi", "100", "50", false));
    assert!(state.copy_read("suwayomi", "199", "200", false));
    assert!(state.row_read(Some(&three), "300"));
    assert!(state.row_read(Some(&four), "400"));
    assert!(!state.row_read(Some(&special), "150"));
    assert!(!state.copy_read("mangadex", "md-unlinked", "200", false));
    assert!(state.copy_read("mangadex", "md-unlinked", "200", true));
    let sent = pool.statements_sent();
    assert!(matches!(
        work_read_state(&pool, None, &w.id).await,
        WorkReadState::Empty
    ));
    assert!(matches!(
        work_read_state(&pool, Some("bad uuid"), &w.id).await,
        WorkReadState::Empty
    ));
    assert!(matches!(
        work_read_state(&pool, Some(ALICE), "").await,
        WorkReadState::Empty
    ));
    assert_eq!(pool.statements_sent(), sent);
    assert!(matches!(
        work_read_state(&pool, Some(ALICE), "missing").await,
        WorkReadState::Empty
    ));

    let cold = work(&pool, "Key fold").await;
    let cold_sw = source(&pool, &cold.id, "8").await;
    let (_, folded) = copy(&pool, &cold.md, "cold-md", "1", "One").await;
    copy(&pool, &cold_sw, "201", "1", "One").await;
    md_read(&pool, ALICE, &cold.id, "cold-md").await;
    mark(&pool, ALICE, &folded, false).await;
    pointer(&pool, ALICE, &cold.id, &folded, 4).await;
    let state = work_read_state(&pool, Some(ALICE), &cold.id).await;
    assert!(matches!(state, WorkReadState::NotReady { .. }));
    assert!(state.copy_read("suwayomi", "201", "100", false));
    assert!(state.row_read(Some(&folded), "100"));
    assert_eq!(state.viewer(), Some((4, Some(folded))));
}

#[tokio::test]
async fn explicit_reads_survive_deletion_without_overriding_other_state() {
    let pool = pool().await;
    users(&pool, &[ALICE]).await;
    let w = work(&pool, "Deletion").await;
    let sw = source(&pool, &w.id, "9").await;
    let mut chapters = Vec::new();
    let mut entries = Vec::new();
    for (id, number, title) in [
        ("gone-one", "1", "One"),
        ("gone-two", "2", "Two"),
        ("gone-three", "3", "Three"),
        ("gone-four", "4", "Four"),
        ("gone-five", "5", "Five"),
        ("gone-six", "6", "Six"),
        ("gone-special", "1.5", "Special"),
    ] {
        let (chapter, entry) = copy(&pool, &w.md, id, number, title).await;
        chapters.push(chapter);
        entries.push(entry);
        md_read(&pool, ALICE, &w.id, id).await;
    }
    copy(&pool, &sw, "905", "5", "Five").await;
    copy(&pool, &sw, "906", "5", "Five again").await;
    sw_read(&pool, ALICE, "9", "905").await;
    sw_read(&pool, ALICE, "9", "906").await;
    ready(&pool, &w.id).await;
    pointer(&pool, ALICE, &w.id, &entries[1], 8).await;
    mark(&pool, ALICE, &entries[3], true).await;
    mark(&pool, ALICE, &entries[5], false).await;
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(keep_explicit_reads(&mut tx, &chapters).await.unwrap(), 2);
    sqlx::query("DELETE FROM chapter WHERE id = ANY($1)")
        .bind(&chapters)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(stored_mark(&pool, ALICE, &entries[0]).await, None);
    assert_eq!(stored_mark(&pool, ALICE, &entries[1]).await, None);
    assert_eq!(stored_mark(&pool, ALICE, &entries[2]).await, Some(true));
    assert_eq!(stored_mark(&pool, ALICE, &entries[3]).await, Some(true));
    assert_eq!(stored_mark(&pool, ALICE, &entries[4]).await, None);
    assert_eq!(stored_mark(&pool, ALICE, &entries[5]).await, Some(false));
    assert_eq!(stored_mark(&pool, ALICE, &entries[6]).await, Some(true));
    let state = work_read_state(&pool, Some(ALICE), &w.id).await;
    assert!(state.row_read(Some(&entries[2]), "300"));
    assert!(!state.row_read(Some(&entries[5]), "600"));
    assert_eq!(state.viewer(), Some((8, Some(entries[1].clone()))));
    // The store-wide reset must exclude every vanishing sibling, not just one copy.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        keep_explicit_reads_of_store(&mut tx, "suwayomi")
            .await
            .unwrap(),
        1
    );
    tx.commit().await.unwrap();
    assert_eq!(stored_mark(&pool, ALICE, &entries[4]).await, Some(true));
}

#[tokio::test]
async fn unticks_convert_only_linked_copies_of_the_matching_store() {
    let pool = pool().await;
    users(&pool, &[ALICE, BOB, CAROL]).await;
    let w = work(&pool, "Unticks").await;
    let sw = source(&pool, &w.id, "10").await;
    let (_, one) = copy(&pool, &w.md, "untick-md", "1", "One").await;
    copy(&pool, &sw, "1001", "1", "One").await;
    // Numeric ids on the MangaDex side do not consume Suwayomi unticks.
    copy(&pool, &w.md, "1002", "2", "Two").await;
    let (unlinked, _) = copy(&pool, &w.md, "untick-unlinked", "3", "Three").await;
    sqlx::query("DELETE FROM work_chapter_link WHERE chapter_id = $1")
        .bind(unlinked)
        .execute(&pool)
        .await
        .unwrap();
    mark(&pool, ALICE, &one, true).await;
    for (user, chapter) in [
        (ALICE, "untick-md"),
        (ALICE, "1001"),
        (BOB, "1001"),
        (CAROL, "1002"),
        (CAROL, "untick-unlinked"),
        (CAROL, "missing"),
    ] {
        sqlx::query(
            "INSERT INTO progress_untick (user_id, chapter_id, created_at) VALUES ($1, $2, now())",
        )
        .bind(crate::db_types::uuid(user).unwrap())
        .bind(chapter)
        .execute(&pool)
        .await
        .unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        untick_users(&mut tx, &w.id).await.unwrap(),
        [ALICE, BOB, CAROL]
    );
    let affected = affected_users(&mut tx, &[w.id.clone()]).await.unwrap();
    assert_eq!(affected, [ALICE, BOB, CAROL]);
    crate::locks::lock_users(&mut tx, &[ALICE, BOB, CAROL])
        .await
        .unwrap();
    assert_eq!(convert_unticks_on(&mut tx, &w.id).await.unwrap(), 3);
    assert_eq!(untick_users(&mut tx, &w.id).await.unwrap(), [CAROL]);
    assert_eq!(convert_unticks_on(&mut tx, &w.id).await.unwrap(), 0);
    tx.commit().await.unwrap();
    assert_eq!(stored_mark(&pool, ALICE, &one).await, Some(false));
    assert_eq!(stored_mark(&pool, BOB, &one).await, Some(false));
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM progress_untick")
        .fetch_one(pool.read())
        .await
        .unwrap();
    assert_eq!(remaining, 3);
}

#[tokio::test]
async fn engine_reset_unticks_become_marks_before_their_rows_go() {
    let pool = pool().await;
    users(&pool, &[ALICE]).await;
    let w = work(&pool, "Reset unticks").await;
    let sw = source(&pool, &w.id, "20").await;
    let (_, one) = copy(&pool, &w.md, "reset-md-one", "1", "One").await;
    copy(&pool, &sw, "2001", "1", "One").await;
    let (_, two) = copy(&pool, &w.md, "reset-md-two", "2", "Two").await;
    // A finish on the not-ready work set the pointer; the numeric untick keeps
    // entry one unread until the drain converts it.
    pointer(&pool, ALICE, &w.id, &two, 1).await;
    for chapter in ["2001", "reset-md-two"] {
        sqlx::query(
            "INSERT INTO progress_untick (user_id, chapter_id, created_at) VALUES ($1, $2, now())",
        )
        .bind(crate::db_types::uuid(ALICE).unwrap())
        .bind(chapter)
        .execute(&pool)
        .await
        .unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    crate::locks::lock_users(&mut tx, &[ALICE]).await.unwrap();
    assert_eq!(keep_unticks_of_store(&mut tx, "suwayomi").await.unwrap(), 1);
    tx.commit().await.unwrap();
    assert_eq!(stored_mark(&pool, ALICE, &one).await, Some(false));
    assert_eq!(
        stored_mark(&pool, ALICE, &two).await,
        None,
        "other stores wait for the drain"
    );
    let left: Vec<String> = sqlx::query_scalar("SELECT chapter_id FROM progress_untick")
        .fetch_all(pool.read())
        .await
        .unwrap();
    assert_eq!(left, ["reset-md-two"]);
}

#[tokio::test]
async fn merge_keeps_truth_values_and_adopts_a_loser_pointer() {
    let pool = pool().await;
    users(&pool, &[ALICE, BOB]).await;
    let survivor = work(&pool, "Survivor").await;
    let loser = work(&pool, "Loser").await;
    let (_, survivor_two) = copy(&pool, &survivor.md, "survivor-two", "2", "Two").await;
    let (_, survivor_five) = copy(&pool, &survivor.md, "survivor-five", "5", "Five").await;
    let (_, loser_one) = copy(&pool, &loser.md, "loser-one", "1", "One").await;
    let (_, loser_two) = copy(&pool, &loser.md, "loser-two", "2", "Two").await;
    let (_, loser_four) = copy(&pool, &loser.md, "loser-four", "4", "Four").await;
    let (_, loser_eight) = copy(&pool, &loser.md, "loser-eight", "8", "Eight").await;
    ready(&pool, &survivor.id).await;
    ready(&pool, &loser.id).await;
    pointer(&pool, ALICE, &survivor.id, &survivor_five, 10).await;
    pointer(&pool, BOB, &loser.id, &loser_four, 4).await;
    md_read(&pool, ALICE, &loser.id, "loser-eight").await;
    let works = [survivor.id.clone(), loser.id.clone()];
    let mut tx = pool.begin().await.unwrap();
    let readers = lock_identity(&mut tx, &works).await;
    let before = keep_state_before(&mut tx, &readers, &works).await.unwrap();
    let old = before.effective.clone();
    let moves = work_chapter::merge_entries_on(&mut tx, &loser.id, &survivor.id)
        .await
        .unwrap();
    sqlx::query("UPDATE source_series SET work_id = $2 WHERE work_id = $1")
        .bind(&loser.id)
        .bind(&survivor.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE canonical_progress SET work_id = $2 WHERE work_id = $1")
        .bind(&loser.id)
        .bind(&survivor.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    keep_state_after(
        &mut tx,
        before,
        &moves,
        &[],
        PointerPlan::Merge {
            survivor: &survivor.id,
            loser: &loser.id,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let after = snapshot(&pool, &readers, &[survivor.id.clone()]).await;
    for user in [ALICE, BOB] {
        let uid = crate::db_types::uuid(user).unwrap();
        for entry in after.entries.keys() {
            let want = old
                .iter()
                .filter(|((u, old_id), _)| {
                    *u == uid
                        && (old_id == entry
                            || moves.iter().any(|m| m.from == *old_id && m.to == *entry))
                })
                .any(|(_, read)| *read);
            assert_eq!(
                after.effective[&(uid, entry.clone())],
                want,
                "{user}: {entry}"
            );
        }
    }
    assert_eq!(stored_mark(&pool, ALICE, &loser_one).await, Some(false));
    assert_eq!(stored_mark(&pool, ALICE, &loser_four).await, Some(false));
    assert!(work_read_state(&pool, Some(ALICE), &survivor.id)
        .await
        .row_read(Some(&loser_eight), "800"));
    assert_eq!(
        work_read_state(&pool, Some(ALICE), &survivor.id)
            .await
            .viewer(),
        Some((11, Some(survivor_five)))
    );
    assert_eq!(
        work_read_state(&pool, Some(BOB), &survivor.id)
            .await
            .viewer(),
        Some((1, Some(loser_four)))
    );
    assert!(moves.contains(&EntryMove {
        from: loser_two,
        to: survivor_two
    }));
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM work_progress WHERE work_id = $1")
            .bind(&loser.id)
            .fetch_one(pool.read())
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn merge_kept_read_drops_the_losers_pending_untick() {
    let pool = pool().await;
    users(&pool, &[ALICE]).await;
    let survivor = work(&pool, "Kept survivor").await;
    let loser = work(&pool, "Kept loser").await;
    let (_, kept) = copy(&pool, &survivor.md, "kept-two", "2", "Two").await;
    copy(&pool, &loser.md, "kept-loser-two", "2", "Two").await;
    ready(&pool, &survivor.id).await;
    // The loser stays not ready: its untick is still a legacy record.
    mark(&pool, ALICE, &kept, true).await;
    sqlx::query(
        "INSERT INTO progress_untick (user_id, chapter_id, created_at) VALUES ($1, 'kept-loser-two', now() + interval '1 hour')",
    )
    .bind(crate::db_types::uuid(ALICE).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let works = [survivor.id.clone(), loser.id.clone()];
    let mut tx = pool.begin().await.unwrap();
    let readers = lock_identity(&mut tx, &works).await;
    let before = keep_state_before(&mut tx, &readers, &works).await.unwrap();
    let moves = work_chapter::merge_entries_on(&mut tx, &loser.id, &survivor.id)
        .await
        .unwrap();
    sqlx::query("UPDATE source_series SET work_id = $2 WHERE work_id = $1")
        .bind(&loser.id)
        .bind(&survivor.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    keep_state_after(
        &mut tx,
        before,
        &moves,
        &[],
        PointerPlan::Merge {
            survivor: &survivor.id,
            loser: &loser.id,
        },
    )
    .await
    .unwrap();
    // A later drain of the merged work converts what unticks are left.
    convert_unticks_on(&mut tx, &survivor.id).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(stored_mark(&pool, ALICE, &kept).await, Some(true));
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM progress_untick")
        .fetch_one(pool.read())
        .await
        .unwrap();
    assert_eq!(left, 0);
}

async fn move_mapping(conn: &mut PgConnection, source: &str, destination: &str) {
    sqlx::query(
        "UPDATE canonical_progress cp SET work_id = $2 WHERE EXISTS \
         (SELECT 1 FROM chapter c JOIN source_series ss ON ss.id = c.source_series_id \
           WHERE ss.id = $1 AND ss.source_type = 'mangadex' AND c.external_id = cp.chapter_id AND cp.work_id = ss.work_id)",
    ).bind(source).bind(destination).execute(&mut *conn).await.unwrap();
    sqlx::query("UPDATE source_series SET work_id = $2 WHERE id = $1")
        .bind(source)
        .bind(destination)
        .execute(conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn split_new_derives_pointers_and_preserves_every_reader_entry() {
    let pool = pool().await;
    users(&pool, &[ALICE, BOB]).await;
    let source_work = work(&pool, "Split source").await;
    let dest = work(&pool, "New destination").await;
    let sw = source(&pool, &source_work.id, "11").await;
    let mut old_entries = Vec::new();
    for n in 1..=5 {
        let (_, entry) = copy(
            &pool,
            &source_work.md,
            &format!("split-md-{n}"),
            &n.to_string(),
            "Chapter",
        )
        .await;
        old_entries.push(entry);
    }
    copy(&pool, &sw, "1102", "2", "Two").await;
    copy(&pool, &sw, "1104", "4", "Four").await;
    ready(&pool, &source_work.id).await;
    pointer(&pool, ALICE, &source_work.id, &old_entries[2], 6).await;
    pointer(&pool, BOB, &source_work.id, &old_entries[4], 2).await;
    mark(&pool, ALICE, &old_entries[1], false).await;
    md_read(&pool, ALICE, &source_work.id, "split-md-5").await;
    mark(&pool, BOB, &old_entries[3], false).await;
    let works = [source_work.id.clone(), dest.id.clone()];
    let mut tx = pool.begin().await.unwrap();
    let readers = lock_identity(&mut tx, &works).await;
    let before = keep_state_before(&mut tx, &readers, &works).await.unwrap();
    let old = before.effective.clone();
    let moves = work_chapter::split_entries_on(
        &mut tx,
        &source_work.id,
        &dest.id,
        &[source_work.md.clone()],
        true,
    )
    .await
    .unwrap();
    move_mapping(&mut tx, &source_work.md, &dest.id).await;
    keep_state_after(
        &mut tx,
        before,
        &moves,
        &[],
        PointerPlan::Split {
            source: &source_work.id,
            dest: &dest.id,
            dest_is_new: true,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // A new destination becomes ready through the real drain, just as in production.
    ready(&pool, &dest.id).await;
    let after = snapshot(&pool, &readers, &works).await;
    for user in [ALICE, BOB] {
        let uid = crate::db_types::uuid(user).unwrap();
        for ((u, old_id), read) in &old {
            if *u != uid {
                continue;
            }
            let mut targets = moves
                .iter()
                .filter(|m| m.from == *old_id)
                .map(|m| m.to.clone())
                .collect::<HashSet<_>>();
            if after.entries.contains_key(old_id) {
                targets.insert(old_id.clone());
            }
            for target in targets {
                assert_eq!(
                    after.effective[&(uid, target.clone())],
                    *read,
                    "{user}: {old_id} -> {target}"
                );
            }
        }
    }
    let alice_source = work_read_state(&pool, Some(ALICE), &source_work.id)
        .await
        .viewer()
        .unwrap();
    let alice_dest = work_read_state(&pool, Some(ALICE), &dest.id)
        .await
        .viewer()
        .unwrap();
    assert_eq!(alice_source, (7, Some(old_entries[1].clone())));
    assert_eq!(alice_dest, (1, Some(old_entries[2].clone())));
    assert_eq!(
        work_read_state(&pool, Some(BOB), &source_work.id)
            .await
            .viewer(),
        Some((3, Some(old_entries[3].clone())))
    );
    assert_eq!(
        work_read_state(&pool, Some(BOB), &dest.id).await.viewer(),
        Some((1, Some(old_entries[4].clone())))
    );
}

#[tokio::test]
async fn split_existing_keeps_destination_pointer_and_ors_colliding_entries() {
    let pool = pool().await;
    users(&pool, &[ALICE]).await;
    let source_work = work(&pool, "Existing split source").await;
    let dest = work(&pool, "Existing destination").await;
    let sw = source(&pool, &source_work.id, "12").await;
    let (_, src_two) = copy(&pool, &source_work.md, "existing-src-two", "2", "Two").await;
    copy(&pool, &sw, "1202", "2", "Two").await;
    let (_, dst_one) = copy(&pool, &dest.md, "existing-dst-one", "1", "One").await;
    let (_, dst_two) = copy(&pool, &dest.md, "existing-dst-two", "2", "Two").await;
    ready(&pool, &source_work.id).await;
    ready(&pool, &dest.id).await;
    pointer(&pool, ALICE, &source_work.id, &src_two, 2).await;
    pointer(&pool, ALICE, &dest.id, &dst_one, 8).await;
    let works = [source_work.id.clone(), dest.id.clone()];
    let mut tx = pool.begin().await.unwrap();
    let readers = lock_identity(&mut tx, &works).await;
    let before = keep_state_before(&mut tx, &readers, &works).await.unwrap();
    let moves = work_chapter::split_entries_on(
        &mut tx,
        &source_work.id,
        &dest.id,
        &[source_work.md.clone()],
        false,
    )
    .await
    .unwrap();
    move_mapping(&mut tx, &source_work.md, &dest.id).await;
    keep_state_after(
        &mut tx,
        before,
        &moves,
        &[],
        PointerPlan::Split {
            source: &source_work.id,
            dest: &dest.id,
            dest_is_new: false,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(moves.contains(&EntryMove {
        from: src_two.clone(),
        to: dst_two.clone()
    }));
    let dst = work_read_state(&pool, Some(ALICE), &dest.id).await;
    assert_eq!(dst.viewer(), Some((9, Some(dst_one))));
    assert!(dst.row_read(Some(&dst_two), "200"));
    assert_eq!(stored_mark(&pool, ALICE, &dst_two).await, Some(true));
    assert_eq!(
        work_read_state(&pool, Some(ALICE), &source_work.id)
            .await
            .viewer(),
        Some((3, Some(src_two)))
    );
}

#[tokio::test]
async fn edit_relink_uses_the_copys_own_read_and_merge_maps_a_deleted_pointer() {
    let pool = pool().await;
    users(&pool, &[ALICE, BOB]).await;
    let w = work(&pool, "Edits").await;
    let sw = source(&pool, &w.id, "13").await;
    let (copy_one, one) = copy(&pool, &w.md, "edit-one", "1", "One").await;
    copy(&pool, &sw, "1301", "1", "One").await;
    let (_, two) = copy(&pool, &w.md, "edit-two", "2", "Two").await;
    let (_, three) = copy(&pool, &w.md, "edit-three", "3", "Three").await;
    ready(&pool, &w.id).await;
    pointer(&pool, ALICE, &w.id, &one, 3).await;
    md_read(&pool, ALICE, &w.id, "edit-one").await;
    // Own true rows count at a relink destination even when a false entry mark hid them.
    mark(&pool, ALICE, &one, false).await;
    pointer(&pool, BOB, &w.id, &three, 5).await;
    mark(&pool, BOB, &two, false).await;
    let works = [w.id.clone()];
    let mut tx = pool.begin().await.unwrap();
    let readers = lock_identity(&mut tx, &works).await;
    let before = keep_state_before(&mut tx, &readers, &works).await.unwrap();
    sqlx::query("UPDATE work_chapter_link SET work_chapter_id = $2 WHERE chapter_id = $1")
        .bind(&copy_one)
        .bind(&two)
        .execute(&mut *tx)
        .await
        .unwrap();
    keep_state_after(
        &mut tx,
        before,
        &[EntryMove {
            from: one.clone(),
            to: two.clone(),
        }],
        &[(copy_one.clone(), two.clone())],
        PointerPlan::Edit { work: &w.id },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let state = work_read_state(&pool, Some(ALICE), &w.id).await;
    assert!(!state.row_read(Some(&one), "100"));
    assert!(state.row_read(Some(&two), "200"));
    assert_eq!(state.viewer(), Some((4, Some(one.clone()))));
    assert_eq!(
        work_read_state(&pool, Some(BOB), &w.id).await.viewer(),
        Some((6, Some(three.clone())))
    );

    let mut tx = pool.begin().await.unwrap();
    let readers = lock_identity(&mut tx, &works).await;
    let before = keep_state_before(&mut tx, &readers, &works).await.unwrap();
    sqlx::query("UPDATE work_chapter_link SET work_chapter_id = $2 WHERE work_chapter_id = $1")
        .bind(&three)
        .bind(&two)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM work_chapter WHERE id = $1")
        .bind(&three)
        .execute(&mut *tx)
        .await
        .unwrap();
    keep_state_after(
        &mut tx,
        before,
        &[EntryMove {
            from: three,
            to: two.clone(),
        }],
        &[],
        PointerPlan::Edit { work: &w.id },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        work_read_state(&pool, Some(BOB), &w.id).await.viewer(),
        Some((7, Some(two.clone())))
    );
    assert_eq!(stored_mark(&pool, BOB, &two).await, Some(true));
    assert_eq!(
        work_read_state(&pool, Some(ALICE), &w.id).await.viewer(),
        Some((5, Some(one)))
    );
}

#[tokio::test]
async fn ready_library_counts_visible_populated_entries_and_formats_latest_read() {
    let pool = pool().await;
    users(&pool, &[ALICE, BOB]).await;
    let w = work(&pool, "Library").await;
    let sw = source(&pool, &w.id, "14").await;
    let (_, one) = copy(&pool, &w.md, "library-one", "1", "One").await;
    let (_, two) = copy(&pool, &w.md, "library-two", "2", "Two").await;
    let (_, three) = copy(&pool, &w.md, "library-three", "3", "Three").await;
    let (_, hidden) = copy(&pool, &w.md, "library-hidden", "4", "Four").await;
    let (gone, _) = copy(&pool, &w.md, "library-gone", "5", "Five").await;
    copy(&pool, &sw, "1401", "1", "One").await;
    copy(&pool, &sw, "1403", "3", "Three").await;
    ready(&pool, &w.id).await;
    sqlx::query("UPDATE work_chapter SET hidden = true WHERE id = $1")
        .bind(&hidden)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM chapter WHERE id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .unwrap();
    pointer(&pool, ALICE, &w.id, &two, 1).await;
    md_read(&pool, ALICE, &w.id, "library-one").await;
    sw_read(&pool, ALICE, "14", "1403").await;
    mark(&pool, ALICE, &one, false).await;
    let cold = work(&pool, "Not ready library").await;
    copy(&pool, &cold.md, "library-cold", "1", "One").await;
    let empty = work(&pool, "Empty ready library").await;
    ready(&pool, &empty.id).await;
    // Keep the cold fixture not ready after the shared drain visited it.
    sqlx::query("UPDATE work_chapter_state SET ready = false WHERE work_id = $1")
        .bind(&cold.id)
        .execute(&pool)
        .await
        .unwrap();
    for id in [&w.id, &"14".to_owned(), &cold.id, &empty.id] {
        sqlx::query(
            "INSERT INTO user_library (user_id, series_id, created_at) VALUES ($1, $2, now())",
        )
        .bind(crate::db_types::uuid(ALICE).unwrap())
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    let sent = pool.statements_sent();
    let progress = ready_library_progress(&pool, ALICE).await.unwrap();
    assert_eq!(pool.statements_sent() - sent, 1);
    assert_eq!(
        progress[&w.id],
        (2, 3, Some("2026-01-03T00:00:00.000000+00:00".into()))
    );
    assert_eq!(progress[&empty.id], (0, 0, None));
    assert!(!progress.contains_key(&cold.id));
    assert_eq!(progress.len(), 2);
    assert!(ready_library_progress(&pool, BOB).await.unwrap().is_empty());
    assert!(work_read_state(&pool, Some(ALICE), &w.id)
        .await
        .row_read(Some(&three), "300"));
    // With no later true copy row, the pointer supplies the activity timestamp.
    sqlx::query("DELETE FROM suwayomi_progress WHERE user_id = $1")
        .bind(crate::db_types::uuid(ALICE).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        ready_library_progress(&pool, ALICE).await.unwrap()[&w.id].2,
        Some("2026-01-02T03:04:05.000000+00:00".into())
    );
}

#[tokio::test]
async fn not_ready_snapshot_folds_keys_and_keeps_redundant_marks_untouched() {
    let pool = pool().await;
    users(&pool, &[ALICE, BOB, CAROL]).await;
    let w = work(&pool, "Legacy snapshot").await;
    let sw = source(&pool, &w.id, "15").await;
    let (_, one) = copy(&pool, &w.md, "snapshot-one", "1", "One").await;
    let (sw_copy, _) = copy(&pool, &sw, "1501", "1", "One").await;
    let separate = "wc_snapshot_separate";
    sqlx::query(
        "INSERT INTO work_chapter (id, work_id, sort_key, special, hidden, created_at) \
         SELECT $1, work_id, sort_key, false, false, now() FROM work_chapter WHERE id = $2",
    )
    .bind(separate)
    .bind(&one)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE work_chapter_link SET work_chapter_id = $2 WHERE chapter_id = $1")
        .bind(sw_copy)
        .bind(separate)
        .execute(&pool)
        .await
        .unwrap();
    md_read(&pool, ALICE, &w.id, "snapshot-one").await;
    sw_read(&pool, BOB, "15", "1501").await;
    mark(&pool, ALICE, separate, false).await;
    mark(&pool, ALICE, &one, true).await;
    let old_version: String = sqlx::query_scalar(
        "SELECT xmin::text FROM work_chapter_mark WHERE user_id = $1 AND work_chapter_id = $2",
    )
    .bind(crate::db_types::uuid(ALICE).unwrap())
    .bind(&one)
    .fetch_one(pool.read())
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    crate::locks::lock_users(&mut tx, &[ALICE, BOB, CAROL])
        .await
        .unwrap();
    crate::locks::lock_works(&mut tx, &[&w.id]).await.unwrap();
    let readers = [ALICE.to_owned(), BOB.to_owned(), CAROL.to_owned()];
    let before = keep_state_before(&mut tx, &readers, &[w.id.clone()])
        .await
        .unwrap();
    for user in [ALICE, BOB] {
        let uid = crate::db_types::uuid(user).unwrap();
        assert!(before.effective[&(uid, one.clone())]);
        assert!(before.effective[&(uid, separate.into())]);
    }
    assert!(!before.effective[&(crate::db_types::uuid(CAROL).unwrap(), one.clone())]);
    keep_state_after(&mut tx, before, &[], &[], PointerPlan::Edit { work: &w.id })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    ready(&pool, &w.id).await;
    for user in [ALICE, BOB] {
        let state = work_read_state(&pool, Some(user), &w.id).await;
        assert!(state.row_read(Some(&one), "100"));
        assert!(state.row_read(Some(separate), "100"));
        assert_eq!(state.viewer(), Some((1, None)));
    }
    let new_version: String = sqlx::query_scalar(
        "SELECT xmin::text FROM work_chapter_mark WHERE user_id = $1 AND work_chapter_id = $2",
    )
    .bind(crate::db_types::uuid(ALICE).unwrap())
    .bind(&one)
    .fetch_one(pool.read())
    .await
    .unwrap();
    assert_eq!(new_version, old_version);
    let untouched: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM work_progress WHERE user_id = $1")
            .bind(crate::db_types::uuid(CAROL).unwrap())
            .fetch_one(pool.read())
            .await
            .unwrap();
    assert_eq!(untouched, 0);
}

#[test]
fn pure_pointer_plans_and_entry_targets() {
    let uid = crate::db_types::uuid(ALICE).unwrap();
    let make = |rows: &[(&str, &str, &str)]| Before {
        users: vec![uid],
        entries: rows
            .iter()
            .map(|(id, work, sort)| {
                (
                    id.to_string(),
                    Entry {
                        id: id.to_string(),
                        work_id: work.to_string(),
                        sort_key: sort.to_string(),
                        special: false,
                        ready: true,
                        keys: Vec::new(),
                    },
                )
            })
            .collect(),
        pointers: HashMap::new(),
        marks: HashMap::new(),
        explicit: HashSet::new(),
        own_true: HashSet::new(),
        had_state: HashSet::new(),
        effective: HashMap::new(),
    };
    let mut before = make(&[
        ("old", "source", "2.000000000000000000001"),
        ("shared", "source", "1"),
    ]);
    before.pointers.insert(
        (uid, "source".into()),
        (
            Some("old".into()),
            Some("2.000000000000000000001".into()),
            3,
        ),
    );
    let current = make(&[
        ("a", "source", "2"),
        ("b", "source", "2"),
        ("above", "source", "2.000000000000000000002"),
        ("old", "dest", "2.000000000000000000001"),
        ("shared", "source", "1"),
        ("new-shared", "dest", "1"),
    ]);
    let moves = [
        EntryMove {
            from: "old".into(),
            to: "old".into(),
        },
        EntryMove {
            from: "shared".into(),
            to: "new-shared".into(),
        },
    ];
    let targets = entry_targets(&before, &current, &moves);
    assert_eq!(targets["old"], ["old"]);
    assert_eq!(targets["shared"], ["shared", "new-shared"]);
    let plan = PointerPlan::Split {
        source: "source",
        dest: "dest",
        dest_is_new: true,
    };
    assert_eq!(
        planned_pointer(uid, "source", &plan, &before, &current, &targets),
        Some("b".into())
    );
    assert_eq!(
        planned_pointer(uid, "dest", &plan, &before, &current, &targets),
        Some("old".into())
    );
    assert_eq!(
        planned_pointer(
            crate::db_types::uuid(BOB).unwrap(),
            "dest",
            &plan,
            &before,
            &current,
            &targets
        ),
        None
    );
    before.pointers.insert(
        (uid, "dest".into()),
        (Some("new-shared".into()), Some("1".into()), 7),
    );
    let existing = PointerPlan::Split {
        source: "source",
        dest: "dest",
        dest_is_new: false,
    };
    assert_eq!(
        planned_pointer(uid, "dest", &existing, &before, &current, &targets),
        Some("new-shared".into())
    );

    let merged = make(&[("keep", "dest", "3")]);
    let maps = [EntryMove {
        from: "old".into(),
        to: "keep".into(),
    }];
    let targets = entry_targets(&before, &merged, &maps);
    let plan = PointerPlan::Merge {
        survivor: "dest",
        loser: "source",
    };
    assert_eq!(
        planned_pointer(uid, "dest", &plan, &before, &merged, &targets),
        Some("keep".into())
    );
    let plan = PointerPlan::Edit { work: "source" };
    let edited = make(&[("keep", "source", "3")]);
    let targets = entry_targets(&before, &edited, &maps);
    assert_eq!(
        planned_pointer(uid, "source", &plan, &before, &edited, &targets),
        Some("keep".into())
    );
}

#[tokio::test]
async fn affected_users_unions_all_state_stores_without_requiring_mirrored_progress() {
    let pool = pool().await;
    let mark_user = "00000000-0000-4000-8000-000000000004";
    let pointer_user = "00000000-0000-4000-8000-000000000005";
    users(&pool, &[ALICE, BOB, CAROL, mark_user, pointer_user]).await;
    let w = work(&pool, "Affected readers").await;
    source(&pool, &w.id, "16").await;
    let (_, entry) = copy(&pool, &w.md, "affected-copy", "1", "One").await;
    md_read(&pool, ALICE, &w.id, "not-mirrored-md").await;
    sw_read(&pool, BOB, "16", "1600").await;
    sqlx::query("INSERT INTO progress_untick (user_id, chapter_id, created_at) VALUES ($1, 'affected-copy', now())")
        .bind(crate::db_types::uuid(CAROL).unwrap()).execute(&pool).await.unwrap();
    mark(&pool, mark_user, &entry, false).await;
    pointer(&pool, pointer_user, &w.id, &entry, 2).await;
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        affected_users(&mut tx, &[w.id.clone(), w.id.clone()])
            .await
            .unwrap(),
        [ALICE, BOB, CAROL, mark_user, pointer_user]
    );
    assert!(affected_users(&mut tx, &[]).await.unwrap().is_empty());
    let readers = lock_identity(&mut tx, &[w.id.clone()]).await;
    let before = keep_state_before(&mut tx, &readers, &[w.id.clone()])
        .await
        .unwrap();
    assert!(before
        .had_state
        .contains(&(crate::db_types::uuid(ALICE).unwrap(), w.id.clone())));
    assert!(before
        .had_state
        .contains(&(crate::db_types::uuid(BOB).unwrap(), w.id.clone())));
    keep_state_after(&mut tx, before, &[], &[], PointerPlan::Edit { work: &w.id })
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        work_read_state(&pool, Some(ALICE), &w.id).await.viewer(),
        Some((1, None))
    );
    assert_eq!(
        work_read_state(&pool, Some(BOB), &w.id).await.viewer(),
        Some((1, None))
    );
    // A legacy untick by itself has no entry state to compensate yet.
    assert_eq!(
        work_read_state(&pool, Some(CAROL), &w.id).await.viewer(),
        Some((0, None))
    );
}
