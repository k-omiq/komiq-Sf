//! Native PostgreSQL overlap regressions. Every writer uses the foreground pool.
use super::*;
use crate::locks::pause_points::PausePoints;
use std::{future::Future, time::Duration};

pub(crate) async fn pool() -> Database {
    Database::connect("sqlite::memory:")
        .await
        .unwrap()
        .foreground()
}

pub(crate) async fn wait_for_lock(pool: &Database) -> bool {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity a WHERE a.datname = current_database() \
                 AND a.wait_event_type = 'Lock' AND cardinality(pg_blocking_pids(a.pid)) > 0)",
            ).fetch_one(pool).await.unwrap();
            if waiting { return; }
            tokio::task::yield_now().await;
        }
    }).await.is_ok()
}

pub(crate) async fn overlap<A, B, FA, FB, TA, TB>(
    pool: &Database,
    first: A,
    second: B,
) -> (Result<TA>, Result<TB>)
where
    A: FnOnce(Database) -> FA + Send + 'static,
    B: FnOnce(Database) -> FB + Send + 'static,
    FA: Future<Output = Result<TA>> + Send + 'static,
    FB: Future<Output = Result<TB>> + Send + 'static,
    TA: Send + 'static,
    TB: Send + 'static,
{
    let points = PausePoints::default();
    let held = points.arm("guard-held");
    let a_pool = pool.foreground();
    let a = tokio::spawn(guards::TEST_PAUSE.scope(points, async move { first(a_pool).await }));
    tokio::time::timeout(Duration::from_secs(5), held.reached())
        .await
        .expect("writer A did not reach its real transaction guard");
    let b_pool = pool.foreground();
    let b = tokio::spawn(async move { second(b_pool).await });
    let waited = wait_for_lock(pool).await;
    held.release();
    let a = tokio::time::timeout(Duration::from_secs(15), a)
        .await
        .unwrap()
        .unwrap();
    let b = tokio::time::timeout(Duration::from_secs(15), b)
        .await
        .unwrap()
        .unwrap();
    assert!(
        waited,
        "writer B never overlapped A on a PostgreSQL lock; leased-pool queuing is not evidence"
    );
    (a, b)
}

