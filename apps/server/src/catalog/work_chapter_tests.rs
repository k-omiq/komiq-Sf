//! W3a: the work's chapter list, against real PostgreSQL.
use super::*;

use crate::catalog::phase4_tests::pool;
use crate::catalog::{self, ChapterInput, WorkInput};
use crate::phase_f::{self, Apply};

pub(crate) struct MdWork {
    pub(crate) work: String,
    pub(crate) source: String,
    pub(crate) uuid: String,
}

pub(crate) async fn md_work(pool: &Database, title: &str) -> MdWork {
    let uuid = uuid::Uuid::new_v4().to_string();
    let work = catalog::upsert_work_from_mangadex(
        pool,
        &uuid,
        &WorkInput {
            primary_title: Some(title.into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let (source, owner) = catalog::find_source_series_by_key(pool, "mangadex", &uuid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner, work);
    MdWork { work, source, uuid }
}

pub(crate) async fn source(pool: &Database, work: &str, key: &str) -> String {
    catalog::upsert_source_series(pool, work, "suwayomi", "source", key, None, false)
        .await
        .unwrap()
}

pub(crate) fn chapter(id: &str, number: Option<&str>, title: &str) -> ChapterInput {
    ChapterInput {
        external_id: id.into(),
        number: number.map(str::to_owned),
        title: Some(title.into()),
        lang: Some("en".into()),
        ..Default::default()
    }
}

pub(crate) async fn entry(pool: &Database, work: &str, key: &str) -> AdminEntry {
    admin_entry_list(pool, work)
        .await
        .unwrap()
        .1
        .into_iter()
        .find(|e| e.match_key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("missing entry {key} on {work}"))
}

pub(crate) async fn linked_entry(pool: &Database, source: &str, external: &str) -> String {
    sqlx::query_scalar(
        "SELECT l.work_chapter_id FROM chapter c \
         JOIN work_chapter_link l ON l.chapter_id = c.id \
         WHERE c.source_series_id = $1 AND c.external_id = $2",
    )
    .bind(source)
    .bind(external)
    .fetch_one(pool.read())
    .await
    .unwrap()
}

// Include every stored field as well as xmin: equality proves no UPDATE, replacement,
// insertion or deletion in any of the three catalogue-side tables.
type Snapshot = Vec<(String, String, String, serde_json::Value)>;

async fn snapshot(pool: &Database, work: &str) -> Snapshot {
    sqlx::query_as(
        "SELECT 'entry'::text AS kind, wc.id, wc.xmin::text AS version, to_jsonb(wc) AS data \
           FROM work_chapter wc WHERE wc.work_id = $1 \
         UNION ALL \
         SELECT 'link', l.chapter_id, l.xmin::text, to_jsonb(l) \
           FROM work_chapter_link l JOIN work_chapter wc ON wc.id = l.work_chapter_id \
          WHERE wc.work_id = $1 \
         UNION ALL \
         SELECT 'state', s.work_id, s.xmin::text, to_jsonb(s) \
           FROM work_chapter_state s WHERE s.work_id = $1 \
         ORDER BY kind, id",
    )
    .bind(work)
    .fetch_all(pool.read())
    .await
    .unwrap()
}

pub(crate) async fn make_ready(pool: &Database, work: &str) {
    for _ in 0..10 {
        if drain_work_attempt(pool, work).await.unwrap().1 {
            assert!(work_entries(pool, work).await.unwrap().ready);
            return;
        }
    }
    panic!("fixture work did not finish draining");
}

async fn edit(pool: &Database, work: &str, id: &str, change: EntryEdit) {
    let mut tx = pool.begin().await.unwrap();
    crate::locks::lock_works(&mut tx, &[work]).await.unwrap();
    assert!(edit_entry_on(&mut tx, work, id, &change).await.unwrap());
    tx.commit().await.unwrap();
}

async fn override_row(pool: &Database, work: &str, key: &str, hidden: bool, title: Option<&str>) {
    let mut tx = pool.begin().await.unwrap();
    crate::locks::lock_works(&mut tx, &[work]).await.unwrap();
    write_override_row_on(&mut tx, work, key, hidden, title)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

fn assert_same_attributes(before: &AdminEntry, after: &AdminEntry) {
    assert_eq!(after.match_key, before.match_key);
    assert_eq!(after.sort_key_text, before.sort_key_text);
    assert_eq!(after.number, before.number);
    assert_eq!(after.label, before.label);
    assert_eq!(after.special, before.special);
    assert_eq!(after.special_by, before.special_by);
    assert_eq!(after.hidden, before.hidden);
    assert_eq!(after.title_override, before.title_override);
}

pub(crate) async fn all_mangadex_source(
    pool: &Database,
    work: &str,
    key: &str,
    uuid: &str,
) -> String {
    catalog::upsert_source_extension(
        pool,
        phase_f::PRIMARY_SOURCE_ID,
        &catalog::SourceExtensionInput {
            pkg_name: phase_f::ALL_MANGADEX_PKG.into(),
            repo_url: "http://localhost/extensions".into(),
            apk_name: None,
            version_code: None,
            lang: Some("en".into()),
            is_nsfw: false,
        },
    )
    .await
    .unwrap();
    let ss = catalog::upsert_source_series(
        pool,
        work,
        "suwayomi",
        phase_f::PRIMARY_SOURCE_ID,
        key,
        None,
        false,
    )
    .await
    .unwrap();
    let report = phase_f::record_uuids(
        pool,
        &HashMap::from([(key.into(), format!("/manga/{uuid}"))]),
        Apply::Yes,
    )
    .await
    .unwrap();
    assert_eq!(report.resolved, 1);
    assert!(report.unresolved.is_empty());
    ss
}

#[tokio::test]
async fn link_once_survives_renumber_retitle_and_language_refresh() {
    let pool = pool().await;
    let md = md_work(&pool, "Link once").await;
    let mut copy = chapter("md-ten", Some("10"), "Original");
    catalog::upsert_chapter(&pool, &md.source, &copy)
        .await
        .unwrap();
    let original = entry(&pool, &md.work, "1000").await;
    assert_eq!(
        linked_entry(&pool, &md.source, &copy.external_id).await,
        original.id
    );
    assert_eq!(original.sort_key, 10.0);
    let before = snapshot(&pool, &md.work).await;

    copy.number = Some("11".into());
    copy.title = Some("Extra, revised".into());
    copy.lang = Some("ja".into());
    catalog::replace_source_chapters(&pool, &md.source, &[copy.clone()])
        .await
        .unwrap();
    catalog::upsert_chapter(&pool, &md.source, &copy)
        .await
        .unwrap();

    assert_eq!(snapshot(&pool, &md.work).await, before);
    let stored: (String, String, String, String) = sqlx::query_as(
        "SELECT number, title, lang, chapter_key FROM chapter \
         WHERE source_series_id = $1 AND external_id = $2",
    )
    .bind(&md.source)
    .bind(&copy.external_id)
    .fetch_one(pool.read())
    .await
    .unwrap();
    assert_eq!(
        stored,
        (
            "11".into(),
            "Extra, revised".into(),
            "ja".into(),
            "1100".into()
        )
    );
    let overlays = work_entries(&pool, &md.work).await.unwrap();
    let overlay = overlays.overlay("mangadex", &copy.external_id, "1100");
    assert_eq!(overlay.entry_id.as_deref(), Some(original.id.as_str()));
    assert_eq!(overlay.entry_order, Some(1));
    assert!(!overlay.special);
    assert!(!overlays.ready);
    assert_eq!(admin_entry_list(&pool, &md.work).await.unwrap().1.len(), 1);
}

#[tokio::test]
async fn chapter_zero_unnumbered_copies_and_numbered_extra_have_distinct_entries() {
    let pool = pool().await;
    let md = md_work(&pool, "Zero and specials").await;
    catalog::replace_source_chapters(
        &pool,
        &md.source,
        &[
            chapter("zero", Some("0"), "Prologue"),
            chapter("oneshot-a", None, "Oneshot"),
            chapter("oneshot-b", None, "Oneshot"),
            chapter("extra", Some("3"), "Extra"),
        ],
    )
    .await
    .unwrap();
    let zero = entry(&pool, &md.work, "0").await;
    assert_eq!(zero.number.as_deref(), Some("0"));
    assert_eq!(zero.sort_key, 0.0);
    assert!(!zero.special);
    assert_eq!(zero.special_by, None);
    let a = entry(&pool, &md.work, "x:oneshot-a").await;
    let b = entry(&pool, &md.work, "x:oneshot-b").await;
    assert_ne!(a.id, b.id);
    for special in [&a, &b] {
        assert_ne!(special.id, zero.id);
        assert!(special.special);
        assert_eq!(special.special_by.as_deref(), Some("matcher"));
        assert_eq!(special.number, None);
        assert_eq!(special.copies.len(), 1);
    }
    let extra = entry(&pool, &md.work, "300").await;
    assert!(extra.special);
    assert_eq!(extra.special_by.as_deref(), Some("matcher"));
    assert_eq!(extra.sort_key, 3.0);
    assert_eq!(extra.number.as_deref(), Some("3"));
    assert_eq!(admin_entry_list(&pool, &md.work).await.unwrap().1.len(), 4);
}

#[tokio::test]
async fn specials_follow_release_order_between_numbered_entries_in_one_scan() {
    // EXPECTS: numbered links must be visible to PLACE_SQL within the same scan.
    // Currently link_copies_on inserts those links only AFTER it places specials.
    let pool = pool().await;
    let md = md_work(&pool, "Release order").await;
    let mut copies = vec![
        chapter("one", Some("1"), "One"),
        chapter("two", Some("2"), "Two"),
        chapter("a", None, "Appendix A"),
        chapter("b", None, "Appendix B"),
        chapter("c", None, "Undated appendix"),
        chapter("before", None, "Opening special"),
    ];
    for (index, release) in [
        (0, "2026-01-01T00:00:00Z"),
        (1, "2026-01-03T00:00:00Z"),
        (2, "2026-01-02T00:00:00Z"),
        (3, "2026-01-02T12:00:00Z"),
        (5, "2025-12-31T00:00:00Z"),
    ] {
        // A misleading publish date also proves readable_at is preferred.
        copies[index].published_at = Some("2037-01-01T00:00:00Z".into());
        copies[index].readable_at = Some(release.into());
    }
    // Input order must not decide placement.
    copies.reverse();
    catalog::replace_source_chapters(&pool, &md.source, &copies)
        .await
        .unwrap();
    let one = entry(&pool, &md.work, "100").await;
    let two = entry(&pool, &md.work, "200").await;
    let a = entry(&pool, &md.work, "x:a").await;
    let b = entry(&pool, &md.work, "x:b").await;
    let c = entry(&pool, &md.work, "x:c").await;
    let before = entry(&pool, &md.work, "x:before").await;
    assert_eq!((one.sort_key, two.sort_key), (1.0, 2.0));
    assert!(
        1.0 < a.sort_key && a.sort_key < 1.01,
        "A: {}",
        a.sort_key_text
    );
    assert!(
        a.sort_key < b.sort_key && b.sort_key < 1.01,
        "B: {}",
        b.sort_key_text
    );
    assert!(c.sort_key > two.sort_key);
    assert!(before.sort_key < one.sort_key);
    let ordered: Vec<String> = admin_entry_list(&pool, &md.work)
        .await
        .unwrap()
        .1
        .into_iter()
        .map(|e| e.match_key.unwrap())
        .collect();
    assert_eq!(ordered, ["x:before", "100", "x:a", "x:b", "200", "x:c"]);
}

#[tokio::test]
async fn two_sources_share_one_numbered_entry_and_enforce_unique_work_key() {
    let pool = pool().await;
    let md = md_work(&pool, "Two sources").await;
    let sw = source(&pool, &md.work, "7").await;
    catalog::upsert_chapter(&pool, &md.source, &chapter("md-one", Some("1"), "One"))
        .await
        .unwrap();
    catalog::replace_source_chapters(&pool, &sw, &[chapter("8", Some("1"), "Other copy")])
        .await
        .unwrap();
    let one = entry(&pool, &md.work, "100").await;
    assert_eq!(one.copies.len(), 2);
    assert_eq!(linked_entry(&pool, &md.source, "md-one").await, one.id);
    assert_eq!(linked_entry(&pool, &sw, "8").await, one.id);
    assert_eq!(admin_entry_list(&pool, &md.work).await.unwrap().1.len(), 1);
    // Exercise the database constraint itself; no writer can intentionally insert
    // a duplicate, since the matcher uses ON CONFLICT DO NOTHING.
    let mut tx = pool.begin().await.unwrap();
    let error = sqlx::query(
        "INSERT INTO work_chapter (id, work_id, match_key, sort_key, created_at) \
         VALUES ('wc_duplicate', $1, '100', 1, now())",
    )
    .bind(&md.work)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("23505")
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn vanished_and_returning_copies_reuse_their_copyless_entries() {
    let pool = pool().await;
    let md = md_work(&pool, "Returning copies").await;
    let numbered = chapter("old-numbered-id", Some("10"), "Ten");
    let special = chapter("stable-special-id", None, "Oneshot");
    catalog::replace_source_chapters(&pool, &md.source, &[numbered, special.clone()])
        .await
        .unwrap();
    let ten = entry(&pool, &md.work, "1000").await;
    let x = entry(&pool, &md.work, "x:stable-special-id").await;
    let old_chapters: Vec<String> =
        sqlx::query_scalar("SELECT id FROM chapter WHERE source_series_id = $1 ORDER BY id")
            .bind(&md.source)
            .fetch_all(pool.read())
            .await
            .unwrap();
    catalog::replace_source_chapters(&pool, &md.source, &[])
        .await
        .unwrap();
    let empty = admin_entry_list(&pool, &md.work).await.unwrap().1;
    assert_eq!(empty.len(), 2);
    assert!(empty.iter().all(|e| e.copies.is_empty()));
    assert_eq!(entry(&pool, &md.work, "1000").await.id, ten.id);
    assert_eq!(entry(&pool, &md.work, "x:stable-special-id").await.id, x.id);
    assert!(snapshot(&pool, &md.work)
        .await
        .iter()
        .all(|r| r.0 != "link"));
    catalog::replace_source_chapters(
        &pool,
        &md.source,
        &[
            chapter("new-numbered-id", Some("10"), "Returning ten"),
            special,
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        linked_entry(&pool, &md.source, "new-numbered-id").await,
        ten.id
    );
    assert_eq!(
        linked_entry(&pool, &md.source, "stable-special-id").await,
        x.id
    );
    let new_chapters: Vec<String> =
        sqlx::query_scalar("SELECT id FROM chapter WHERE source_series_id = $1 ORDER BY id")
            .bind(&md.source)
            .fetch_all(pool.read())
            .await
            .unwrap();
    assert_eq!(new_chapters.len(), 2);
    assert!(new_chapters.iter().all(|id| !old_chapters.contains(id)));
    assert_same_attributes(&ten, &entry(&pool, &md.work, "1000").await);
    assert_same_attributes(&x, &entry(&pool, &md.work, "x:stable-special-id").await);
}

#[tokio::test]
async fn unchanged_replacement_and_upsert_preserve_all_catalogue_row_versions() {
    let pool = pool().await;
    let md = md_work(&pool, "No write scan").await;
    let copies = [
        chapter("ten", Some("10"), "Ten"),
        chapter("extra", None, "Extra"),
    ];
    catalog::replace_source_chapters(&pool, &md.source, &copies)
        .await
        .unwrap();
    make_ready(&pool, &md.work).await;
    let before = snapshot(&pool, &md.work).await;
    for (kind, count) in [("entry", 2), ("link", 2), ("state", 1)] {
        assert_eq!(before.iter().filter(|row| row.0 == kind).count(), count);
    }
    catalog::replace_source_chapters(&pool, &md.source, &copies)
        .await
        .unwrap();
    assert_eq!(snapshot(&pool, &md.work).await, before);
    for copy in &copies {
        catalog::upsert_chapter(&pool, &md.source, copy)
            .await
            .unwrap();
        assert_eq!(snapshot(&pool, &md.work).await, before);
    }
}

#[tokio::test]
async fn drain_resumes_an_incomplete_batch_marks_only_complete_works_and_is_idempotent() {
    let pool = pool().await;
    let large = md_work(&pool, "Large legacy work").await;
    let small = md_work(&pool, "Small legacy work").await;
    let empty = catalog::create_work(&pool, &WorkInput::default())
        .await
        .unwrap();
    let copies: Vec<_> = (0..=COPY_BATCH)
        .map(|i| chapter(&format!("legacy-{i}"), Some("1"), "One"))
        .collect();
    catalog::replace_source_chapters(&pool, &large.source, &copies)
        .await
        .unwrap();
    catalog::upsert_chapter(&pool, &small.source, &chapter("small", None, "Oneshot"))
        .await
        .unwrap();
    // Simulate the pre-W3a database, retaining only rows made through real writers.
    sqlx::raw_sql(
        "DELETE FROM work_chapter_link; DELETE FROM work_chapter; DELETE FROM work_chapter_state",
    )
    .execute(pool.write())
    .await
    .unwrap();
    assert_eq!(remaining(&pool).await.unwrap(), (3, COPY_BATCH + 2));

    let (linked, ready) = drain_work_attempt(&pool, &large.work).await.unwrap();
    assert_eq!(linked, COPY_BATCH as u64);
    assert!(!ready);
    let partial = snapshot(&pool, &large.work).await;
    assert_eq!(
        partial.iter().filter(|r| r.0 == "link").count(),
        COPY_BATCH as usize
    );
    assert!(!work_entries(&pool, &large.work).await.unwrap().ready);
    assert_eq!(remaining(&pool).await.unwrap(), (3, 2));

    // A restarted process starts with no cursor; the committed partial batch survives.
    let mut cursor = None;
    let mut finished = false;
    let mut total = DrainOutcome::default();
    for _ in 0..10 {
        let (pass, next) = drain_once(&pool, cursor.as_deref()).await.unwrap();
        total.works_visited += pass.works_visited;
        total.works_ready += pass.works_ready;
        total.copies_linked += pass.copies_linked;
        let prematurely_ready: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM work_chapter_state s \
             JOIN source_series ss ON ss.work_id = s.work_id \
             JOIN chapter c ON c.source_series_id = ss.id \
             WHERE s.ready AND NOT EXISTS(SELECT 1 FROM work_chapter_link l WHERE l.chapter_id = c.id))",
        ).fetch_one(pool.read()).await.unwrap();
        assert!(!prematurely_ready);
        let left = remaining(&pool).await.unwrap();
        if pass.works_visited == 0 || (next.is_none() && left == (0, 0)) {
            assert_eq!(left, (0, 0));
            finished = true;
            break;
        }
        cursor = next;
    }
    assert!(finished, "drain did not finish");
    assert_eq!(total.works_ready, 3);
    assert_eq!(total.copies_linked, 2);
    let after = snapshot(&pool, &large.work).await;
    for row in partial.iter().filter(|r| r.0 != "state") {
        assert!(
            after.contains(row),
            "resume rewrote an already-linked row: {row:?}"
        );
    }
    let mut before = Vec::new();
    for work in [&large.work, &small.work, &empty] {
        assert!(work_entries(&pool, work).await.unwrap().ready);
        before.push(snapshot(&pool, work).await);
    }
    let (second, cursor) = drain_once(&pool, None).await.unwrap();
    assert_eq!(second, DrainOutcome::default());
    assert_eq!(cursor, None);
    assert_eq!(remaining(&pool).await.unwrap(), (0, 0));
    for (work, before) in [&large.work, &small.work, &empty].into_iter().zip(before) {
        assert_eq!(snapshot(&pool, work).await, before);
    }
}

#[tokio::test]
async fn drain_defers_unkeyed_suwayomi_specials_until_engine_backfill_and_visits_other_works() {
    for state in ["missing", "pending", "quarantined"] {
        let pool = pool().await;
        let md = md_work(&pool, "Deferred special").await;
        let ss = source(&pool, &md.work, "7").await;
        catalog::replace_source_chapters(
            &pool,
            &ss,
            &[
                chapter("8", Some("1"), "One"),
                chapter("9", None, "Appendix"),
            ],
        )
        .await
        .unwrap();
        let generation = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO engine_generation (id, first_seen, last_seen, state) VALUES ($1, now(), now(), $2)")
            .bind(generation).bind(if state == "quarantined" { state } else { "active" })
            .execute(pool.write()).await.unwrap();
        if state != "missing" {
            sqlx::query("INSERT INTO engine_manga_key (generation, manga_id, source_id, manga_key, updated_at) VALUES ($1, 7, 'source', '/manga/stable', now())")
                .bind(generation).execute(pool.write()).await.unwrap();
        }
        // Simulate legacy copies before either catalogue drain has reached them.
        sqlx::query("DELETE FROM work_chapter WHERE work_id = $1")
            .bind(&md.work)
            .execute(pool.write())
            .await
            .unwrap();
        sqlx::query("UPDATE chapter SET chapter_key = NULL WHERE source_series_id = $1 AND external_id = '8'")
            .bind(&ss).execute(pool.write()).await.unwrap();
        assert_eq!(
            drain_work_attempt(&pool, &md.work).await.unwrap(),
            (1, false)
        );
        assert!(!work_entries(&pool, &md.work).await.unwrap().ready);
        let unlinked: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT external_id FROM ({UNLINKED_COPY_SELECT}) c"
        ))
        .fetch_all(pool.read())
        .await
        .unwrap();
        assert_eq!(unlinked, ["9"]);

        let other = catalog::create_work(&pool, &WorkInput::default())
            .await
            .unwrap();
        let (pass, _) = tokio::time::timeout(Duration::from_secs(5), drain_once(&pool, None))
            .await
            .expect("a deferred work must not spin forever")
            .unwrap();
        assert_eq!(
            (pass.works_visited, pass.works_ready, pass.copies_linked),
            (2, 1, 0)
        );
        assert!(work_entries(&pool, &other).await.unwrap().ready);
        assert!(!work_entries(&pool, &md.work).await.unwrap().ready);

        sqlx::query("UPDATE engine_generation SET state = 'active' WHERE id = $1")
            .bind(generation)
            .execute(pool.write())
            .await
            .unwrap();
        sqlx::query("INSERT INTO engine_manga_key (generation, manga_id, source_id, manga_key, updated_at, backfilled_at) VALUES ($1, 7, 'source', '/manga/stable', now(), now()) ON CONFLICT (generation, manga_id) DO UPDATE SET backfilled_at = now()")
            .bind(generation).execute(pool.write()).await.unwrap();
        sqlx::query("INSERT INTO engine_chapter_key (generation, chapter_id, manga_id, source_order, chapter_key, updated_at) VALUES ($1, 9, 7, 2, '/chapter/appendix', now())")
            .bind(generation).execute(pool.write()).await.unwrap();
        assert_eq!(
            drain_work_attempt(&pool, &md.work).await.unwrap(),
            (1, true)
        );
        let special = entry(&pool, &md.work, "x:source:/chapter/appendix").await;
        assert_eq!(linked_entry(&pool, &ss, "9").await, special.id);
        assert!(work_entries(&pool, &md.work).await.unwrap().ready);
        assert_eq!(remaining(&pool).await.unwrap(), (0, 0));
    }
}

#[tokio::test]
async fn drain_uses_numeric_fallback_only_after_manga_backfill() {
    let pool = pool().await;
    let md = md_work(&pool, "Backfilled without key").await;
    let ss = source(&pool, &md.work, "7").await;
    catalog::upsert_chapter(&pool, &ss, &chapter("9", None, "Appendix"))
        .await
        .unwrap();
    // Live writers still link immediately without engine keys.
    assert_eq!(
        linked_entry(&pool, &ss, "9").await,
        entry(&pool, &md.work, "x:source:#9").await.id
    );
    sqlx::query("DELETE FROM work_chapter WHERE work_id = $1")
        .bind(&md.work)
        .execute(pool.write())
        .await
        .unwrap();
    let generation = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO engine_generation (id, first_seen, last_seen, state) VALUES ($1, now(), now(), 'active')")
        .bind(generation).execute(pool.write()).await.unwrap();
    sqlx::query("INSERT INTO engine_manga_key (generation, manga_id, source_id, manga_key, updated_at, backfilled_at) VALUES ($1, 7, 'source', '/manga/stable', now(), now())")
        .bind(generation).execute(pool.write()).await.unwrap();
    assert_eq!(
        drain_work_attempt(&pool, &md.work).await.unwrap(),
        (1, true)
    );
    assert_eq!(
        linked_entry(&pool, &ss, "9").await,
        entry(&pool, &md.work, "x:source:#9").await.id
    );
    assert!(work_entries(&pool, &md.work).await.unwrap().ready);
}

#[tokio::test]
async fn drain_batch_prioritizes_numbered_copies_with_null_keys() {
    let pool = pool().await;
    let md = md_work(&pool, "Null legacy key").await;
    let ss = source(&pool, &md.work, "7").await;
    sqlx::query(
        "INSERT INTO chapter (id, source_series_id, external_id, number, title, chapter_key, created_at) \
         SELECT 'special-' || n, $1, n::text, NULL, 'Appendix', 'x:' || n, now() \
           FROM generate_series(1, $2::int) n",
    ).bind(&ss).bind(COPY_BATCH as i32).execute(pool.write()).await.unwrap();
    sqlx::query("INSERT INTO chapter (id, source_series_id, external_id, number, title, created_at) VALUES ('numbered', $1, '9999', '1', 'One', now())")
        .bind(&ss).execute(pool.write()).await.unwrap();
    assert_eq!(
        drain_work_attempt(&pool, &md.work).await.unwrap(),
        (1, false)
    );
    assert_eq!(
        linked_entry(&pool, &ss, "9999").await,
        entry(&pool, &md.work, "100").await.id
    );
    assert!(!work_entries(&pool, &md.work).await.unwrap().ready);
}

#[tokio::test]
async fn drain_seeds_new_special_entries_from_the_copys_legacy_override_key() {
    let pool = pool().await;
    let md = md_work(&pool, "Legacy special overrides").await;
    let sw = source(&pool, &md.work, "7").await;
    catalog::upsert_chapter(&pool, &md.source, &chapter("abc", None, "Appendix"))
        .await
        .unwrap();
    catalog::upsert_chapter(&pool, &sw, &chapter("9", None, "Other appendix"))
        .await
        .unwrap();
    sqlx::query("DELETE FROM work_chapter WHERE work_id = $1")
        .bind(&md.work)
        .execute(pool.write())
        .await
        .unwrap();
    let generation = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO engine_generation (id, first_seen, last_seen, state) VALUES ($1, now(), now(), 'active')")
        .bind(generation).execute(pool.write()).await.unwrap();
    sqlx::query("INSERT INTO engine_manga_key (generation, manga_id, source_id, manga_key, updated_at, backfilled_at) VALUES ($1, 7, 'source', '/manga/stable', now(), now())")
        .bind(generation).execute(pool.write()).await.unwrap();
    sqlx::query("INSERT INTO engine_chapter_key (generation, chapter_id, manga_id, source_order, chapter_key, updated_at) VALUES ($1, 9, 7, 1, '/chapter/appendix', now())")
        .bind(generation).execute(pool.write()).await.unwrap();
    override_row(&pool, &md.work, "x:abc", true, Some("Legacy appendix")).await;
    override_row(&pool, &md.work, "x:9", true, Some("Legacy source appendix")).await;

    assert_eq!(
        drain_work_attempt(&pool, &md.work).await.unwrap(),
        (2, true)
    );
    for (key, source, copy, title) in [
        ("x:abc", &md.source, "abc", "Legacy appendix"),
        (
            "x:source:/chapter/appendix",
            &sw,
            "9",
            "Legacy source appendix",
        ),
    ] {
        let special = entry(&pool, &md.work, key).await;
        assert!(special.hidden);
        assert_eq!(special.title_override.as_deref(), Some(title));
        assert_eq!(linked_entry(&pool, source, copy).await, special.id);
    }
}

#[tokio::test]
async fn merge_maps_collisions_moves_unique_entries_and_combines_overrides_and_readiness() {
    for (loser_ready, survivor_ready) in
        [(true, true), (true, false), (false, true), (false, false)]
    {
        let pool = pool().await;
        let loser = md_work(&pool, "Loser").await;
        let survivor = md_work(&pool, "Survivor").await;
        catalog::replace_source_chapters(
            &pool,
            &survivor.source,
            &[
                chapter("survivor-one", Some("1"), "One"),
                chapter("survivor-two", Some("2"), "Two"),
            ],
        )
        .await
        .unwrap();
        catalog::replace_source_chapters(
            &pool,
            &loser.source,
            &[
                chapter("loser-one", Some("1"), "One"),
                chapter("loser-two", Some("2"), "Two"),
                chapter("loser-three", Some("3"), "Extra"),
            ],
        )
        .await
        .unwrap();
        let s1 = entry(&pool, &survivor.work, "100").await;
        let s2 = entry(&pool, &survivor.work, "200").await;
        let l1 = entry(&pool, &loser.work, "100").await;
        let l2 = entry(&pool, &loser.work, "200").await;
        let l3 = entry(&pool, &loser.work, "300").await;
        edit(
            &pool,
            &survivor.work,
            &s1.id,
            EntryEdit {
                hidden: Some(true),
                title_override: Some(Some("Survivor title".into())),
                ..Default::default()
            },
        )
        .await;
        for (id, hidden) in [(&l1.id, false), (&l2.id, true)] {
            edit(
                &pool,
                &loser.work,
                id,
                EntryEdit {
                    hidden: Some(hidden),
                    title_override: Some(Some("Loser title".into())),
                    ..Default::default()
                },
            )
            .await;
        }
        if loser_ready {
            make_ready(&pool, &loser.work).await;
        }
        if survivor_ready {
            make_ready(&pool, &survivor.work).await;
        }

        let outcome = catalog::merge_works(&pool, &loser.work, &survivor.work)
            .await
            .unwrap();
        assert_eq!(outcome.moved_source_series, 1);
        let mut expected = vec![
            EntryMove {
                from: l1.id.clone(),
                to: s1.id.clone(),
            },
            EntryMove {
                from: l2.id.clone(),
                to: s2.id.clone(),
            },
            EntryMove {
                from: l3.id.clone(),
                to: l3.id.clone(),
            },
        ];
        expected.sort_by(|a, b| a.from.cmp(&b.from));
        assert_eq!(outcome.entry_map, expected);
        let removed: i64 =
            sqlx::query_scalar("SELECT count(*) FROM work_chapter WHERE id = ANY($1)")
                .bind(vec![&l1.id, &l2.id])
                .fetch_one(pool.read())
                .await
                .unwrap();
        assert_eq!(removed, 0);
        assert_eq!(linked_entry(&pool, &loser.source, "loser-one").await, s1.id);
        assert_eq!(linked_entry(&pool, &loser.source, "loser-two").await, s2.id);
        assert_eq!(
            linked_entry(&pool, &loser.source, "loser-three").await,
            l3.id
        );
        let one = entry(&pool, &survivor.work, "100").await;
        assert!(one.hidden);
        assert_eq!(one.title_override.as_deref(), Some("Survivor title"));
        let two = entry(&pool, &survivor.work, "200").await;
        assert!(two.hidden);
        assert_eq!(two.title_override.as_deref(), Some("Loser title"));
        let three = entry(&pool, &survivor.work, "300").await;
        assert_eq!(three.id, l3.id);
        assert_same_attributes(&l3, &three);
        let (ready, entries) = admin_entry_list(&pool, &survivor.work).await.unwrap();
        assert_eq!(ready, loser_ready && survivor_ready);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries.iter().map(|e| e.copies.len()).sum::<usize>(), 5);
        let owner: String = sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1")
            .bind(&loser.source)
            .fetch_one(pool.read())
            .await
            .unwrap();
        assert_eq!(owner, survivor.work);
        let loser_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM work WHERE id = $1)")
                .bind(&loser.work)
                .fetch_one(pool.read())
                .await
                .unwrap();
        assert!(!loser_exists);
    }
}

#[tokio::test]
async fn merge_and_phase_f_split_apply_legacy_overrides_to_existing_destination_entries() {
    for merge in [false, true] {
        for destination_edit in [false, true] {
            let pool = pool().await;
            let from = md_work(&pool, "Legacy source").await;
            let to = md_work(&pool, "Existing destination").await;
            let moving = if merge {
                from.source.clone()
            } else {
                all_mangadex_source(&pool, &from.work, "101", &to.uuid).await
            };
            catalog::upsert_chapter(&pool, &to.source, &chapter("dest-one", Some("1"), "One"))
                .await
                .unwrap();
            let before = entry(&pool, &to.work, "100").await;
            if destination_edit {
                edit(
                    &pool,
                    &to.work,
                    &before.id,
                    EntryEdit {
                        title_override: Some(Some("Destination wins".into())),
                        ..Default::default()
                    },
                )
                .await;
            }
            // A pre-drain copy has no entry to carry its legacy override across.
            sqlx::query(
                "INSERT INTO chapter (id, source_series_id, external_id, number, chapter_key, created_at) \
                 VALUES ('legacy-one', $1, '1011', '1', '100', now())",
            )
            .bind(&moving)
            .execute(pool.write())
            .await
            .unwrap();
            override_row(&pool, &from.work, "100", true, Some("Legacy title")).await;

            if merge {
                catalog::merge_works(&pool, &from.work, &to.work)
                    .await
                    .unwrap();
            } else {
                let report = phase_f::split_mismatched(&pool, Apply::Yes).await.unwrap();
                assert_eq!(report.acted, 1);
                assert!(report.blocked.is_empty());
            }
            let after = entry(&pool, &to.work, "100").await;
            assert_eq!(after.id, before.id);
            assert_eq!(after.hidden, !destination_edit);
            assert_eq!(
                after.title_override.as_deref(),
                Some(if destination_edit {
                    "Destination wins"
                } else {
                    "Legacy title"
                })
            );
            let row: (bool, Option<String>) = sqlx::query_as(
                "SELECT hidden, title_override FROM chapter_override WHERE work_id = $1 AND chapter_key = '100'",
            ).bind(&to.work).fetch_one(pool.read()).await.unwrap();
            assert_eq!(row, (after.hidden, after.title_override));
        }
    }
}