pub(crate) async fn work(pool: &Database, title: &str) -> String {
    create_work(
        pool,
        &WorkInput {
            primary_title: Some(title.into()),
            aliases: vec![Alias {
                raw: title.into(),
                lang: None,
            }],
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

pub(crate) async fn source(pool: &Database, work: &str, key: &str) -> String {
    upsert_source_series(pool, work, "suwayomi", "source", key, None, false)
        .await
        .unwrap()
}

fn chapter(id: &str, number: &str) -> ChapterInput {
    ChapterInput {
        external_id: id.into(),
        number: Some(number.into()),
        lang: Some("en".into()),
        published_at: Some("2020-01-01T00:00:00Z".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn mangadex_identity_has_one_work_and_anchor() {
    let pool = pool().await;
    let input = WorkInput {
        primary_title: Some("Identity".into()),
        tags: vec!["Action".into()],
        ..Default::default()
    };
    let a_input = input.clone();
    let (a, b) = overlap(
        &pool,
        move |pool| async move { upsert_work_from_mangadex(&pool, "identity", &a_input).await },
        move |pool| async move { upsert_work_from_mangadex(&pool, "identity", &input).await },
    )
    .await;
    assert_eq!(a.unwrap(), b.unwrap());
    let counts: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM work), (SELECT COUNT(*) FROM source_series), (SELECT COUNT(*) FROM work_external_id WHERE provider = 'mangadex' AND external_id = 'identity')")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        counts,
        (1, 1, 1),
        "losing identity claims must not leave an orphan work"
    );
}

#[tokio::test]
async fn external_identity_create_adopts_owner() {
    let pool = pool().await;
    let input = WorkInput {
        external_ids: vec![("al".into(), "one".into())],
        ..Default::default()
    };
    let mut other = input.clone();
    other.external_ids.push(("mal".into(), "two".into()));
    let (a, b) = overlap(
        &pool,
        move |pool| async move { create_work(&pool, &input).await },
        move |pool| async move { create_work(&pool, &other).await },
    )
    .await;
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let owners: Vec<String> = sqlx::query_scalar("SELECT DISTINCT work_id FROM work_external_id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM work_external_id")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn disjoint_splits_keep_one_source_in_both_orders() {
    for reverse in [false, true] {
        let pool = pool().await;
        let work = work(&pool, "Split").await;
        let one = source(&pool, &work, "1").await;
        let two = source(&pool, &work, "2").await;
        let a_work = work.clone();
        let b_work = work.clone();
        let (one, two) = if reverse { (two, one) } else { (one, two) };
        let (a, b) = overlap(
            &pool,
            move |pool| async move { split_source_series(&pool, &a_work, &[one], None).await },
            move |pool| async move { split_source_series(&pool, &b_work, &[two], None).await },
        )
        .await;
        assert_eq!(a.unwrap().moved_sources, 1);
        assert!(b
            .unwrap_err()
            .to_string()
            .contains("at least one source has to stay"));
        let counts: Vec<i64> = sqlx::query_scalar(
            "SELECT COUNT(*) FROM source_series GROUP BY work_id ORDER BY work_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(counts, vec![1, 1]);
    }
}

#[tokio::test]
async fn merge_preserves_aliases_library_and_history_in_both_orders() {
    for merge_first in [false, true] {
        let pool = pool().await;
        let loser = work(&pool, "Loser").await;
        let survivor = work(&pool, "Survivor").await;
        let ssid = source(&pool, &loser, "1").await;
        upsert_chapter(&pool, &ssid, &chapter("chapter", "1"))
            .await
            .unwrap();
        ledger::record_source_series(&pool, &ssid).await.unwrap();
        let user = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,username,email,password_hash,created_at) VALUES($1,'reader','reader@example.test','hash',now())")
            .bind(user).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO user_library(user_id,series_id,created_at,is_favorite,status) VALUES($1,$2,now(),TRUE,'READING')")
            .bind(user).bind(&loser).execute(&pool).await.unwrap();
        let l = loser.clone();
        let t = survivor.clone();
        let merge =
            move |pool: Database| async move { merge_works(&pool, &l, &t).await.map(|_| ()) };
        let alias_work = if merge_first {
            survivor.clone()
        } else {
            loser.clone()
        };
        let alias = move |pool: Database| async move {
            add_work_alias(&pool, &alias_work, "Kept Token")
                .await
                .map(|_| ())
        };
        let (a, b) = if merge_first {
            overlap(&pool, merge, alias).await
        } else {
            overlap(&pool, alias, merge).await
        };
        a.unwrap();
        b.unwrap();
        let owner: String = sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1")
            .bind(&ssid)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(owner, survivor);
        let favorite: bool = sqlx::query_scalar(
            "SELECT is_favorite FROM user_library WHERE user_id = $1 AND series_id = $2",
        )
        .bind(user)
        .bind(&survivor)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(favorite);
        let rows: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM release_event WHERE work_id=$1), (SELECT COUNT(*) FROM work_alias WHERE work_id=$1 AND raw_title='Kept Token'), (SELECT COUNT(*) FROM work_alias_token WHERE work_id=$1 AND token='kept')")
            .bind(&survivor).fetch_one(&pool).await.unwrap();
        assert_eq!(rows, (1, 1, 1));
        assert_eq!(
            redirect_work_id(&pool, &loser).await.unwrap().as_deref(),
            Some(survivor.as_str())
        );
    }
}

#[tokio::test]
async fn provisional_cleanup_keeps_a_concurrently_adopted_work() {
    let pool = pool().await;
    let work = work(&pool, "Adopted").await;
    let a_work = work.clone();
    let b_work = work.clone();
    let (a, b) = overlap(
        &pool,
        move |pool| async move {
            upsert_source_series(&pool, &a_work, "suwayomi", "source", "1", None, false).await
        },
        move |pool| async move { delete_work_cascade(&pool, &b_work).await },
    )
    .await;
    let ssid = a.unwrap();
    b.unwrap();
    assert_eq!(
        work_id_for_source_series(&pool, &ssid)
            .await
            .unwrap()
            .as_deref(),
        Some(work.as_str())
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn alias_removal_and_addition_preserve_shared_tokens_in_both_orders() {
    for remove_first in [false, true] {
        let pool = pool().await;
        let work = work(&pool, "Shared Base").await;
        add_work_alias(&pool, &work, "Shared Removed")
            .await
            .unwrap();
        let a_work = work.clone();
        let b_work = work.clone();
        let remove = move |pool: Database| async move {
            remove_work_alias(&pool, &a_work, "Shared Removed").await
        };
        let add = move |pool: Database| async move {
            add_work_alias(&pool, &b_work, "Shared Added")
                .await
                .map(|_| ())
        };
        let (a, b) = if remove_first {
            overlap(&pool, remove, add).await
        } else {
            overlap(&pool, add, remove).await
        };
        a.unwrap();
        b.unwrap();
        let tokens: Vec<String> = sqlx::query_scalar(
            "SELECT token FROM work_alias_token WHERE work_id=$1 ORDER BY token",
        )
        .bind(&work)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(tokens, vec!["added", "base", "shared"]);
    }
}

#[tokio::test]
async fn pin_source_update_and_unpin_share_the_parent() {
    let pool = pool().await;
    let work = upsert_work_from_mangadex(
        &pool,
        "pin",
        &WorkInput {
            author: Some("Original".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let a_work = work.clone();
    let (a, b) = overlap(
        &pool,
        move |pool| async move {
            let mut conn = pool.write().acquire().await?;
            pin_work_field(&mut conn, &a_work, PinnedField::Author, Some("Pinned")).await
        },
        move |pool| async move {
            upsert_work_from_mangadex(
                &pool,
                "pin",
                &WorkInput {
                    author: Some("Fresh".into()),
                    ..Default::default()
                },
            )
            .await
            .map(|_| ())
        },
    )
    .await;
    a.unwrap();
    b.unwrap();
    let values:(Option<String>,Option<String>)=sqlx::query_as("SELECT w.author,p.source_value FROM work w JOIN work_field_pin p ON p.work_id=w.id WHERE w.id=$1 AND p.field='author'").bind(&work).fetch_one(&pool).await.unwrap();
    assert_eq!(values, (Some("Pinned".into()), Some("Fresh".into())));
    let a_work = work.clone();
    let (a, b) = overlap(
        &pool,
        move |pool| async move {
            let mut conn = pool.write().acquire().await?;
            unpin_work_field(&mut conn, &a_work, PinnedField::Author).await
        },
        move |pool| async move {
            upsert_work_from_mangadex(
                &pool,
                "pin",
                &WorkInput {
                    author: Some("Newest".into()),
                    ..Default::default()
                },
            )
            .await
            .map(|_| ())
        },
    )
    .await;
    a.unwrap();
    b.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT author FROM work WHERE id=$1")
            .bind(work)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "Newest"
    );
}

#[tokio::test]
async fn mangadex_details_replace_tags_and_trackers_as_one_set() {
    let pool = pool().await;
    let work = upsert_work_from_mangadex(&pool, "details", &WorkInput::default())
        .await
        .unwrap();
    let a_work = work.clone();
    let (a, b) = overlap(
        &pool,
        move |pool| async move {
            refresh_mangadex_details(
                &pool,
                &a_work,
                "details",
                &WorkInput {
                    tags: vec!["Action".into()],
                    external_ids: vec![("al".into(), "1".into())],
                    ..Default::default()
                },
            )
            .await
        },
        move |pool| async move {
            upsert_work_from_mangadex(
                &pool,
                "details",
                &WorkInput {
                    tags: vec!["Drama".into()],
                    external_ids: vec![("mal".into(), "2".into())],
                    ..Default::default()
                },
            )
            .await
            .map(|_| ())
        },
    )
    .await;
    a.unwrap();
    b.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT tag FROM work_tag WHERE work_id=$1")
            .bind(&work)
            .fetch_all(&pool)
            .await
            .unwrap(),
        vec!["Drama"]
    );
    let trackers: Vec<(String, String)> = sqlx::query_as(
        "SELECT provider,external_id FROM work_tracker_link WHERE work_id=$1 ORDER BY provider",
    )
    .bind(work)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(trackers, vec![("mal".into(), "2".into())]);
}

#[tokio::test]
async fn source_claim_and_source_pin_converge_in_both_orders() {
    for pin_first in [false, true] {
        let pool = pool().await;
        let work = work(&pool, "Policy").await;
        let a_work = work.clone();
        let claim = move |pool: Database| async move {
            upsert_source_series(&pool, &a_work, "suwayomi", "source", "1", None, false)
                .await
                .map(|_| ())
        };
        let pin = move |pool: Database| async move {
            set_source_nsfw_pin(&pool, "source", true).await.map(|_| ())
        };
        let (a, b) = if pin_first {
            overlap(&pool, pin, claim).await
        } else {
            overlap(&pool, claim, pin).await
        };
        a.unwrap();
        b.unwrap();
        let flags:(bool,bool)=sqlx::query_as("SELECT w.is_nsfw_override,b.is_nsfw FROM work w JOIN browse_catalogue b ON b.work_id=w.id WHERE w.id=$1").bind(work).fetch_one(&pool).await.unwrap();
        assert_eq!(flags, (true, true));
    }
}

/// The panic message of a task that must have panicked.
#[cfg(debug_assertions)]
async fn panic_of<F>(task: F) -> String
where
    F: Future<Output = ()> + Send + 'static,
{
    let panic = tokio::spawn(task)
        .await
        .expect_err("the helper accepted a caller without its guards")
        .into_panic();
    match panic.downcast::<String>() {
        Ok(text) => *text,
        Err(other) => (*other.downcast::<&str>().unwrap()).to_owned(),
    }
}

/// The `_on` helpers neither begin nor lock for their caller. In debug and test builds
/// each one refuses a caller that did not take the guards its documentation names:
/// the mistake would otherwise show only as a rare deadlock or a lost update.
#[cfg(debug_assertions)]
#[tokio::test]
async fn on_helpers_refuse_a_caller_without_their_guards() {
    let pool = pool().await;
    let a = work(&pool, "Guarded A").await;
    let b = work(&pool, "Guarded B").await;
    let ssid = source(&pool, &a, "1").await;

    // An external identity key that the caller did not take.
    let writer = pool.clone();
    let message = panic_of(async move {
        let mut tx = writer.begin().await.unwrap();
        let input = WorkInput {
            primary_title: Some("Unguarded".into()),
            external_ids: vec![("mal".into(), "77".into())],
            ..Default::default()
        };
        let _ = create_work_on(&mut tx, &input).await;
    })
    .await;
    assert!(message.contains("catalog::create_work_on"), "{message}");
    assert!(message.contains("advisory keys"), "{message}");

    // A source claim without its identity keys, then with them but without the policy.
    for with_identity in [false, true] {
        let (writer, owner) = (pool.clone(), a.clone());
        let message = panic_of(async move {
            let mut tx = writer.begin().await.unwrap();
            if with_identity {
                crate::locks::advisory(
                    &mut tx,
                    &guards::source_identity_keys("suwayomi", "source", "9"),
                )
                .await
                .unwrap();
            }
            let _ =
                upsert_source_series_on(&mut tx, &owner, "suwayomi", "source", "9", None, false)
                    .await;
        })
        .await;
        assert!(
            message.contains("catalog::upsert_source_series_on"),
            "{message}"
        );
        assert_eq!(
            message.contains("SourcePolicy(source)"),
            with_identity,
            "{message}"
        );
    }

    // A merge that is not the first lock acquisition of its transaction.
    for early in ["row", "key"] {
        let (writer, loser, survivor) = (pool.clone(), a.clone(), b.clone());
        let message = panic_of(async move {
            let mut tx = writer.begin().await.unwrap();
            if early == "row" {
                crate::locks::lock_works(&mut tx, &[&survivor])
                    .await
                    .unwrap();
            } else {
                crate::locks::advisory(&mut tx, &[crate::locks::AdvisoryKey::membership()])
                    .await
                    .unwrap();
            }
            let _ = merge_works_guarded_on(&mut tx, &loser, &survivor, None, None).await;
        })
        .await;
        assert!(
            message.contains("catalog::merge_works_guarded_on"),
            "{early}: {message}"
        );
        assert!(
            message.contains("first lock acquisition"),
            "{early}: {message}"
        );
    }

    // A ledger write on a connection that is not inside a transaction.
    let writer = pool.clone();
    let message = panic_of(async move {
        let mut conn = writer.acquire().await.unwrap();
        let _ = ledger::record_source_series_on(&mut conn, &ssid).await;
    })
    .await;
    assert!(
        message.contains("ledger::record_source_series_on"),
        "{message}"
    );

    // Nothing above changed the catalogue, and the guarded forms still work.
    let mut tx = pool.begin().await.unwrap();
    merge_works_guarded_on(&mut tx, &a, &b, None, None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

/// Claims of one source take its policy key in SHARED mode: two claims of different
/// series of the source do not wait for each other. The admin pin of the source takes
/// the key exclusively and waits for every open claim, so a claim still never overlaps
/// a change of the policy it reads.
#[tokio::test]
async fn a_source_pin_excludes_claims_and_claims_share() {
    let pool = pool().await;
    let first = work(&pool, "Claim One").await;
    let second = work(&pool, "Claim Two").await;
    let points = PausePoints::default();
    let held = points.arm("guard-held");
    let (writer, owner) = (pool.foreground(), first.clone());
    let open_claim = tokio::spawn(guards::TEST_PAUSE.scope(points, async move {
        upsert_source_series(&writer, &owner, "suwayomi", "source", "1", None, false).await
    }));
    tokio::time::timeout(Duration::from_secs(30), held.reached())
        .await
        .expect("the first claim did not reach its guards");

    // A second claim of the same source finishes while the first one is still open.
    tokio::time::timeout(
        Duration::from_secs(30),
        upsert_source_series(&pool, &second, "suwayomi", "source", "2", None, false),
    )
    .await
    .expect("a claim waited for another claim of the same source")
    .expect("a claim waited for another claim of the same source");

    // The pin waits for the open claim.
    let writer = pool.foreground();
    let mut pin = tokio::spawn(async move { set_source_nsfw_pin(&writer, "source", true).await });
    tokio::select! {
        _ = crate::cover::wait_for_writer_lock(&pool) => {}
        done = &mut pin => panic!("the pin did not wait for an open claim: {done:?}"),
    }
    assert!(!source_nsfw_pinned(&pool, "source").await.unwrap());
    held.release();
    open_claim.await.unwrap().unwrap();
    assert_eq!(pin.await.unwrap().unwrap(), 2);
    let pinned: i64 = sqlx::query_scalar("SELECT count(*) FROM work WHERE is_nsfw_override")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(pinned, 2, "both claimed works carry the pin");
}

/// Two refreshes of one projection run one at a time. The second one must not stage
/// before the first has published: it waits for the projection's Job key, takes its
/// snapshot afterwards, and therefore repairs a row that changed after the first
/// refresh had staged. (Overlapping, both would have staged the old state.)
#[tokio::test]
async fn refreshes_of_one_projection_run_one_at_a_time() {
    let pool = pool().await;
    let (work, _) = mirror_fixture(&pool).await;
    refresh_browse_catalogue(&pool).await.unwrap();
    let points = PausePoints::default();
    let staged = points.arm("snapshot-staged");
    let first_pool = pool.foreground();
    let first = tokio::spawn(guards::TEST_PAUSE.scope(points, async move {
        refresh_browse_catalogue(&first_pool).await
    }));
    tokio::time::timeout(Duration::from_secs(30), staged.reached())
        .await
        .expect("the first refresh did not stage");
    let second_pool = pool.foreground();
    let second = tokio::spawn(async move { refresh_browse_catalogue(&second_pool).await });
    // The second refresh is asking for the key the first one holds.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let asking: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname = current_database() \
                 AND pid <> pg_backend_pid() AND query LIKE 'SELECT pg_try_advisory_lock(%')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if asking {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the second refresh did not wait for the first one");
    assert!(!second.is_finished());
    // After the first refresh staged: only a refresh that stages LATER can repair it.
    sqlx::query("UPDATE browse_catalogue SET title = 'Stale' WHERE work_id = $1")
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();
    staged.release();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    let title: String = sqlx::query_scalar("SELECT title FROM browse_catalogue WHERE work_id = $1")
        .bind(&work)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(title, "Mirror");
    // The key goes with the session: a later refresh is not held up.
    tokio::time::timeout(Duration::from_secs(30), refresh_browse_catalogue(&pool))
        .await
        .expect("the refresh key outlived its refresh")
        .unwrap();
}

/// Every input of a published row is part of the authority version: a new row version
/// in any of the ten tables must change it, or a publication turn would overwrite the
/// projection with a row staged from the older input.
#[tokio::test]
async fn the_authority_version_follows_every_input_table() {
    let pool = pool().await;
    let (work, _) = mirror_fixture(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO suwayomi_series (id, title, status, in_library, source_id, updated_at) \
           VALUES (7, 'Authority', 'ONGOING', TRUE, 'source', now()); \
         INSERT INTO suwayomi_chapter (id, manga_id, name, chapter_number, page_count, updated_at) \
           VALUES (70, 7, 'Ch 1', 1, 0, now()); \
         INSERT INTO series_scan_state (series_id, updated_at, next_scan_at) \
           VALUES ('7', now(), now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    source(&pool, &work, "7").await;
    for sql in [
        "INSERT INTO work_tag (work_id, tag, ord, source) VALUES ($1, 'Action', 0, 'admin')",
        "INSERT INTO work_stats (work_id, fetched_at) VALUES ($1, now())",
    ] {
        sqlx::query(sql).bind(&work).execute(&pool).await.unwrap();
    }
    let version = || async {
        let mut conn = pool.acquire().await.unwrap();
        refresh_authority(&mut conn, std::slice::from_ref(&work))
            .await
            .unwrap()
            .remove(&work)
            .expect("the work has an authority version")
    };
    let mut before = version().await;
    // A no-op UPDATE writes a new row version of exactly one input row.
    for (table, touch) in [
        ("work", "UPDATE work SET updated_at = updated_at WHERE id = $1"),
        ("source_series", "UPDATE source_series SET source_key = source_key WHERE work_id = $1 AND source_type = 'mangadex'"),
        ("chapter", "UPDATE chapter SET external_id = external_id WHERE source_series_id IN (SELECT id FROM source_series WHERE work_id = $1)"),
        ("suwayomi_chapter", "UPDATE suwayomi_chapter SET name = name WHERE manga_id = 7 AND $1 <> ''"),
        ("work_alias", "UPDATE work_alias SET lang = lang WHERE work_id = $1"),
        ("work_tag", "UPDATE work_tag SET ord = ord WHERE work_id = $1"),
        ("release_event", "UPDATE release_event SET label = label WHERE work_id = $1"),
        ("suwayomi_series", "UPDATE suwayomi_series SET title = title WHERE id = 7 AND $1 <> ''"),
        ("series_scan_state", "UPDATE series_scan_state SET updated_at = updated_at WHERE series_id = '7' AND $1 <> ''"),
        ("work_stats", "UPDATE work_stats SET fetched_at = fetched_at WHERE work_id = $1"),
    ] {
        let rows = sqlx::query(touch)
            .bind(&work)
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected();
        assert!(rows > 0, "{table}: the fixture has no row of this input");
        let after = version().await;
        assert_ne!(before, after, "{table}: a new row version did not change the authority version");
        before = after;
    }
}

/// The users tier of a merge. A merge moves library, progress and review rows of every
/// user who has rows on either work, so it locks those users FIRST, before any work.
/// Another transaction holds one of them (an account change in progress): the merge
/// must wait there, with both works still free. Without the users tier it would take
/// the works and then meet the user's rows below them, in the opposite order to every
/// writer that goes user first.
#[tokio::test]
async fn a_merge_waits_for_its_users_before_it_locks_a_work() {
    let pool = pool().await;
    let loser = work(&pool, "Merge Loser").await;
    let survivor = work(&pool, "Merge Survivor").await;
    source(&pool, &loser, "1").await;
    let user = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,email,password_hash,created_at) VALUES($1,'reader','reader@example.test','hash',now())")
        .bind(user).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO user_library(user_id,series_id,created_at,is_favorite,status) VALUES($1,$2,now(),TRUE,'READING')")
        .bind(user).bind(&loser).execute(&pool).await.unwrap();
    let mut account = pool.begin().await.unwrap();
    crate::locks::lock_users_for_delete(&mut account, &[user.to_string()])
        .await
        .unwrap();
    let (merging, l, t) = (pool.foreground(), loser.clone(), survivor.clone());
    let mut merge = tokio::spawn(async move { merge_works(&merging, &l, &t).await });
    tokio::select! {
        _ = crate::cover::wait_for_writer_lock(&pool) => {}
        done = &mut merge => panic!("the merge did not wait for the user: {done:?}"),
    }
    let mut probe = pool.begin().await.unwrap();
    let free = sqlx::query("SELECT id FROM work WHERE id = ANY($1) FOR NO KEY UPDATE NOWAIT")
        .bind(vec![loser.clone(), survivor.clone()])
        .execute(&mut *probe)
        .await;
    probe.rollback().await.unwrap();
    assert!(
        free.is_ok(),
        "the merge holds a work while it waits for a user: {free:?}"
    );
    account.rollback().await.unwrap();
    merge.await.unwrap().unwrap();
    let shelf: String = sqlx::query_scalar("SELECT series_id FROM user_library WHERE user_id = $1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(shelf, survivor);
}

/// 250 works (`w_pin_0001` to `w_pin_0250`) linked to the Suwayomi source `big`:
/// three chunks of a source pin.
async fn big_source(pool: &Database) {
    sqlx::raw_sql(
        "INSERT INTO work (id, primary_title, created_at, updated_at) \
           SELECT 'w_pin_' || lpad(n::text, 4, '0'), 'Pin ' || n, now(), now() \
             FROM generate_series(1, 250) n; \
         INSERT INTO source_series (id, work_id, source_type, source_id, source_key, created_at) \
           SELECT 'ss_pin_' || n, 'w_pin_' || lpad(n::text, 4, '0'), 'suwayomi', 'big', \
                  (1000 + n)::text, now() \
             FROM generate_series(1, 250) n",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// (works with the override, the highest such work, browse rows shown as adult).
async fn big_source_pins(pool: &Database) -> (i64, Option<String>, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM work WHERE is_nsfw_override), \
                (SELECT max(id) FROM work WHERE is_nsfw_override), \
                (SELECT count(*) FROM browse_catalogue WHERE is_nsfw)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A source pin is one transaction per 100 works. A failure in the second chunk leaves
/// the pin row and the first chunk committed, and a second call completes the rest.
/// (One transaction over the whole source would roll everything back.)
#[tokio::test]
async fn a_large_source_pin_commits_in_chunks_and_a_second_call_completes_it() {
    let pool = pool().await;
    big_source(&pool).await;
    sqlx::raw_sql(
        "CREATE FUNCTION reject_pin_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
           IF NEW.id = 'w_pin_0150' THEN RAISE EXCEPTION 'injected pin failure'; END IF; \
           RETURN NEW; END $$; \
         CREATE TRIGGER reject_pin BEFORE UPDATE OF is_nsfw_override ON work \
           FOR EACH ROW EXECUTE FUNCTION reject_pin_fn()",
    )
    .execute(&pool)
    .await
    .unwrap();
    let error = set_source_nsfw_pin(&pool, "big", true).await.unwrap_err();
    assert!(format!("{error:#}").contains("injected pin failure"));
    assert!(source_nsfw_pinned(&pool, "big").await.unwrap());
    assert_eq!(
        big_source_pins(&pool).await,
        (100, Some("w_pin_0100".into()), 100),
        "exactly the first chunk is committed"
    );

    sqlx::query("DROP TRIGGER reject_pin ON work")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(set_source_nsfw_pin(&pool, "big", true).await.unwrap(), 250);
    assert_eq!(
        big_source_pins(&pool).await,
        (250, Some("w_pin_0250".into()), 250)
    );
    assert_eq!(set_source_nsfw_pin(&pool, "big", false).await.unwrap(), 250);
    assert!(!source_nsfw_pinned(&pool, "big").await.unwrap());
    assert_eq!(big_source_pins(&pool).await, (0, None, 0));
}

/// While a pin waits for a work of its last chunk, the earlier chunks are committed and
/// their works are free: a writer of the first work does not wait for the pin.
#[tokio::test]
async fn a_source_pin_releases_each_chunk_before_the_next() {
    let pool = pool().await.foreground();
    big_source(&pool).await;
    let mut holder = pool.begin().await.unwrap();
    crate::locks::lock_works(&mut holder, &["w_pin_0250"])
        .await
        .unwrap();
    let writer = pool.clone();
    let mut pin = tokio::spawn(async move { set_source_nsfw_pin(&writer, "big", true).await });
    tokio::select! {
        _ = crate::cover::wait_for_writer_lock(&pool) => {}
        done = &mut pin => panic!("the pin did not wait for the held work: {done:?}"),
    }
    assert_eq!(
        big_source_pins(&pool).await,
        (200, Some("w_pin_0200".into()), 200),
        "two chunks are committed while the third waits"
    );
    let mut other = pool.begin().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(30),
        crate::locks::lock_works(&mut other, &["w_pin_0001"]),
    )
    .await
    .expect("a work of a committed chunk is still locked by the pin")
    .unwrap();
    other.rollback().await.unwrap();
    holder.rollback().await.unwrap();
    assert_eq!(pin.await.unwrap().unwrap(), 250);
    assert_eq!(
        big_source_pins(&pool).await,
        (250, Some("w_pin_0250".into()), 250)
    );
}

/// Another admin reverses the choice between two chunks of a pin. The pin must stop:
/// the other call walks every work, so the last committed choice wins on all of them.
/// A pin that went on would leave the works of its later chunks pinned without a pin.
#[tokio::test]
async fn a_source_pin_stops_when_the_choice_changes_between_chunks() {
    let pool = pool().await.foreground();
    big_source(&pool).await;
    let points = PausePoints::default();
    let between = points.arm("source-pin-chunk");
    let writer = pool.clone();
    let pin = tokio::spawn(guards::TEST_PAUSE.scope(points, async move {
        set_source_nsfw_pin(&writer, "big", true).await
    }));
    tokio::time::timeout(Duration::from_secs(30), between.reached())
        .await
        .expect("the pin did not finish its first chunk");
    assert_eq!(
        big_source_pins(&pool).await,
        (100, Some("w_pin_0100".into()), 100)
    );
    assert_eq!(set_source_nsfw_pin(&pool, "big", false).await.unwrap(), 250);
    between.release();
    assert_eq!(pin.await.unwrap().unwrap(), 100, "only the first chunk");
    assert!(!source_nsfw_pinned(&pool, "big").await.unwrap());
    assert_eq!(big_source_pins(&pool).await, (0, None, 0));
}

#[tokio::test]
async fn chapter_replacement_and_incremental_upsert_share_parents_in_both_orders() {
    for replace_first in [false, true] {
        let pool = pool().await;
        let work = work(&pool, "Chapters").await;
        let ssid = source(&pool, &work, "1").await;
        let a_ssid = ssid.clone();
        let b_ssid = ssid.clone();
        let replace = move |pool: Database| async move {
            replace_source_chapters(&pool, &a_ssid, &[chapter("whole", "1")]).await
        };
        let insert = move |pool: Database| async move {
            upsert_chapter(&pool, &b_ssid, &chapter("incremental", "2")).await
        };
        let (a, b) = if replace_first {
            overlap(&pool, replace, insert).await
        } else {
            overlap(&pool, insert, replace).await
        };
        a.unwrap();
        b.unwrap();
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT external_id FROM chapter WHERE source_series_id=$1 ORDER BY external_id",
        )
        .bind(ssid)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            ids,
            if replace_first {
                vec!["incremental", "whole"]
            } else {
                vec!["whole"]
            }
        );
    }
}

#[tokio::test]
async fn every_ledger_writer_preserves_owner_through_merge_in_both_orders() {
    for seed in ["batch", "work", "source"] {
        for merge_first in [false, true] {
            let pool = pool().await;
            let loser = work(&pool, "Ledger").await;
            let target = work(&pool, "Target").await;
            let ssid = source(&pool, &loser, "1").await;
            upsert_chapter(&pool, &ssid, &chapter("ledger", "1"))
                .await
                .unwrap();
            let l = loser.clone();
            let t = target.clone();
            let merge =
                move |pool: Database| async move { merge_works(&pool, &l, &t).await.map(|_| ()) };
            let seed = move |pool: Database| async move {
                match seed {
                    "batch" => ledger::seed_batch(&pool).await?,
                    "work" => {
                        ledger::seed_work(&pool, &loser, Utc::now().timestamp_millis()).await?
                    }
                    _ => ledger::record_source_series(&pool, &ssid).await?,
                };
                Ok(())
            };
            let (a, b) = if merge_first {
                overlap(&pool, merge, seed).await
            } else {
                overlap(&pool, seed, merge).await
            };
            a.unwrap();
            b.unwrap();
            let events: Vec<(String, String)> =
                sqlx::query_as("SELECT work_id,chapter_key FROM release_event")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(events, vec![(target, "100".into())]);
        }
    }
}

#[tokio::test]
async fn review_pairs_suppress_siblings_and_reverse_orientation() {
    let pool = pool().await;
    let a = work(&pool, "A").await;
    let b = work(&pool, "B").await;
    let a_source = source(&pool, &a, "1").await;
    let sibling = source(&pool, &a, "2").await;
    let reverse = source(&pool, &b, "3").await;
    let a_work = a.clone();
    let b_work = b.clone();
    let (one, two) =
        overlap(
            &pool,
            move |pool| async move {
                insert_merge_candidate(&pool, &a_source, &b_work, 0.7, "test").await
            },
            move |pool| async move {
                insert_merge_candidate(&pool, &reverse, &a_work, 0.7, "test").await
            },
        )
        .await;
    assert!(one.unwrap().is_some());
    assert!(two.unwrap().is_none());
    assert!(insert_merge_candidate(&pool, &sibling, &b, 0.8, "test")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn source_outage_has_one_alert_claim_and_clear_shares_the_guard() {
    let pool = pool().await;
    let record = |pool: Database| async move {
        record_source_outage(&pool, "source", None, 10, 10, "fetch_error", None, 24).await
    };
    let (a, b) = overlap(&pool, record, record).await;
    assert!(a.unwrap());
    assert!(!b.unwrap());
    let (a, b) = overlap(
        &pool,
        move |pool| async move { clear_source_outage(&pool, "source").await },
        record,
    )
    .await;
    assert!(a.unwrap());
    assert!(b.unwrap(), "an outage after recovery is a new alert");
    assert_eq!(source_outages(&pool).await.unwrap().len(), 1);
}

#[tokio::test]
async fn breaker_claim_survives_a_concurrent_reset() {
    let pool = pool().await;
    set_extension_subscription(&pool, "pkg", true)
        .await
        .unwrap();
    sqlx::query("UPDATE extension_subscription SET consecutive_failures=$1 WHERE pkg_name='pkg'")
        .bind(SUBSCRIPTION_FAILURE_LIMIT - 1)
        .execute(&pool)
        .await
        .unwrap();
    let points = PausePoints::default();
    let returned = points.arm("subscription-returned");
    let writer_pool = pool.clone();
    let writer = tokio::spawn(guards::TEST_PAUSE.scope(points, async move {
        mark_subscription_synced(&writer_pool, "pkg", 0, Some("failure")).await
    }));
    tokio::time::timeout(Duration::from_secs(5), returned.reached())
        .await
        .unwrap();
    reset_subscription_breaker(&pool, "pkg").await.unwrap();
    returned.release();
    assert!(
        writer.await.unwrap().unwrap(),
        "RETURNING must report this operation's own transition"
    );
    let state: (i64, Option<chrono::DateTime<Utc>>) = sqlx::query_as(
        "SELECT consecutive_failures,disabled_at FROM extension_subscription WHERE pkg_name='pkg'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, (0, None));
}

/// Two failing passes of the same subscription arrive together with one strike left.
/// The statement reads the prior state under a row lock, so the second pass sees the
/// first one's trip and does not claim it again. A third transaction holds the row until
/// both passes are waiting, which makes the overlap certain: without the lock inside the
/// statement both passes read "not tripped" before either writes, and both claim.
#[tokio::test]
async fn two_failing_passes_claim_one_breaker_trip() {
    let pool = pool().await;
    set_extension_subscription(&pool, "pkg", true)
        .await
        .unwrap();
    sqlx::query("UPDATE extension_subscription SET consecutive_failures=$1 WHERE pkg_name='pkg'")
        .bind(SUBSCRIPTION_FAILURE_LIMIT - 1)
        .execute(&pool)
        .await
        .unwrap();
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pkg_name FROM extension_subscription WHERE pkg_name='pkg' FOR UPDATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let passes: Vec<_> = (0..2)
        .map(|_| {
            let pool = pool.foreground();
            tokio::spawn(
                async move { mark_subscription_synced(&pool, "pkg", 0, Some("failure")).await },
            )
        })
        .collect();
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity a WHERE a.datname = current_database() \
                 AND a.wait_event_type = 'Lock' AND cardinality(pg_blocking_pids(a.pid)) > 0",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting == 2 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both passes must wait for the subscription row");
    holder.rollback().await.unwrap();
    let mut claims = 0;
    for pass in passes {
        claims += i64::from(pass.await.unwrap().unwrap());
    }
    assert_eq!(claims, 1, "exactly one pass may report the trip");
    let state: (i64, bool) = sqlx::query_as(
        "SELECT consecutive_failures, disabled_at IS NOT NULL FROM extension_subscription WHERE pkg_name='pkg'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, (SUBSCRIPTION_FAILURE_LIMIT + 1, true));
}

async fn mirror_fixture(pool: &Database) -> (String, String) {
    mirror_work(pool, "mirror", "Mirror").await
}

/// A MangaDex work with one dated chapter and a live row in each projection.
async fn mirror_work(pool: &Database, mangadex_id: &str, title: &str) -> (String, String) {
    let work = upsert_work_from_mangadex(
        pool,
        mangadex_id,
        &WorkInput {
            primary_title: Some(title.into()),
            primary_lang: Some("en".into()),
            aliases: vec![Alias {
                raw: title.into(),
                lang: None,
            }],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let ssid: String = sqlx::query_scalar("SELECT id FROM source_series WHERE work_id=$1")
        .bind(&work)
        .fetch_one(pool)
        .await
        .unwrap();
    upsert_chapter(pool, &ssid, &chapter("dated", "1"))
        .await
        .unwrap();
    ledger::record_source_series(pool, &ssid).await.unwrap();
    assert!(publish_mirror_feed_row(pool, &work).await.unwrap());
    (work, ssid)
}

#[tokio::test]
async fn refresh_facades_lock_each_work_before_publication() {
    for refresh in ["feed", "series", "browse", "reconcile", "search"] {
        let pool = pool().await;
        let (work, _) = mirror_fixture(&pool).await;
        if refresh == "search" {
            sqlx::query("DELETE FROM work_search_alias WHERE id = 'title:' || $1")
                .bind(&work)
                .execute(&pool)
                .await
                .unwrap();
        }
        // A rebuild opens a writer turn (and takes the work lock) only for a row it has
        // to change, so every variant starts from a live projection row that is wrong.
        let stale = match refresh {
            "feed" | "reconcile" => Some("feed_updates"),
            "series" => Some("feed_series_updates"),
            "browse" => Some("browse_catalogue"),
            _ => None,
        };
        if let Some(table) = stale {
            let rows = sqlx::query(&format!(
                "UPDATE {table} SET title='Stale' WHERE work_id=$1"
            ))
            .bind(&work)
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected();
            assert_eq!(
                rows, 1,
                "{refresh}: the fixture must own a live {table} row"
            );
        }
        let edited = work.clone();
        let (a, b) = overlap(
            &pool,
            move |pool| async move {
                match refresh {
                    "feed" => {
                        refresh_feed_updates(&pool).await?;
                    }
                    "series" => {
                        refresh_feed_series_updates(&pool).await?;
                    }
                    "browse" => {
                        refresh_browse_catalogue(&pool).await?;
                    }
                    "reconcile" => {
                        ledger::reconcile_feed(&pool).await?;
                    }
                    _ => {
                        refresh_work_fts(&pool).await?;
                    }
                }
                Ok(())
            },
            move |pool| async move {
                add_work_alias(&pool, &edited, "Concurrent Alias")
                    .await
                    .map(|_| ())
            },
        )
        .await;
        a.unwrap();
        b.unwrap();
        let counts:(i64,i64)=sqlx::query_as("SELECT (SELECT COUNT(*) FROM work_alias WHERE work_id=$1 AND raw_title='Concurrent Alias'), (SELECT COUNT(*) FROM work_search_alias WHERE work_id=$1 AND raw_title='Concurrent Alias')")
            .bind(&work).fetch_one(&pool).await.unwrap();
        assert_eq!(
            counts,
            (1, 1),
            "{refresh}: a rebuild must preserve committed aliases and unique search rows"
        );
        if let Some(table) = stale {
            let title: String =
                sqlx::query_scalar(&format!("SELECT title FROM {table} WHERE work_id=$1"))
                    .bind(&work)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                title, "Mirror",
                "{refresh}: the rebuild must republish the stale {table} row under the work lock"
            );
        }
        let card: (String, i64) =
            sqlx::query_as("SELECT title,en_chapter_count FROM browse_catalogue WHERE work_id=$1")
                .bind(work)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(card, ("Mirror".into(), 1));
    }
}

/// A publication turn must never WAIT for a work. Another transaction holds one of the
/// two works whose rows the rebuild has to change, for the whole rebuild. The rebuild
/// must finish, publish the free work's row, and leave the held work's row for the
/// next refresh. A turn that waited would hold the free work's lock meanwhile (and time
/// out here, on the foreground lane's 5 s lock timeout).
#[tokio::test]
async fn a_publication_turn_never_waits_for_a_work_a_writer_holds() {
    for (refresh, table) in [
        ("feed", "feed_updates"),
        ("series", "feed_series_updates"),
        ("browse", "browse_catalogue"),
    ] {
        let pool = pool().await.foreground();
        let (held, _) = mirror_work(&pool, "mirror-held", "Held").await;
        let (free, _) = mirror_work(&pool, "mirror-free", "Free").await;
        let rebuild = |pool: Database| async move {
            match refresh {
                "feed" => refresh_feed_updates(&pool).await.map(|_| ()),
                "series" => refresh_feed_series_updates(&pool).await.map(|_| ()),
                _ => refresh_browse_catalogue(&pool).await.map(|_| ()),
            }
        };
        let titles = || async {
            sqlx::query_scalar::<_, String>(&format!(
                "SELECT title FROM {table} WHERE work_id = ANY($1) ORDER BY work_id"
            ))
            .bind(vec![held.clone(), free.clone()])
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        let in_id_order = |held_title: &str, free_title: &str| {
            let mut rows = [
                (held.clone(), held_title.to_owned()),
                (free.clone(), free_title.to_owned()),
            ];
            rows.sort();
            rows.map(|(_, title)| title).to_vec()
        };
        rebuild(pool.clone()).await.unwrap();
        assert_eq!(
            sqlx::query(&format!(
                "UPDATE {table} SET title = 'Stale' WHERE work_id = ANY($1)"
            ))
            .bind(vec![held.clone(), free.clone()])
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected(),
            2,
            "{refresh}: the fixture must own two live {table} rows"
        );

        let mut writer = pool.begin().await.unwrap();
        crate::locks::lock_works(&mut writer, &[&held])
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), rebuild(pool.clone()))
            .await
            .unwrap_or_else(|_| panic!("{refresh}: the rebuild waited for a held work"))
            .unwrap_or_else(|e| panic!("{refresh}: the rebuild waited for a held work: {e:#}"));
        assert_eq!(
            titles().await,
            in_id_order("Stale", "Free"),
            "{refresh}: the free work is published, the held work is left alone"
        );

        writer.rollback().await.unwrap();
        rebuild(pool.clone()).await.unwrap();
        assert_eq!(
            titles().await,
            in_id_order("Held", "Free"),
            "{refresh}: the next refresh publishes the row that was left"
        );
    }
}

/// The turn's row count follows the time the turn held its transaction.
#[test]
fn the_publication_turn_shrinks_when_it_runs_out_of_time() {
    let full = REFRESH_BATCH_ROWS as usize;
    assert_eq!(next_turn_rows(full, REFRESH_TURN), full / 2);
    assert_eq!(
        next_turn_rows(1, REFRESH_TURN * 10),
        1,
        "a turn always takes a row"
    );
    assert_eq!(next_turn_rows(full / 4, REFRESH_TURN / 4), full / 2);
    assert_eq!(
        next_turn_rows(full, Duration::ZERO),
        full,
        "never above the batch"
    );
    assert_eq!(next_turn_rows(8, REFRESH_TURN * 3 / 4), 8);
}

/// A rebuild that has nothing to change must not take a work lock: on the real catalogue
/// that is almost every row of every refresh, and each lock makes a foreground writer of
/// that work wait. A foreign transaction holds the only work's row lock for the whole
/// rebuild here; a rebuild that locked the row would wait for it and time out.
#[tokio::test]
async fn unchanged_rebuilds_take_no_work_lock() {
    let pool = pool().await;
    let (work, _) = mirror_fixture(&pool).await;
    // Converge first, so the three projections already hold what a rebuild computes.
    refresh_feed_updates(&pool).await.unwrap();
    refresh_browse_catalogue(&pool).await.unwrap();
    let before: (String, String, String) = sqlx::query_as(
        "SELECT (SELECT md5(f::text) FROM feed_updates f WHERE work_id=$1), \
                (SELECT md5(f::text) FROM feed_series_updates f WHERE work_id=$1), \
                (SELECT md5(f::text) FROM browse_catalogue f WHERE work_id=$1)",
    )
    .bind(&work)
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM work WHERE id=$1 FOR UPDATE")
        .bind(&work)
        .execute(&mut *holder)
        .await
        .unwrap();
    let rebuild = pool.foreground();
    let done = tokio::time::timeout(Duration::from_secs(4), async move {
        let feed = refresh_feed_updates(&rebuild).await?;
        let browse = refresh_browse_catalogue(&rebuild).await?;
        anyhow::Ok((feed, browse))
    })
    .await
    .expect("an unchanged rebuild waited for a work row lock");
    done.unwrap();
    holder.rollback().await.unwrap();
    let after: (String, String, String) = sqlx::query_as(
        "SELECT (SELECT md5(f::text) FROM feed_updates f WHERE work_id=$1), \
                (SELECT md5(f::text) FROM feed_series_updates f WHERE work_id=$1), \
                (SELECT md5(f::text) FROM browse_catalogue f WHERE work_id=$1)",
    )
    .bind(&work)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn mirror_and_ledger_projection_exclude_chapter_writers() {
    for projection in ["mirror", "connection", "facade"] {
        let pool = pool().await;
        let (work, ssid) = mirror_fixture(&pool).await;
        let projected = work.clone();
        let replaced = ssid.clone();
        let (a, b) = overlap(
            &pool,
            move |pool| async move {
                match projection {
                    "mirror" => {
                        publish_mirror_feed_row(&pool, &projected).await?;
                    }
                    "connection" => {
                        let mut tx = pool.begin().await?;
                        project_feed_from_ledger(&mut tx, Some(&projected)).await?;
                        tx.commit().await?;
                    }
                    _ => {
                        project_feed_from_ledger_for_work(&pool, &projected).await?;
                    }
                }
                Ok(())
            },
            move |pool| async move {
                replace_source_chapters(&pool, &replaced, &[chapter("new", "2")]).await
            },
        )
        .await;
        a.unwrap();
        b.unwrap();
        ledger::record_source_series(&pool, &ssid).await.unwrap();
        publish_mirror_feed_row(&pool, &work).await.unwrap();
        let card: (String, i64) = sqlx::query_as(
            "SELECT latest_chapter,en_chapter_count FROM feed_series_updates WHERE work_id=$1",
        )
        .bind(work)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Equal release timestamps retain the ledger's deterministic chapter-key tie.
        assert_eq!(card, ("1".into(), 1));
    }
}

#[tokio::test]
async fn snapshot_publication_rejects_changed_authority_without_a_projection_edit() {
    let pool = pool().await;
    let work = work(&pool, "Captured").await;
    source(&pool, &work, "1").await;
    let mut snapshot = refresh_snapshot(&pool, &["browse_catalogue"])
        .await
        .unwrap();
    // Stage a real, DIFFERENT row for the work: without it the snapshot would only reach
    // the prune path, which the next test covers.
    let staged_rows = sqlx::query(
        "INSERT INTO pg_temp.browse_catalogue SELECT * FROM pg_temp.previous_browse_catalogue",
    )
    .execute(&mut *snapshot)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(staged_rows, 1);
    sqlx::query("UPDATE pg_temp.browse_catalogue SET title='Staged'")
        .execute(&mut *snapshot)
        .await
        .unwrap();
    let points = PausePoints::default();
    let staged = points.arm("snapshot-staged");
    let publish_pool = pool.clone();
    let publisher = tokio::spawn(guards::TEST_PAUSE.scope(points, async move {
        publish_snapshot(&publish_pool, &mut snapshot, "browse_catalogue").await
    }));
    tokio::time::timeout(Duration::from_secs(5), staged.reached())
        .await
        .unwrap();
    let mut conn = pool.write().acquire().await.unwrap();
    pin_work_field(&mut conn, &work, PinnedField::Author, Some("Edited"))
        .await
        .unwrap();
    drop(conn);
    staged.release();
    assert_eq!(publisher.await.unwrap().unwrap(), 0);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT title FROM browse_catalogue WHERE work_id=$1")
            .bind(work)
            .fetch_one(&pool)
            .await
            .unwrap(),
        "Captured"
    );
}

#[tokio::test]
async fn snapshot_prune_rejects_a_revalidated_source_even_when_the_row_is_equal() {
    let pool = pool().await;
    let work = work(&pool, "Retained").await;
    source(&pool, &work, "1").await;
    let mut snapshot = refresh_snapshot(&pool, &["browse_catalogue"])
        .await
        .unwrap();
    sqlx::query("DELETE FROM browse_catalogue")
        .execute(&mut *snapshot)
        .await
        .unwrap();
    let points = PausePoints::default();
    let staged = points.arm("snapshot-staged");
    let publish_pool = pool.clone();
    let publisher = tokio::spawn(guards::TEST_PAUSE.scope(points, async move {
        publish_snapshot(&publish_pool, &mut snapshot, "browse_catalogue").await
    }));
    tokio::time::timeout(Duration::from_secs(5), staged.reached())
        .await
        .unwrap();
    source(&pool, &work, "1").await;
    staged.release();
    assert_eq!(publisher.await.unwrap().unwrap(), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM browse_catalogue WHERE work_id=$1")
            .bind(work)
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn snapshot_session_uses_background_limits_and_retains_its_slot() {
    let pool = pool().await;
    let mut snapshot = refresh_snapshot(&pool, &["browse_catalogue"])
        .await
        .unwrap();
    let limits = crate::config::DatabaseLimits::from_env().background;
    let timeouts:(i64,i64,i64)=sqlx::query_as("SELECT (EXTRACT(EPOCH FROM current_setting('statement_timeout')::interval)*1000)::bigint, (EXTRACT(EPOCH FROM current_setting('lock_timeout')::interval)*1000)::bigint, (EXTRACT(EPOCH FROM current_setting('idle_in_transaction_session_timeout')::interval)*1000)::bigint")
        .fetch_one(&mut *snapshot).await.unwrap();
    assert_eq!(
        timeouts,
        (
            limits.statement_ms as i64,
            limits.lock_ms as i64,
            limits.idle_transaction_ms as i64
        )
    );
    assert_eq!(snapshot._permit.num_permits(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SHOW transaction_read_only")
            .fetch_one(&mut *snapshot)
            .await
            .unwrap(),
        "on"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SHOW transaction_isolation")
            .fetch_one(&mut *snapshot)
            .await
            .unwrap(),
        "repeatable read"
    );
    // The larger staging budget cannot permit changes to public data.
    let error = sqlx::query("INSERT INTO public.work (id, primary_title, created_at, updated_at) VALUES ('w_forbidden_snapshot', 'Forbidden', now(), now())")
        .execute(&mut *snapshot).await.unwrap_err();
    assert_eq!(
        error.as_database_error().unwrap().code().as_deref(),
        Some("25006")
    );
    assert_eq!(snapshot._permit.num_permits(), 1);
}

#[tokio::test]
async fn chapter_key_drain_does_not_overwrite_a_concurrent_chapter_edit() {
    let pool = pool().await;
    let work = work(&pool, "Keys").await;
    let ssid = source(&pool, &work, "1").await;
    upsert_chapter(&pool, &ssid, &chapter("edited", "1"))
        .await
        .unwrap();
    sqlx::query("UPDATE chapter SET chapter_key=NULL,label=NULL WHERE source_series_id=$1")
        .bind(&ssid)
        .execute(&pool)
        .await
        .unwrap();
    let points = PausePoints::default();
    let discovered = points.arm("keys-discovered");
    let drain_pool = pool.clone();
    let drain = tokio::spawn(guards::TEST_PAUSE.scope(points, async move {
        spine::drain_chapter_keys(&drain_pool).await
    }));
    tokio::time::timeout(Duration::from_secs(5), discovered.reached())
        .await
        .unwrap();
    upsert_chapter(&pool, &ssid, &chapter("edited", "2"))
        .await
        .unwrap();
    discovered.release();
    assert_eq!(drain.await.unwrap().unwrap(), 0);
    let keyed: (String, String) =
        sqlx::query_as("SELECT chapter_key,label FROM chapter WHERE source_series_id=$1")
            .bind(&ssid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(keyed, ("200".into(), "2".into()));
    sqlx::query("UPDATE chapter SET chapter_key=NULL,label=NULL WHERE source_series_id=$1")
        .bind(&ssid)
        .execute(&pool)
        .await
        .unwrap();
    let (a, b) = overlap(
        &pool,
        move |pool| async move {
            replace_source_chapters(&pool, &ssid, &[chapter("edited", "3")]).await
        },
        move |pool| async move { spine::drain_chapter_keys(&pool).await },
    )
    .await;
    a.unwrap();
    assert_eq!(b.unwrap(), 0);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT chapter_key FROM chapter")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "300"
    );
}

#[tokio::test]
async fn suwayomi_spine_drain_and_replacement_share_inputs_in_both_orders() {
    for drain_first in [false, true] {
        let pool = pool().await;
        let work = work(&pool, "Drain").await;
        let ssid = source(&pool, &work, "1").await;
        sqlx::query("INSERT INTO suwayomi_series(id,title,status,source_id,lang,updated_at) VALUES(1,'Drain','ONGOING','source','en',now())").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO suwayomi_chapter(id,manga_id,name,chapter_number,updated_at) VALUES(1,1,'Old',1,now())").execute(&pool).await.unwrap();
        let drain = move |pool: Database| async move {
            spine::drain_suwayomi_series(&pool).await.map(|_| ())
        };
        let replace = move |pool: Database| async move {
            replace_source_chapters(&pool, &ssid, &[chapter("fresh", "2")]).await
        };
        let (a, b) = if drain_first {
            overlap(&pool, drain, replace).await
        } else {
            overlap(&pool, replace, drain).await
        };
        a.unwrap();
        b.unwrap();
        let rows: Vec<String> = sqlx::query_scalar("SELECT external_id FROM chapter")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows, vec!["fresh"]);
    }
}

#[tokio::test]
async fn source_claim_adopts_the_stored_owner_across_source_ids() {
    let pool = pool().await;
    let a_work = work(&pool, "One").await;
    let b_work = work(&pool, "Two").await;
    let original = a_work.clone();
    let (a, b) = overlap(
        &pool,
        move |pool| async move {
            upsert_source_series(&pool, &a_work, "suwayomi", "", "1", None, false).await
        },
        move |pool| async move {
            upsert_source_series(&pool, &b_work, "suwayomi", "source", "1", None, false).await
        },
    )
    .await;
    let a = a.unwrap();
    assert_eq!(a, b.unwrap());
    assert_eq!(
        work_id_for_source_series(&pool, &a).await.unwrap(),
        Some(original)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM source_series")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn purge_rechecks_orphanhood_after_a_concurrent_enrolment() {
    let pool = pool().await;
    let work = work(&pool, "Purge").await;
    source(&pool, &work, "1").await;
    sqlx::query("INSERT INTO suwayomi_series(id,title,status,source_id,lang,updated_at) VALUES(1,'Foreign','ONGOING','source','ja',now())").execute(&pool).await.unwrap();
    let enrolled = work.clone();
    let (a, b) = overlap(
        &pool,
        move |pool| async move {
            upsert_source_series(&pool, &enrolled, "suwayomi", "english", "2", None, false).await
        },
        move |pool| async move { purge_foreign_language_suwayomi(&pool).await },
    )
    .await;
    let kept = a.unwrap();
    assert_eq!(b.unwrap(), vec![1]);
    assert_eq!(
        work_id_for_source_series(&pool, &kept).await.unwrap(),
        Some(work)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn purge_and_absent_source_claim_share_the_global_key() {
    let pool = pool().await;
    let work = work(&pool, "Unlinked").await;
    sqlx::query("INSERT INTO suwayomi_series(id,title,status,source_id,lang,updated_at) VALUES(1,'Foreign','ONGOING','source','ja',now())")
        .execute(&pool).await.unwrap();
    let (a, b) = overlap(
        &pool,
        move |pool| async move {
            upsert_source_series(&pool, &work, "suwayomi", "source", "1", None, false).await
        },
        move |pool| async move { purge_foreign_language_suwayomi(&pool).await },
    )
    .await;
    a.unwrap();
    assert_eq!(b.unwrap(), vec![1]);
    let counts:(i64,i64,i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM work), (SELECT COUNT(*) FROM source_series), (SELECT COUNT(*) FROM suwayomi_series)")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        counts,
        (0, 0, 0),
        "purge must restart changed discovery and clean the claimed parent"
    );
}

#[tokio::test]
async fn suwayomi_tag_plans_are_rebuilt_after_the_parent_lock() {
    let pool = pool().await;
    let work = work(&pool, "Tags").await;
    source(&pool, &work, "1").await;
    let a_work = work.clone();
    let b_work = work.clone();
    let (a, b) = overlap(
        &pool,
        move |pool| async move {
            crate::tags::sync_work_suwayomi_tags(&pool, &a_work, None, &["Action".into()]).await
        },
        move |pool| async move {
            crate::tags::sync_work_suwayomi_tags(&pool, &b_work, None, &["Drama".into()]).await
        },
    )
    .await;
    a.unwrap();
    b.unwrap();
    let tags: Vec<String> =
        sqlx::query_scalar("SELECT tag FROM work_tag WHERE work_id=$1 ORDER BY tag")
            .bind(work)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(tags, vec!["Drama"]);
}

#[test]
fn source_identity_keys_cover_unknown_source_claims() {
    let unknown = guards::source_keys("suwayomi", "", "1");
    let known = guards::source_keys("suwayomi", "source", "1");
    assert!(unknown.iter().any(|key| known.contains(key)));
    assert!(!unknown
        .iter()
        .any(|key| guards::source_keys("suwayomi", "other", "2").contains(key)));
}

// ── Fix round 1: lock strength ───────────────────────────────────────────────────

/// The lock a foreign-key check takes on its parent row, held by an open transaction:
/// what an uncommitted child insert under `table.id` looks like to other writers.
async fn key_share(
    pool: &Database,
    rows: &[(&str, &str)],
) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut holder = pool.begin().await.unwrap();
    for (table, id) in rows {
        let locked = sqlx::query(&format!("SELECT id FROM {table} WHERE id=$1 FOR KEY SHARE"))
            .bind(id)
            .execute(&mut *holder)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(locked, 1, "{table} {id} must exist");
    }
    holder
}

/// A merge deletes only its source work. The target keeps its key, so a child insert
/// under the target must not wait for the merge (a delete-capable lock on the target
/// would make it wait, and time the merge out behind any long child writer).
#[tokio::test]
async fn merge_takes_a_delete_capable_lock_only_on_the_work_it_deletes() {
    let pool = pool().await;
    let source_work = work(&pool, "Merge Loser").await;
    let target = work(&pool, "Merge Winner").await;
    source(&pool, &source_work, "1").await;
    source(&pool, &target, "2").await;
    let holder = key_share(&pool, &[("work", &target)]).await;
    let merging = pool.foreground();
    let (loser, winner) = (source_work.clone(), target.clone());
    let merged = tokio::time::timeout(Duration::from_secs(3), async move {
        merge_works(&merging, &loser, &winner).await
    })
    .await
    .expect("the merge waited for a key-share holder of the surviving work");
    assert_eq!(merged.unwrap().moved_source_series, 1);
    holder.rollback().await.unwrap();
    // The work that IS deleted still takes the delete-capable lock.
    let third = work(&pool, "Merge Third").await;
    let holder = key_share(&pool, &[("work", &third)]).await;
    let merging = pool.foreground();
    let (loser, winner) = (third.clone(), target.clone());
    let blocked = tokio::spawn(async move { merge_works(&merging, &loser, &winner).await });
    assert!(
        wait_for_lock(&pool).await,
        "a merge must wait for a key-share holder of the work it deletes"
    );
    holder.rollback().await.unwrap();
    blocked.await.unwrap().unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM work")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

/// The purge deletes orphaned works and foreign-language source mappings. A consolidated
/// work survives with its other sources, so child inserts under either must not wait.
#[tokio::test]
async fn purge_takes_delete_capable_locks_only_on_what_it_deletes() {
    let pool = pool().await;
    let (kept, mangadex_source) = mirror_fixture(&pool).await;
    let foreign = source(&pool, &kept, "1").await;
    sqlx::query("INSERT INTO suwayomi_series(id,title,status,source_id,lang,updated_at) VALUES(1,'Foreign','ONGOING','source','ja',now())").execute(&pool).await.unwrap();
    let holder = key_share(
        &pool,
        &[("work", &kept), ("source_series", &mangadex_source)],
    )
    .await;
    let purging = pool.foreground();
    let purged = tokio::time::timeout(Duration::from_secs(3), async move {
        purge_foreign_language_suwayomi(&purging).await
    })
    .await
    .expect("the purge waited for a key-share holder of a row it keeps");
    assert_eq!(purged.unwrap(), vec![1]);
    holder.rollback().await.unwrap();
    let left: Vec<String> =
        sqlx::query_scalar("SELECT id FROM source_series WHERE work_id=$1 ORDER BY id")
            .bind(&kept)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(left, vec![mangadex_source]);
    assert!(!left.contains(&foreign));

    // An orphaned work IS deleted, so its row still takes the delete-capable lock.
    let orphan = work(&pool, "Orphan").await;
    source(&pool, &orphan, "2").await;
    sqlx::query("INSERT INTO suwayomi_series(id,title,status,source_id,lang,updated_at) VALUES(2,'Foreign','ONGOING','source','ja',now())").execute(&pool).await.unwrap();
    let holder = key_share(&pool, &[("work", &orphan)]).await;
    let purging = pool.foreground();
    let blocked = tokio::spawn(async move { purge_foreign_language_suwayomi(&purging).await });
    assert!(
        wait_for_lock(&pool).await,
        "the purge must wait for a key-share holder of a work it deletes"
    );
    holder.rollback().await.unwrap();
    assert_eq!(blocked.await.unwrap().unwrap(), vec![2]);
    let works: Vec<String> = sqlx::query_scalar("SELECT id FROM work")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(works, vec![kept]);
}

/// The weaker lock on a work the purge keeps still excludes a guarded writer of that
/// work: an alias edit waits until the purge has removed the foreign-language mapping.
#[tokio::test]
async fn purge_still_excludes_writers_of_a_work_it_keeps() {
    let pool = pool().await;
    let (kept, mangadex_source) = mirror_fixture(&pool).await;
    source(&pool, &kept, "1").await;
    sqlx::query("INSERT INTO suwayomi_series(id,title,status,source_id,lang,updated_at) VALUES(1,'Foreign','ONGOING','source','ja',now())").execute(&pool).await.unwrap();
    let edited = kept.clone();
    let (purged, aliased) = overlap(
        &pool,
        move |pool| async move { purge_foreign_language_suwayomi(&pool).await },
        move |pool| async move {
            add_work_alias(&pool, &edited, "During Purge")
                .await
                .map(|_| ())
        },
    )
    .await;
    assert_eq!(purged.unwrap(), vec![1]);
    aliased.unwrap();
    let left: Vec<String> =
        sqlx::query_scalar("SELECT id FROM source_series WHERE work_id=$1 ORDER BY id")
            .bind(&kept)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(left, vec![mangadex_source]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM work_alias WHERE work_id=$1 AND raw_title='During Purge'"
        )
        .bind(&kept)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}

/// The full Suwayomi tag pass plans without locks and opens a guarded transaction only
/// for a work that has something to change. A foreign transaction holds the only work's
/// row lock here; a pass that locked every linked work would wait for it.
#[tokio::test]
async fn suwayomi_tag_full_pass_locks_only_works_with_a_change() {
    let pool = pool().await;
    let work = work(&pool, "Tagged").await;
    source(&pool, &work, "1").await;
    sqlx::query("INSERT INTO suwayomi_series(id,title,genre,status,source_id,updated_at) VALUES(1,'T',$1,'ONGOING','source',now())")
        .bind(sqlx::types::Json(serde_json::json!(["Action"])))
        .execute(&pool)
        .await
        .unwrap();
    let first = crate::tags::sync_suwayomi_tags(&pool).await.unwrap();
    assert_eq!(first.inserted, 1);
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM work WHERE id=$1 FOR UPDATE")
        .bind(&work)
        .execute(&mut *holder)
        .await
        .unwrap();
    let syncing = pool.foreground();
    let again = tokio::time::timeout(Duration::from_secs(3), async move {
        crate::tags::sync_suwayomi_tags(&syncing).await
    })
    .await
    .expect("a tag pass with nothing to change waited for a work row lock")
    .unwrap();
    assert_eq!(
        (again.inserted, again.deleted, again.demographics),
        (0, 0, 0)
    );
    holder.rollback().await.unwrap();

    // A work that does have a change is still synced under its work lock.
    sqlx::query("UPDATE suwayomi_series SET genre=$1 WHERE id=1")
        .bind(sqlx::types::Json(serde_json::json!(["Drama"])))
        .execute(&pool)
        .await
        .unwrap();
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM work WHERE id=$1 FOR UPDATE")
        .bind(&work)
        .execute(&mut *holder)
        .await
        .unwrap();
    let syncing = pool.foreground();
    let changing = tokio::spawn(async move { crate::tags::sync_suwayomi_tags(&syncing).await });
    assert!(
        wait_for_lock(&pool).await,
        "a tag pass with a change must take the work lock"
    );
    holder.rollback().await.unwrap();
    let changed = changing.await.unwrap().unwrap();
    assert_eq!((changed.inserted, changed.deleted), (1, 1));
    let tags: Vec<String> =
        sqlx::query_scalar("SELECT tag FROM work_tag WHERE work_id=$1 ORDER BY tag")
            .bind(work)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(tags, vec!["Drama"]);
}

// ── Fix round 1: transaction-taking APIs ─────────────────────────────────────────

async fn work_exists(pool: &Database, id: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM work WHERE id=$1)")
            .bind(id)
            .fetch_one(pool)
            .await?,
    )
}

fn titled(title: &str, author: Option<&str>, external: &[(&str, &str)]) -> WorkInput {
    WorkInput {
        primary_title: Some(title.into()),
        author: author.map(Into::into),
        aliases: vec![Alias {
            raw: title.into(),
            lang: None,
        }],
        external_ids: external
            .iter()
            .map(|(p, e)| ((*p).into(), (*e).into()))
            .collect(),
        ..Default::default()
    }
}

/// Every read helper answers from the caller's transaction (it sees that transaction's
/// uncommitted rows, which the pool form cannot) and leaves the transaction open.
#[tokio::test]
async fn read_helpers_run_on_the_callers_transaction() {
    let pool = pool().await;
    let mut tx = pool.begin().await.unwrap();
    let input = titled("The Lantern", Some("Author"), &[("al", "77")]);
    // The unit's single advisory call: the mint and the source claim further down.
    let mut keys = create_work_keys(&input);
    keys.extend(source_claim_keys("suwayomi", "source", "1"));
    crate::locks::advisory(&mut tx, &keys).await.unwrap();
    let first = create_work_on(&mut tx, &input).await.unwrap();
    let second = create_work_on(&mut tx, &titled("Lantern Road", None, &[]))
        .await
        .unwrap();

    assert_eq!(
        find_work_by_external_on(&mut tx, "al", "77").await.unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        find_work_by_external(&pool, "al", "77").await.unwrap(),
        None
    );

    let norm = normalize::normalize_title("The Lantern");
    assert_eq!(
        find_works_by_alias_on(&mut tx, &norm).await.unwrap(),
        vec![first.clone()]
    );
    assert!(find_works_by_alias(&pool, &norm).await.unwrap().is_empty());

    let key = normalize::match_key(&norm);
    assert_eq!(
        find_works_by_match_key_on(&mut tx, &key).await.unwrap(),
        vec![first.clone()]
    );
    assert!(find_works_by_match_key(&pool, &key)
        .await
        .unwrap()
        .is_empty());

    let mut blocked = candidate_work_ids_by_token_on(&mut tx, "lantern", 10)
        .await
        .unwrap();
    blocked.sort();
    let mut both = vec![first.clone(), second.clone()];
    both.sort();
    assert_eq!(blocked, both);
    assert!(candidate_work_ids_by_token(&pool, "lantern", 10)
        .await
        .unwrap()
        .is_empty());

    let one = load_match_data_on(&mut tx, &first).await.unwrap().unwrap();
    assert_eq!(one.author.as_deref(), Some("Author"));
    assert_eq!(one.aliases_norm, vec![norm.clone()]);
    assert!(load_match_data(&pool, &first).await.unwrap().is_none());

    let batch = load_match_data_batch_on(&mut tx, &both).await.unwrap();
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[&first].aliases_norm, vec![norm]);
    assert!(load_match_data_batch(&pool, &both)
        .await
        .unwrap()
        .is_empty());

    // The richer work survives: only `first` gets a source mapping.
    upsert_source_series_on(&mut tx, &first, "suwayomi", "source", "1", None, false)
        .await
        .unwrap();
    assert_eq!(pick_survivor_on(&mut tx, &both).await.unwrap(), first);

    let (low, high) = (both[0].clone(), both[1].clone());
    assert!(!is_split_pair_on(&mut tx, &first, &second).await.unwrap());
    sqlx::query("INSERT INTO work_split(work_a,work_b,split_at) VALUES($1,$2,now())")
        .bind(&low)
        .bind(&high)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(is_split_pair_on(&mut tx, &second, &first).await.unwrap());
    assert!(!is_split_pair(&pool, &second, &first).await.unwrap());

    // None of them committed or ended the transaction.
    tx.rollback().await.unwrap();
    let rows: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM work), (SELECT COUNT(*) FROM source_series), (SELECT COUNT(*) FROM work_split)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, (0, 0, 0));
}

/// `create_work_on` adopts the owner of a supplied external id instead of minting, and
/// refuses ids that belong to two different works.
#[tokio::test]
async fn create_work_on_adopts_inside_the_callers_transaction() {
    let pool = pool().await;
    let owner = create_work(&pool, &titled("Owner", None, &[("al", "1")]))
        .await
        .unwrap();
    let other = create_work(&pool, &titled("Other", None, &[("mal", "2")]))
        .await
        .unwrap();
    let input = titled("Owner Again", None, &[("al", "1"), ("kt", "9")]);
    let mut tx = pool.begin().await.unwrap();
    crate::locks::advisory(&mut tx, &create_work_keys(&input))
        .await
        .unwrap();
    assert_eq!(create_work_on(&mut tx, &input).await.unwrap(), owner);
    assert_eq!(
        find_work_by_external_on(&mut tx, "kt", "9").await.unwrap(),
        Some(owner.clone())
    );
    assert_eq!(find_work_by_external(&pool, "kt", "9").await.unwrap(), None);
    tx.commit().await.unwrap();
    assert_eq!(
        find_work_by_external(&pool, "kt", "9").await.unwrap(),
        Some(owner)
    );

    let split = titled("Both", None, &[("al", "1"), ("mal", "2")]);
    let mut tx = pool.begin().await.unwrap();
    crate::locks::advisory(&mut tx, &create_work_keys(&split))
        .await
        .unwrap();
    let error = create_work_on(&mut tx, &split).await.unwrap_err();
    assert!(error.to_string().contains("different works"), "{error}");
    drop(tx);
    assert!(work_exists(&pool, &other).await.unwrap());
}

/// One transaction upserts a MangaDex work and stamps both sync markers; nothing is
/// visible before the caller commits and nothing survives the caller's rollback.
#[tokio::test]
async fn mangadex_upsert_and_sync_markers_share_the_callers_transaction() {
    let pool = pool().await;
    let input = titled("Dex", None, &[("al", "5")]);
    let ids = vec!["dex-1".to_owned()];
    for commit in [false, true] {
        let mut tx = pool.begin().await.unwrap();
        crate::locks::advisory(&mut tx, &mangadex_upsert_keys("dex-1", &input))
            .await
            .unwrap();
        assert_eq!(mangadex_owner_on(&mut tx, "dex-1").await.unwrap(), None);
        let work = upsert_work_from_mangadex_on(&mut tx, "dex-1", &input)
            .await
            .unwrap();
        assert_eq!(
            mangadex_owner_on(&mut tx, "dex-1").await.unwrap(),
            Some(work.clone())
        );
        mark_metadata_synced_on(&mut tx, &ids).await.unwrap();
        mark_covers_synced_on(&mut tx, &ids).await.unwrap();
        let stamped: (bool, bool) = sqlx::query_as(
            "SELECT mangadex_details_synced_at IS NOT NULL, covers_synced_at IS NOT NULL FROM work WHERE id=$1",
        )
        .bind(&work)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(stamped, (true, true));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM work")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "the upsert must not be visible before the caller commits"
        );
        if commit {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
    }
    let committed: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM work WHERE mangadex_details_synced_at IS NOT NULL AND covers_synced_at IS NOT NULL), \
                (SELECT COUNT(*) FROM source_series WHERE source_type='mangadex' AND source_key='dex-1'), \
                (SELECT COUNT(*) FROM work_external_id)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(committed, (1, 1, 2));
    // The pool forms still work, one statement for each id.
    mark_metadata_synced(&pool, &ids).await.unwrap();
    mark_covers_synced(&pool, &ids).await.unwrap();
}

/// A source claim and its review candidate in ONE transaction, with one advisory call
/// and one sorted work-lock call, then a stale pair that must restart.
#[tokio::test]
async fn source_claim_and_review_candidate_share_the_callers_transaction() {
    let pool = pool().await;
    let a = work(&pool, "Claim A").await;
    let b = work(&pool, "Claim B").await;
    let mut tx = pool.begin().await.unwrap();
    let mut works = source_claim_works_on(&mut tx, &a, "suwayomi", "source", "1")
        .await
        .unwrap();
    assert_eq!(works, vec![a.clone()]);
    let pair = ReviewPair {
        owner: a.clone(),
        target: b.clone(),
    };
    let mut keys = source_claim_keys("suwayomi", "source", "1");
    keys.push(pair.key());
    crate::locks::advisory(&mut tx, &keys).await.unwrap();
    works.push(b.clone());
    crate::locks::lock_works(&mut tx, &works).await.unwrap();
    let mapping = upsert_source_series_on(&mut tx, &a, "suwayomi", "source", "1", None, false)
        .await
        .unwrap();
    assert_eq!(
        review_pair_on(&mut tx, &mapping, &b).await.unwrap(),
        Some(pair.clone())
    );
    let candidate = insert_merge_candidate_on(&mut tx, &mapping, &pair, 0.7, "test")
        .await
        .unwrap();
    assert!(candidate.is_some());
    assert_eq!(
        insert_merge_candidate_on(&mut tx, &mapping, &pair, 0.9, "test")
            .await
            .unwrap(),
        None,
        "the same pair is suppressed inside the same transaction"
    );
    let visible: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM source_series), (SELECT COUNT(*) FROM merge_candidate)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        visible,
        (0, 0),
        "nothing is visible before the caller commits"
    );
    tx.commit().await.unwrap();
    assert_eq!(
        review_pair_on(&mut pool.acquire().await.unwrap(), "missing", &b)
            .await
            .unwrap(),
        None
    );
    // A second claim of the same key keeps the stored owner, whatever work is proposed.
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        source_claim_works_on(&mut tx, &b, "suwayomi", "source", "1")
            .await
            .unwrap(),
        {
            let mut both = vec![a.clone(), b.clone()];
            both.sort();
            both
        }
    );
    crate::locks::advisory(&mut tx, &source_claim_keys("suwayomi", "source", "1"))
        .await
        .unwrap();
    assert_eq!(
        upsert_source_series_on(&mut tx, &b, "suwayomi", "source", "1", None, false)
            .await
            .unwrap(),
        mapping
    );
    // A pair whose owner is not the mapping's owner is not the pair the caller's key
    // covers: the unit must restart, not insert.
    let stale = ReviewPair {
        owner: b.clone(),
        target: a.clone(),
    };
    let error = insert_merge_candidate_on(&mut tx, &mapping, &stale, 0.7, "test")
        .await
        .unwrap_err();
    assert!(error
        .downcast_ref::<crate::locks::RestartTransaction>()
        .is_some());
    drop(tx);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM merge_candidate")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
}

/// The guarded merge runs inside the caller's transaction: invisible before the commit,
/// undone by a rollback, and it refuses a self merge before it takes any lock.
#[tokio::test]
async fn guarded_merge_runs_inside_the_callers_transaction() {
    let pool = pool().await;
    let loser = work(&pool, "Guarded Loser").await;
    let winner = work(&pool, "Guarded Winner").await;
    let mapping = source(&pool, &loser, "1").await;
    let mut tx = pool.begin().await.unwrap();
    let error = merge_works_guarded_on(&mut tx, &loser, &loser, None, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("into itself"), "{error}");
    drop(tx);
    for commit in [false, true] {
        let mut tx = pool.begin().await.unwrap();
        let outcome = merge_works_guarded_on(&mut tx, &loser, &winner, None, None)
            .await
            .unwrap();
        assert_eq!(outcome.moved_source_series, 1);
        assert!(
            work_exists(&pool, &loser).await.unwrap(),
            "the merge must not be visible before the caller commits"
        );
        if commit {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
            assert_eq!(
                work_id_for_source_series(&pool, &mapping).await.unwrap(),
                Some(loser.clone())
            );
        }
    }
    assert!(!work_exists(&pool, &loser).await.unwrap());
    assert_eq!(
        work_id_for_source_series(&pool, &mapping).await.unwrap(),
        Some(winner)
    );
}

/// Two confirmations of the same pending review: the second waits for the first on the
/// merge guards and then fails with a defined error instead of merging again.
#[tokio::test]
async fn one_pending_review_is_merged_once() {
    let pool = pool().await;
    let loser = work(&pool, "Review Loser").await;
    let winner = work(&pool, "Review Winner").await;
    let mapping = source(&pool, &loser, "1").await;
    source(&pool, &winner, "2").await;
    let candidate = insert_merge_candidate(&pool, &mapping, &winner, 0.7, "test")
        .await
        .unwrap()
        .unwrap();
    let (a_loser, a_winner, a_candidate) = (loser.clone(), winner.clone(), candidate.clone());
    let (b_loser, b_winner, b_candidate) = (loser.clone(), winner.clone(), candidate.clone());
    let (first, second) = overlap(
        &pool,
        move |pool| async move {
            merge_pending_candidate(&pool, None, &a_loser, &a_winner, &a_candidate).await
        },
        move |pool| async move {
            merge_pending_candidate(&pool, None, &b_loser, &b_winner, &b_candidate).await
        },
    )
    .await;
    assert_eq!(first.unwrap().moved_source_series, 1);
    let error = second.unwrap_err().to_string();
    assert!(
        error.contains("no such work") || error.contains("no longer pending"),
        "{error}"
    );
    let state: (i64, i64, String) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM work), (SELECT COUNT(*) FROM source_series WHERE work_id=$1), \
                (SELECT status FROM merge_candidate WHERE id=$2)",
    )
    .bind(&winner)
    .bind(&candidate)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, (1, 2, "confirmed".into()));
}

/// The caller-side unit that T024 and T093 need, built only from the transaction-taking
/// helpers: reserve the match domain, match, mint or reuse, claim the source, commit.
async fn match_or_mint(
    pool: Database,
    title: &str,
    source_id: &str,
    source_key: &str,
) -> Result<String> {
    let cand = crate::dedup::Candidate {
        title: title.into(),
        author: Some("Same Author".into()),
        ..Default::default()
    };
    crate::db::retry_transaction(|| async {
        let mut tx = pool.begin().await?;
        let mut keys = crate::dedup::match_domain_keys(&cand);
        keys.extend(source_claim_keys("suwayomi", source_id, source_key));
        crate::locks::advisory(&mut tx, &keys).await?;
        guards::held().await;
        let matched = match crate::dedup::resolve_on(&mut tx, &cand, None).await? {
            crate::dedup::Decision::AutoMerge { work_id, .. } => Some(work_id),
            _ => None,
        };
        let work = match matched {
            Some(work) => {
                let mut works =
                    source_claim_works_on(&mut tx, &work, "suwayomi", source_id, source_key)
                        .await?;
                works.push(work.clone());
                crate::locks::lock_works(&mut tx, &works).await?;
                // The decision is read again under the work lock before it is used.
                match crate::dedup::resolve_on(&mut tx, &cand, None).await? {
                    crate::dedup::Decision::AutoMerge { work_id, .. } if work_id == work => work,
                    _ => return Err(crate::locks::RestartTransaction.into()),
                }
            }
            None => create_work_on(&mut tx, &titled(title, Some("Same Author"), &[])).await?,
        };
        upsert_source_series_on(
            &mut tx, &work, "suwayomi", source_id, source_key, None, false,
        )
        .await?;
        tx.commit().await?;
        Ok(work)
    })
    .await
}

/// Two adds of the same series under different article spellings, from different
/// sources: without the match-domain key both would decide "new work" and mint twice.
/// The two source claims share no advisory key (different source ids and keys), so the
/// match-domain key is the only thing that can make the second add wait.
#[tokio::test]
async fn match_domain_keys_make_two_title_only_mints_one_work() {
    let pool = pool().await;
    let (first, second) = overlap(
        &pool,
        |pool| match_or_mint(pool, "The Harbour", "source-a", "1"),
        |pool| match_or_mint(pool, "Harbour", "source-b", "2"),
    )
    .await;
    let first = first.unwrap();
    assert_eq!(second.unwrap(), first);
    let rows: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM work), (SELECT COUNT(*) FROM source_series WHERE work_id=$1)",
    )
    .bind(&first)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, (1, 2));
}

// ── Fix round 1: sync checkpoints and reset generations ──────────────────────────

/// A checkpoint that carries the generation of a cycle a reset has cancelled is dropped.
#[tokio::test]
async fn a_checkpoint_from_before_a_reset_is_dropped() {
    let pool = pool().await;
    let stale = sync_generation(&pool, "catalogue").await.unwrap();
    assert_eq!(stale, 0);
    assert!(
        set_seed_progress_at(&pool, "catalogue", "2026-07-11T00:00:00", stale)
            .await
            .unwrap()
    );
    reset_sync_state(&pool, "catalogue").await.unwrap();
    let current = sync_generation(&pool, "catalogue").await.unwrap();
    assert_eq!(current, stale + 1);
    assert_eq!(sync_generation(&pool, "chapters").await.unwrap(), 0);

    for written in [
        set_seed_progress_at(&pool, "catalogue", "2026-07-12T00:00:00", stale).await,
        mark_seed_done_at(&pool, "catalogue", "2026-07-12T00:00:00", stale).await,
        set_sync_cursor_at(&pool, "catalogue", "2026-07-12T00:00:00", stale).await,
    ] {
        assert!(
            !written.unwrap(),
            "a stale checkpoint must report that it was dropped"
        );
        assert!(
            get_sync_state(&pool, "catalogue").await.unwrap().is_none(),
            "a stale checkpoint must not put the reset row back"
        );
    }

    assert!(
        mark_seed_done_at(&pool, "catalogue", "2026-07-13T00:00:00", current)
            .await
            .unwrap()
    );
    assert!(
        set_sync_cursor_at(&pool, "catalogue", "2026-07-14T00:00:00", current)
            .await
            .unwrap()
    );
    let state = get_sync_state(&pool, "catalogue").await.unwrap().unwrap();
    assert_eq!(state.cursor, "2026-07-14T00:00:00");
    assert!(state.seed_done);
    reset_sync_state(&pool, "catalogue").await.unwrap();
    assert_eq!(
        sync_generation(&pool, "catalogue").await.unwrap(),
        current + 1
    );
}

/// A reset and a checkpoint of the cycle it cancels share the job key in both orders,
/// and in both orders the job ends reset.
#[tokio::test]
async fn reset_and_checkpoint_share_the_job_key_in_both_orders() {
    for reset_first in [true, false] {
        let pool = pool().await;
        mark_seed_done(&pool, "catalogue", "2026-07-10T00:00:00")
            .await
            .unwrap();
        let generation = sync_generation(&pool, "catalogue").await.unwrap();
        let reset = |pool: Database| async move {
            reset_sync_state(&pool, "catalogue").await.map(|()| true)
        };
        let checkpoint = move |pool: Database| async move {
            set_sync_cursor_at(&pool, "catalogue", "2026-07-15T00:00:00", generation).await
        };
        let written = if reset_first {
            let (a, b) = overlap(&pool, reset, checkpoint).await;
            a.unwrap();
            b.unwrap()
        } else {
            let (a, b) = overlap(&pool, checkpoint, reset).await;
            b.unwrap();
            a.unwrap()
        };
        assert_eq!(
            written, !reset_first,
            "a checkpoint is written only when it commits before the reset"
        );
        assert!(
            get_sync_state(&pool, "catalogue").await.unwrap().is_none(),
            "reset_first={reset_first}: the job must end reset"
        );
        assert_eq!(
            sync_generation(&pool, "catalogue").await.unwrap(),
            generation + 1
        );
    }
}

/// A source policy cannot replace the more specific decision an admin made for a work.
#[tokio::test]
async fn a_claim_on_a_pinned_source_preserves_an_explicit_safe_work_override() {
    let pool = pool().await;
    let work = work(&pool, "Explicit safe").await;
    set_source_nsfw_pin(&pool, "pinned-source", true)
        .await
        .unwrap();
    sqlx::query("UPDATE work SET is_nsfw_override = FALSE WHERE id = $1")
        .bind(&work)
        .execute(&pool)
        .await
        .unwrap();
    upsert_source_series(&pool, &work, "suwayomi", "pinned-source", "17", None, true)
        .await
        .unwrap();
    let flags: (bool, bool) = sqlx::query_as(
        "SELECT w.is_nsfw_override, b.is_nsfw FROM work w JOIN browse_catalogue b ON b.work_id = w.id WHERE w.id = $1"
    ).bind(&work).fetch_one(&pool).await.unwrap();
    assert_eq!(flags, (false, false));
}