#[tokio::test]
async fn admin_split_moves_exclusive_entries_clones_shared_entries_and_reports_the_map() {
    for old_ready in [false, true] {
        let pool = pool().await;
        let md = md_work(&pool, "Admin split").await;
        let moving = source(&pool, &md.work, "7").await;
        catalog::upsert_chapter(&pool, &md.source, &chapter("staying", Some("1"), "One"))
            .await
            .unwrap();
        catalog::replace_source_chapters(
            &pool,
            &moving,
            &[
                chapter("8", Some("1"), "One"),
                chapter("9", Some("2"), "Extra"),
            ],
        )
        .await
        .unwrap();
        let shared_id = entry(&pool, &md.work, "100").await.id;
        edit(
            &pool,
            &md.work,
            &shared_id,
            EntryEdit {
                hidden: Some(true),
                title_override: Some(Some("Shared override".into())),
                special: Some(true),
                sort_key: Some("1.005".into()),
            },
        )
        .await;
        let shared = entry(&pool, &md.work, "100").await;
        let exclusive = entry(&pool, &md.work, "200").await;
        if old_ready {
            make_ready(&pool, &md.work).await;
        }

        let out = catalog::split_source_series(&pool, &md.work, &[moving.clone()], None)
            .await
            .unwrap();
        assert_eq!(out.moved_sources, 1);
        let stayed = entry(&pool, &md.work, "100").await;
        let cloned = entry(&pool, &out.new_work_id, "100").await;
        let moved = entry(&pool, &out.new_work_id, "200").await;
        assert_eq!(stayed.id, shared.id);
        assert_ne!(cloned.id, shared.id);
        assert_eq!(moved.id, exclusive.id);
        assert_same_attributes(&shared, &stayed);
        assert_same_attributes(&shared, &cloned);
        assert_same_attributes(&exclusive, &moved);
        assert_eq!(linked_entry(&pool, &md.source, "staying").await, shared.id);
        assert_eq!(linked_entry(&pool, &moving, "8").await, cloned.id);
        assert_eq!(linked_entry(&pool, &moving, "9").await, exclusive.id);
        let mut expected = vec![
            EntryMove {
                from: shared.id,
                to: cloned.id,
            },
            EntryMove {
                from: exclusive.id.clone(),
                to: exclusive.id,
            },
        ];
        expected.sort_by(|a, b| a.from.cmp(&b.from));
        assert_eq!(out.entry_map, expected);
        assert_eq!(admin_entry_list(&pool, &md.work).await.unwrap().1.len(), 1);
        assert_eq!(
            admin_entry_list(&pool, &out.new_work_id)
                .await
                .unwrap()
                .1
                .len(),
            2
        );
        assert_eq!(
            work_entries(&pool, &md.work).await.unwrap().ready,
            old_ready
        );
        assert!(!work_entries(&pool, &out.new_work_id).await.unwrap().ready);
        make_ready(&pool, &out.new_work_id).await;
        assert!(work_entries(&pool, &out.new_work_id).await.unwrap().ready);
        let owner: String = sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1")
            .bind(&moving)
            .fetch_one(pool.read())
            .await
            .unwrap();
        assert_eq!(owner, out.new_work_id);
    }
}

#[tokio::test]
async fn split_preserves_numbered_legacy_overrides_of_unlinked_moving_copies() {
    let pool = pool().await;
    let from = md_work(&pool, "Legacy split").await;
    let to = md_work(&pool, "Existing destination").await;
    let moving = source(&pool, &from.work, "7").await;
    catalog::replace_source_chapters(
        &pool,
        &moving,
        &[
            chapter("8", Some("1"), "One"),
            chapter("9", Some("2"), "Two"),
            chapter("10", None, "Appendix"),
        ],
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM work_chapter_link WHERE chapter_id IN (SELECT id FROM chapter WHERE source_series_id = $1)")
        .bind(&moving).execute(pool.write()).await.unwrap();
    override_row(&pool, &from.work, "100", true, Some("Source one")).await;
    override_row(&pool, &from.work, "200", true, Some("Source two")).await;
    override_row(&pool, &from.work, "300", true, Some("Not moving")).await;
    override_row(&pool, &from.work, "x:10", true, Some("Legacy appendix")).await;
    override_row(&pool, &to.work, "100", false, Some("Destination wins")).await;
    let mut tx = pool.begin().await.unwrap();
    crate::locks::lock_works(&mut tx, &[&from.work, &to.work])
        .await
        .unwrap();
    // The real split callers move the mappings before the entries.
    sqlx::query("UPDATE source_series SET work_id = $1 WHERE id = $2")
        .bind(&to.work)
        .bind(&moving)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        split_entries_on(&mut tx, &from.work, &to.work, &[moving.clone()], false)
            .await
            .unwrap()
            .is_empty()
    );
    tx.commit().await.unwrap();
    let rows: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT chapter_key, hidden, title_override FROM chapter_override WHERE work_id = $1 ORDER BY chapter_key",
    ).bind(&to.work).fetch_all(pool.read()).await.unwrap();
    assert_eq!(
        rows,
        [
            ("100".into(), false, Some("Destination wins".into())),
            ("200".into(), true, Some("Source two".into())),
        ]
    );
    let overlay = work_entries(&pool, &to.work)
        .await
        .unwrap()
        .overlay("suwayomi", "9", "200");
    assert_eq!(overlay.entry_id, None);
    assert!(overlay.hidden);
    assert_eq!(overlay.title_override.as_deref(), Some("Source two"));
}

#[tokio::test]
async fn phase_f_split_joins_destination_keys_moves_or_clones_other_entries_and_ands_readiness() {
    for (old_ready, destination_ready) in
        [(true, true), (true, false), (false, true), (false, false)]
    {
        let pool = pool().await;
        let old = md_work(&pool, "Base edition").await;
        let dest = md_work(&pool, "Colored edition").await;
        let moving = all_mangadex_source(&pool, &old.work, "101", &dest.uuid).await;
        catalog::replace_source_chapters(
            &pool,
            &old.source,
            &[
                chapter("base-one", Some("1"), "One"),
                chapter("base-three", Some("3"), "Three"),
            ],
        )
        .await
        .unwrap();
        catalog::upsert_chapter(&pool, &dest.source, &chapter("dest-one", Some("1"), "One"))
            .await
            .unwrap();
        catalog::replace_source_chapters(
            &pool,
            &moving,
            &[
                chapter("1011", Some("1"), "One"),
                chapter("1012", Some("2"), "Extra"),
                chapter("1013", Some("3"), "Three"),
            ],
        )
        .await
        .unwrap();
        let old_one = entry(&pool, &old.work, "100").await;
        let dest_one = entry(&pool, &dest.work, "100").await;
        let exclusive = entry(&pool, &old.work, "200").await;
        let shared_id = entry(&pool, &old.work, "300").await.id;
        edit(
            &pool,
            &old.work,
            &shared_id,
            EntryEdit {
                hidden: Some(true),
                title_override: Some(Some("Shared special".into())),
                special: Some(true),
                sort_key: Some("3.005".into()),
            },
        )
        .await;
        let shared = entry(&pool, &old.work, "300").await;
        if old_ready {
            make_ready(&pool, &old.work).await;
        }
        if destination_ready {
            make_ready(&pool, &dest.work).await;
        }
        let classified = phase_f::classify(&pool).await.unwrap();
        assert_eq!(classified.len(), 1);
        assert_eq!(classified[0].verdict(), phase_f::Verdict::Mismatch);

        let report = phase_f::split_mismatched(&pool, Apply::Yes).await.unwrap();
        assert_eq!(report.acted, 1);
        assert!(report.blocked.is_empty());
        assert_eq!(linked_entry(&pool, &moving, "1011").await, dest_one.id);
        assert_eq!(
            linked_entry(&pool, &old.source, "base-one").await,
            old_one.id
        );
        assert_eq!(entry(&pool, &dest.work, "100").await.copies.len(), 2);
        let moved = entry(&pool, &dest.work, "200").await;
        assert_eq!(moved.id, exclusive.id);
        assert_same_attributes(&exclusive, &moved);
        assert_eq!(linked_entry(&pool, &moving, "1012").await, exclusive.id);
        let cloned = entry(&pool, &dest.work, "300").await;
        assert_ne!(cloned.id, shared.id);
        assert_same_attributes(&shared, &cloned);
        assert_same_attributes(&shared, &entry(&pool, &old.work, "300").await);
        assert_eq!(linked_entry(&pool, &moving, "1013").await, cloned.id);
        assert_eq!(
            linked_entry(&pool, &old.source, "base-three").await,
            shared.id
        );
        assert_eq!(
            work_entries(&pool, &old.work).await.unwrap().ready,
            old_ready
        );
        assert_eq!(
            work_entries(&pool, &dest.work).await.unwrap().ready,
            old_ready && destination_ready
        );
        let owner: String = sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1")
            .bind(&moving)
            .fetch_one(pool.read())
            .await
            .unwrap();
        assert_eq!(owner, dest.work);
        assert!(phase_f::gate(&pool).await.unwrap().is_clean());
    }
}

#[tokio::test]
async fn phase_f_delete_requires_entry_parity_even_when_mirror_chapter_keys_match() {
    for relink_mirror in [false, true] {
        let pool = pool().await;
        let md = md_work(&pool, "Delete parity").await;
        let redundant = all_mangadex_source(&pool, &md.work, "100", &md.uuid).await;
        catalog::upsert_chapter(&pool, &md.source, &chapter("mirror", Some("1"), "One"))
            .await
            .unwrap();
        catalog::upsert_chapter(&pool, &redundant, &chapter("1001", Some("1"), "One"))
            .await
            .unwrap();
        let one = entry(&pool, &md.work, "100").await;
        assert_eq!(linked_entry(&pool, &redundant, "1001").await, one.id);
        assert_eq!(linked_entry(&pool, &md.source, "mirror").await, one.id);
        if relink_mirror {
            catalog::upsert_chapter(&pool, &md.source, &chapter("other-entry", Some("2"), "Two"))
                .await
                .unwrap();
            let other = entry(&pool, &md.work, "200").await;
            // An admin link can diverge from the source's number; no public link
            // editor exists yet, so simulate that link edit directly.
            let changed = sqlx::query(
                "UPDATE work_chapter_link l SET work_chapter_id = $1, linked_by = 'admin' \
                 FROM chapter c WHERE c.id = l.chapter_id \
                   AND c.source_series_id = $2 AND c.external_id = 'mirror'",
            )
            .bind(&other.id)
            .bind(&md.source)
            .execute(pool.write())
            .await
            .unwrap()
            .rows_affected();
            assert_eq!(changed, 1);
            assert_ne!(linked_entry(&pool, &md.source, "mirror").await, one.id);
        }
        let keys: Vec<String> = sqlx::query_scalar(
            "SELECT chapter_key FROM chapter WHERE external_id IN ('mirror', '1001') ORDER BY external_id",
        ).fetch_all(pool.read()).await.unwrap();
        assert_eq!(keys, ["100", "100"]);
        assert!(phase_f::gate(&pool).await.unwrap().is_clean());
        let before = snapshot(&pool, &md.work).await;
        let result = phase_f::delete_redundant(
            &pool,
            Apply::Yes,
            Some(phase_f::DELETE_CONFIRM_TOKEN),
            false,
        )
        .await;
        if relink_mirror {
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains("chapter parity failed"),
                "{error:#}"
            );
            assert_eq!(snapshot(&pool, &md.work).await, before);
            assert_eq!(linked_entry(&pool, &redundant, "1001").await, one.id);
        } else {
            let report = result.unwrap();
            assert_eq!(
                (report.deletable, report.deleted, report.cascading_chapters),
                (1, 1, 1)
            );
            assert_eq!(entry(&pool, &md.work, "100").await.id, one.id);
            assert_eq!(entry(&pool, &md.work, "100").await.copies.len(), 1);
            assert_eq!(linked_entry(&pool, &md.source, "mirror").await, one.id);
        }
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM source_series WHERE id = $1)")
                .bind(&redundant)
                .fetch_one(pool.read())
                .await
                .unwrap();
        assert_eq!(exists, relink_mirror);
    }
}

#[tokio::test]
async fn engine_reset_keeps_entries_and_refill_reuses_number_and_source_url_identity() {
    use crate::engine_generation::{
        tests::{chapter_json, manga_json, MockEngine},
        Fence, Replace,
    };
    use crate::suwayomi::SuwayomiClient;

    let pool = pool().await;
    // The catalogue is PostgreSQL; the fence's separate cover cache is SQLite,
    // exactly as in engine_generation_tests.rs. No cover fixtures are needed.
    let covers = crate::database::SqliteTestPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE suwayomi_cover_blob(manga_id INTEGER PRIMARY KEY,webp BLOB,updated_at TEXT); \
         CREATE TABLE cover_variant(asset_key TEXT,width INTEGER,webp BLOB)",
    ).execute(&covers).await.unwrap();
    let engine = MockEngine::spawn().await;
    let engine_rows = |manga_id, numbered_id, special_id| {
        let mut manga = manga_json(manga_id);
        manga["url"] = serde_json::json!("/manga/stable-series");
        let mut numbered = chapter_json(numbered_id, manga_id);
        numbered["url"] = serde_json::json!("/chapter/one");
        numbered["sourceOrder"] = serde_json::json!(1);
        numbered["manga"]["url"] = manga["url"].clone();
        let mut special = chapter_json(special_id, manga_id);
        special["url"] = serde_json::json!("/chapter/appendix");
        special["sourceOrder"] = serde_json::json!(2);
        special["name"] = serde_json::json!("Appendix");
        special["chapterNumber"] = serde_json::json!(-1.0);
        special["manga"]["url"] = manga["url"].clone();
        (manga, vec![numbered, special])
    };
    {
        let mut data = engine.data.lock().await;
        let (manga, chapters) = engine_rows(7, 8, 9);
        data.mangas = vec![manga];
        data.chapters = chapters;
    }
    let mut client = SuwayomiClient::new(engine.url.clone(), None, None);
    let fence = Fence::attach(&pool, &covers, &mut client).await.unwrap();
    let old_generation = fence.generation().unwrap();
    let work = catalog::create_work(
        &pool,
        &WorkInput {
            primary_title: Some("Engine reset".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let old_mapping = source(&pool, &work, "7").await;
    let (manga, chapters) = client.series_and_chapters(7).await.unwrap();
    assert_eq!(chapters.len(), 2);
    assert!(crate::series_cache::put_series(&pool, &manga)
        .await
        .unwrap());
    crate::series_cache::put_chapters(&pool, 7, &chapters)
        .await
        .unwrap();
    let numbered = entry(&pool, &work, "100").await;
    let special_key = "x:source:/chapter/appendix";
    let special = entry(&pool, &work, special_key).await;
    assert_eq!(linked_entry(&pool, &old_mapping, "8").await, numbered.id);
    assert_eq!(linked_entry(&pool, &old_mapping, "9").await, special.id);
    let keys: Vec<(i64, String)> = sqlx::query_as(
        "SELECT chapter_id, chapter_key FROM engine_chapter_key WHERE generation = $1 ORDER BY chapter_id",
    ).bind(old_generation).fetch_all(pool.read()).await.unwrap();
    assert_eq!(
        keys,
        [(8, "/chapter/one".into()), (9, "/chapter/appendix".into())]
    );
    let old_rows: Vec<String> =
        sqlx::query_scalar("SELECT id FROM chapter WHERE source_series_id = $1")
            .bind(&old_mapping)
            .fetch_all(pool.read())
            .await
            .unwrap();
    let entries_before: Vec<_> = snapshot(&pool, &work)
        .await
        .into_iter()
        .filter(|r| r.0 == "entry")
        .collect();
    {
        let mut data = engine.data.lock().await;
        let (manga, chapters) = engine_rows(70, 80, 90);
        data.mangas = vec![manga];
        data.chapters = chapters;
    }
    fence.start_new_generation(Replace::Force).await.unwrap();
    assert_ne!(fence.generation().unwrap(), old_generation);
    assert!(fence.wait().await.is_ok());
    let counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM source_series), (SELECT count(*) FROM chapter), \
                (SELECT count(*) FROM work_chapter), (SELECT count(*) FROM work_chapter_link)",
    )
    .fetch_one(pool.read())
    .await
    .unwrap();
    assert_eq!(counts, (0, 0, 2, 0));
    let entries_after: Vec<_> = snapshot(&pool, &work)
        .await
        .into_iter()
        .filter(|r| r.0 == "entry")
        .collect();
    assert_eq!(entries_after, entries_before);
    assert!(admin_entry_list(&pool, &work)
        .await
        .unwrap()
        .1
        .iter()
        .all(|e| e.copies.is_empty()));

    let new_mapping = source(&pool, &work, "70").await;
    assert_ne!(new_mapping, old_mapping);
    let (manga, chapters) = client.series_and_chapters(70).await.unwrap();
    assert!(crate::series_cache::put_series(&pool, &manga)
        .await
        .unwrap());
    crate::series_cache::put_chapters(&pool, 70, &chapters)
        .await
        .unwrap();
    assert_eq!(linked_entry(&pool, &new_mapping, "80").await, numbered.id);
    assert_eq!(linked_entry(&pool, &new_mapping, "90").await, special.id);
    assert_eq!(entry(&pool, &work, special_key).await.id, special.id);
    assert_eq!(admin_entry_list(&pool, &work).await.unwrap().1.len(), 2);
    let new_rows: Vec<String> =
        sqlx::query_scalar("SELECT id FROM chapter WHERE source_series_id = $1")
            .bind(&new_mapping)
            .fetch_all(pool.read())
            .await
            .unwrap();
    assert_eq!(new_rows.len(), 2);
    assert!(new_rows.iter().all(|id| !old_rows.contains(id)));
    let entries_refilled: Vec<_> = snapshot(&pool, &work)
        .await
        .into_iter()
        .filter(|r| r.0 == "entry")
        .collect();
    assert_eq!(entries_refilled, entries_before);
    let current_keys: Vec<i64> = sqlx::query_scalar(
        "SELECT chapter_id FROM engine_chapter_key WHERE generation = $1 ORDER BY chapter_id",
    )
    .bind(fence.generation().unwrap())
    .fetch_all(pool.read())
    .await
    .unwrap();
    assert_eq!(current_keys, [80, 90]);
}

#[tokio::test]
async fn entry_edits_dual_write_numbered_overrides_and_zero_sync_leaves_specials_alone() {
    let pool = pool().await;
    let md = md_work(&pool, "Entry edits").await;
    catalog::replace_source_chapters(
        &pool,
        &md.source,
        &[
            chapter("zero", Some("0"), "Prologue"),
            chapter("ten", Some("10"), "Ten"),
            chapter("special", None, "Appendix"),
        ],
    )
    .await
    .unwrap();
    make_ready(&pool, &md.work).await;
    let ten = entry(&pool, &md.work, "1000").await;
    let special = entry(&pool, &md.work, "x:special").await;
    edit(
        &pool,
        &md.work,
        &ten.id,
        EntryEdit {
            hidden: Some(true),
            title_override: Some(Some("  Edited ten  ".into())),
            special: Some(true),
            sort_key: Some("10.005".into()),
        },
    )
    .await;
    let edited = entry(&pool, &md.work, "1000").await;
    assert!(edited.hidden && edited.special);
    assert_eq!(edited.title_override.as_deref(), Some("Edited ten"));
    assert_eq!(edited.special_by.as_deref(), Some("admin"));
    assert_eq!(edited.sort_key, 10.005);
    let legacy: (bool, Option<String>) = sqlx::query_as(
        "SELECT hidden, title_override FROM chapter_override WHERE work_id = $1 AND chapter_key = '1000'",
    ).bind(&md.work).fetch_one(pool.read()).await.unwrap();
    assert_eq!(legacy, (true, Some("Edited ten".into())));
    let overlay = work_entries(&pool, &md.work)
        .await
        .unwrap()
        .overlay("mangadex", "ten", "1000");
    assert_eq!(overlay.entry_id.as_deref(), Some(ten.id.as_str()));
    let rank = admin_entry_list(&pool, &md.work)
        .await
        .unwrap()
        .1
        .iter()
        .position(|e| e.id == ten.id)
        .unwrap() as i64
        + 1;
    assert_eq!(overlay.entry_order, Some(rank));
    assert!(overlay.hidden && overlay.special);
    assert_eq!(overlay.title_override.as_deref(), Some("Edited ten"));
    edit(
        &pool,
        &md.work,
        &ten.id,
        EntryEdit {
            hidden: Some(false),
            title_override: Some(None),
            special: Some(false),
            sort_key: Some("10".into()),
        },
    )
    .await;
    let cleared = entry(&pool, &md.work, "1000").await;
    assert!(!cleared.hidden && !cleared.special);
    assert_eq!(cleared.title_override, None);
    assert_eq!(cleared.sort_key, 10.0);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM chapter_override WHERE work_id = $1")
        .bind(&md.work)
        .fetch_one(pool.read())
        .await
        .unwrap();
    assert_eq!(count, 0);

    edit(
        &pool,
        &md.work,
        &special.id,
        EntryEdit {
            hidden: Some(true),
            title_override: Some(Some("Private appendix".into())),
            special: Some(false),
            sort_key: Some("0.005".into()),
        },
    )
    .await;
    let x_before_sync = entry(&pool, &md.work, "x:special").await;
    assert!(x_before_sync.hidden);
    assert!(!x_before_sync.special);
    assert_eq!(x_before_sync.special_by.as_deref(), Some("admin"));
    assert_eq!(x_before_sync.sort_key, 0.005);
    assert_eq!(
        x_before_sync.title_override.as_deref(),
        Some("Private appendix")
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM chapter_override WHERE work_id = $1")
        .bind(&md.work)
        .fetch_one(pool.read())
        .await
        .unwrap();
    assert_eq!(count, 0, "an x: entry must not create a legacy override");
    let zero = entry(&pool, &md.work, "0").await;
    let unchanged_rows: Vec<_> = snapshot(&pool, &md.work)
        .await
        .into_iter()
        .filter(|row| row.0 == "entry" && row.1 != zero.id)
        .collect();
    let mut tx = pool.begin().await.unwrap();
    crate::locks::lock_works(&mut tx, &[&md.work])
        .await
        .unwrap();
    write_override_row_on(&mut tx, &md.work, "0", true, Some("Only chapter zero"))
        .await
        .unwrap();
    sync_entry_from_override_on(&mut tx, &md.work, "0")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let zero_after = entry(&pool, &md.work, "0").await;
    assert_eq!(zero_after.id, zero.id);
    assert!(zero_after.hidden);
    assert_eq!(
        zero_after.title_override.as_deref(),
        Some("Only chapter zero")
    );
    assert_same_attributes(&x_before_sync, &entry(&pool, &md.work, "x:special").await);
    let rows_after = snapshot(&pool, &md.work).await;
    for row in &unchanged_rows {
        assert!(rows_after.contains(row), "key 0 rewrote a different entry");
    }
    let overlays = work_entries(&pool, &md.work).await.unwrap();
    assert!(overlays.ready);
    let x = overlays.overlay("mangadex", "special", "0");
    assert_eq!(x.entry_id.as_deref(), Some(special.id.as_str()));
    assert_eq!(x.title_override.as_deref(), Some("Private appendix"));
    let fallback = overlays.overlay("mangadex", "not-linked-yet", "0");
    assert_eq!(fallback.entry_id, None);
    assert!(fallback.hidden);
    assert_eq!(
        fallback.title_override.as_deref(),
        Some("Only chapter zero")
    );
}

#[tokio::test]
async fn new_numbered_entry_inherits_an_existing_hidden_and_title_override() {
    let pool = pool().await;
    let md = md_work(&pool, "Override inheritance").await;
    override_row(&pool, &md.work, "1000", true, Some("Existing title")).await;
    let fallback = work_entries(&pool, &md.work)
        .await
        .unwrap()
        .overlay("mangadex", "ten", "1000");
    assert_eq!(fallback.entry_id, None);
    assert!(fallback.hidden);
    assert_eq!(fallback.title_override.as_deref(), Some("Existing title"));
    catalog::upsert_chapter(
        &pool,
        &md.source,
        &chapter("ten", Some("10"), "Source title"),
    )
    .await
    .unwrap();
    let ten = entry(&pool, &md.work, "1000").await;
    assert!(ten.hidden);
    assert_eq!(ten.title_override.as_deref(), Some("Existing title"));
    assert_eq!(ten.sort_key, 10.0);
    assert_eq!(linked_entry(&pool, &md.source, "ten").await, ten.id);
    let overlay = work_entries(&pool, &md.work)
        .await
        .unwrap()
        .overlay("mangadex", "ten", "1000");
    assert_eq!(overlay.entry_id.as_deref(), Some(ten.id.as_str()));
    assert!(overlay.hidden);
    assert_eq!(overlay.title_override.as_deref(), Some("Existing title"));
}

#[tokio::test]
async fn work_chapter_reads_are_counted_through_the_database_executor() {
    let pool = pool().await;
    let md = md_work(&pool, "Counted reads").await;
    catalog::upsert_chapter(&pool, &md.source, &chapter("one", Some("1"), "One"))
        .await
        .unwrap();
    make_ready(&pool, &md.work).await;
    let one = entry(&pool, &md.work, "100").await;
    let before = pool.statements_sent();
    work_entries(&pool, &md.work).await.unwrap();
    assert_eq!(pool.statements_sent() - before, 1);
    let before = pool.statements_sent();
    admin_entry_list(&pool, &md.work).await.unwrap();
    assert_eq!(pool.statements_sent() - before, 3);
    let before = pool.statements_sent();
    assert_eq!(entry_work(&pool, &one.id).await.unwrap(), Some(md.work));
    assert_eq!(pool.statements_sent() - before, 1);
    let before = pool.statements_sent();
    assert_eq!(remaining(&pool).await.unwrap(), (0, 0));
    assert_eq!(pool.statements_sent() - before, 1);
    let before = pool.statements_sent();
    assert_eq!(
        drain_once(&pool, None).await.unwrap().0,
        DrainOutcome::default()
    );
    assert_eq!(
        pool.statements_sent() - before,
        1,
        "the drain work-list read is counted"
    );
}
