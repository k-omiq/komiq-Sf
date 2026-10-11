use crate::locks::pause_points::{ArmedPoint, PausePoints};

const ADMIN: &str = "964771b0-a2fb-4849-857a-6198947115ec";
const BOB: &str = "ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c";

/// Failure detector for a step that runs no password hashing.
const STEP: Duration = Duration::from_secs(3);
/// Failure detector for a step in which the writer first verifies or hashes a
/// password. One Argon2id hash (64 MiB, t=3) takes about 3 s in this unoptimised
/// test build and several times that while other tests hash on the same machine.
/// The test continues as soon as the awaited event happens; this is only the bound.
const HASH_STEP: Duration = Duration::from_secs(120);

async fn reached_within(point: &ArmedPoint, bound: Duration) {
    tokio::time::timeout(bound, point.reached())
        .await
        .expect("writer did not reach its guarded transaction");
}

async fn reached(point: &ArmedPoint) {
    reached_within(point, STEP).await;
}

/// The writer hashes or verifies a password before it opens its transaction.
async fn reached_after_hashing(point: &ArmedPoint) {
    reached_within(point, HASH_STEP).await;
}

async fn lock_wait_within(pool: &Database, bound: Duration) {
    use sqlx::ConnectOptions;
    // The observer has its own connection, outside every pool. A writer that hashes
    // on the calling thread (provisioning) keeps its reader connection checked out
    // for the whole hash; an observer that polled through the one-connection reader
    // pool then failed with the pool's own 3 s checkout limit, not with this bound.
    let mut observer = pool
        .connect_options()
        .as_ref()
        .clone()
        .connect()
        .await
        .expect("lock observer could not connect");
    tokio::time::timeout(bound, async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() \
                 AND pid<>pg_backend_pid() AND wait_event_type='Lock')",
            )
            .fetch_one(&mut observer)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second foreground writer never waited on a PostgreSQL lock");
    let _ = sqlx::Connection::close(observer).await;
}

pub(super) async fn lock_wait(pool: &Database) {
    lock_wait_within(pool, STEP).await;
}

/// Start a second writer that hashes a password before the statement that must wait,
/// and return once its hash is done. The writer is parked at the test-only `hashed`
/// point, where it holds no transaction and no lock. The caller then brings the first
/// writer to its in-transaction pause, releases this point and observes the lock wait
/// with the plain `STEP` bound.
///
/// Before this, the second writer hashed AFTER the first writer had paused inside its
/// transaction. One Argon2id hash takes about 3 s in this unoptimised build and many
/// times that on a loaded machine, and PostgreSQL ends a session that sits idle in a
/// transaction for 30 s: the first writer could be terminated by the server while the
/// test was still waiting for the second one to finish hashing.
async fn hashed_first<T: Send + 'static>(
    writer: impl std::future::Future<Output = T> + Send + 'static,
) -> (tokio::task::JoinHandle<T>, ArmedPoint) {
    let points = PausePoints::default();
    let hashed = points.arm("hashed");
    let task = tokio::spawn(GUARD_PAUSE.scope(points, writer));
    reached_within(&hashed, HASH_STEP).await;
    (task, hashed)
}

async fn finish<T>(task: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("writer did not finish after guard release")
        .expect("writer panicked")
}

fn successful(response: async_graphql::Response) {
    assert!(
        response.errors.is_empty(),
        "unexpected GraphQL failure: {:?}",
        response.errors
    );
}

#[tokio::test]
async fn two_sessions_first_owner_has_one_winner() {
    let (schema, pool, state) = setup_without_admin().await;
    let code = state.setup_code.issue();
    let b_query = create_owner_q(&code, "second_owner");
    let b_schema = schema.clone();
    let (b, hashed) =
        hashed_first(async move { exec(&b_schema, &b_query, None, "127.0.0.2").await }).await;
    let points = PausePoints::default();
    let point = points.arm("owner-locked");
    let a_query = create_owner_q(&code, "first_owner");
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(&schema, &a_query, None, "127.0.0.1").await
    }));
    // The writer hashes a password before BEGIN.
    reached_after_hashing(&point).await;
    hashed.release();
    lock_wait(&pool).await;
    point.release();
    successful(finish(a).await);
    let loser = finish(b).await;
    assert_eq!(loser.errors.len(), 1);
    assert_eq!(loser.errors[0].message, "This server is already set up.");
    let admins: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE is_admin")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(admins, 1);
    assert!(!state.setup_code.verify(&code));
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_mutual_demotions_revalidate_authority() {
    let (schema, pool) = setup_full(100).await;
    sqlx::query("UPDATE users SET is_admin=TRUE WHERE id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let points = PausePoints::default();
    let point = points.arm("membership-locked");
    let a_schema = schema.clone();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(
            &a_schema,
            &format!("mutation{{setUserAdmin(userId:\"{BOB}\",isAdmin:false){{id}}}}"),
            Some("admintok"),
            "admin-a",
        )
        .await
    }));
    reached(&point).await;
    let b = tokio::spawn(async move {
        exec(
            &schema,
            &format!("mutation{{setUserAdmin(userId:\"{ADMIN}\",isAdmin:false){{id}}}}"),
            Some("bobtok"),
            "admin-b",
        )
        .await
    });
    lock_wait(&pool).await;
    point.release();
    successful(finish(a).await);
    let loser = finish(b).await;
    assert_eq!(loser.errors.len(), 1);
    assert_eq!(loser.errors[0].message, "Admin access required");
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE is_admin AND NOT is_banned")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 1);
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_ban_and_promotion_have_defined_losers() {
    // Both resolver commit orders: promotion refuses a suspended target;
    // ban refuses an admin. Exercise the public mutations and their once helpers.
    for promote_first in [false, true] {
        let (schema, pool) = setup_full(100).await;
        let points = PausePoints::default();
        let point = points.arm("membership-locked");
        let promotion = format!("mutation{{setUserAdmin(userId:\"{BOB}\",isAdmin:true){{id}}}}");
        let ban = format!("mutation{{banUser(userId:\"{BOB}\",banned:true){{id}}}}");
        let a_schema = schema.clone();
        let a_query = if promote_first {
            promotion.clone()
        } else {
            ban.clone()
        };
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            exec(&a_schema, &a_query, Some("admintok"), "role-a").await
        }));
        reached(&point).await;
        let b_query = if promote_first { ban } else { promotion };
        let b =
            tokio::spawn(async move { exec(&schema, &b_query, Some("admintok"), "role-b").await });
        lock_wait(&pool).await;
        point.release();
        successful(finish(a).await);
        let loser = finish(b).await;
        assert_eq!(loser.errors.len(), 1, "conflicting role change succeeded");
        assert_eq!(
            loser.errors[0].message,
            if promote_first {
                "You cannot ban an admin account."
            } else {
                "You cannot promote a suspended account."
            }
        );
        let row: (bool, bool) = sqlx::query_as("SELECT is_admin,is_banned FROM users WHERE id=$1")
            .bind(crate::db_types::uuid(BOB).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row, (promote_first, !promote_first));
        pool.close().await;
    }
}

#[tokio::test]
async fn two_sessions_login_and_ban_share_the_user_guard() {
    for ban_first in [false, true] {
        let (_, pool) = setup_full(100).await;
        let points = PausePoints::default();
        let point = points.arm(if ban_first {
            "membership-locked"
        } else {
            "session-locked"
        });
        let a_pool = pool.foreground();
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            if ban_first {
                ban_user_once(&a_pool, ADMIN, BOB, true).await.map(|_| ())
            } else {
                new_session(&a_pool, BOB, 3600)
                    .await
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!("{}", e.message))
            }
        }));
        reached(&point).await;
        let b_pool = pool.foreground();
        let b = tokio::spawn(async move {
            if ban_first {
                new_session(&b_pool, BOB, 3600)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.message)
            } else {
                ban_user_once(&b_pool, ADMIN, BOB, true)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        });
        lock_wait(&pool).await;
        point.release();
        assert!(finish(a).await.is_ok());
        let result = finish(b).await;
        if ban_first {
            assert_eq!(result.unwrap_err(), "This account has been suspended.");
        } else {
            result.unwrap();
        }
        let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id=$1")
            .bind(crate::db_types::uuid(BOB).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sessions, 0, "a post-ban session survived revocation");
        pool.close().await;
    }
}

#[tokio::test]
async fn two_sessions_password_change_rejects_old_verified_hash() {
    let (schema, pool) = setup_full(100).await;
    let old: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    let points = PausePoints::default();
    let point = points.arm("password-locked");
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(&schema, "mutation{changePassword(currentPassword:\"password123\",newPassword:\"new-password-123\"){id}}", Some("bobtok"), "change").await
    }));
    // changePassword verifies the old password and hashes the new one before BEGIN.
    reached_after_hashing(&point).await;
    let b_pool = pool.foreground();
    let b = tokio::spawn(async move { new_session_checked(&b_pool, BOB, 3600, Some(&old)).await });
    lock_wait(&pool).await;
    point.release();
    successful(finish(a).await);
    assert_eq!(
        finish(b).await.unwrap_err().message,
        "Invalid username or password"
    );
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 1, "the current device alone must remain");
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_reset_revokes_a_waiting_login() {
    let (schema, pool) = setup_full(100).await;
    // resetUserPassword hashes the new password before BEGIN.
    let (b, hashed) = hashed_first(async move {
        exec(
            &schema,
            &format!(
                "mutation{{resetUserPassword(userId:\"{BOB}\",password:\"reset-password-123\")}}"
            ),
            Some("admintok"),
            "reset",
        )
        .await
    })
    .await;
    let points = PausePoints::default();
    let point = points.arm("session-locked");
    let a_pool = pool.foreground();
    let a = tokio::spawn(
        GUARD_PAUSE.scope(points, async move { new_session(&a_pool, BOB, 3600).await }),
    );
    reached(&point).await;
    hashed.release();
    lock_wait(&pool).await;
    point.release();
    finish(a).await.unwrap();
    successful(finish(b).await);
    let row: (bool, i64) = sqlx::query_as("SELECT must_change_password,(SELECT count(*) FROM sessions WHERE user_id=$1) FROM users WHERE id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(row, (true, 0));
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_revocation_orders_new_session_creation() {
    let (schema, pool) = setup_full(100).await;
    let points = PausePoints::default();
    let point = points.arm("revoke-locked");
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(
            &schema,
            "mutation{revokeAllSessions}",
            Some("bobtok"),
            "revoke",
        )
        .await
    }));
    reached(&point).await;
    let b_pool = pool.foreground();
    let b = tokio::spawn(async move { new_session(&b_pool, BOB, 3600).await });
    lock_wait(&pool).await;
    point.release();
    successful(finish(a).await);
    let token = finish(b).await.unwrap();
    let tokens: Vec<String> =
        sqlx::query_scalar("SELECT token FROM sessions WHERE user_id=$1 ORDER BY token")
            .bind(crate::db_types::uuid(BOB).unwrap())
            .fetch_all(&pool)
            .await
            .unwrap();
    let mut expected = vec![auth::hash_token("bobtok"), auth::hash_token(&token)];
    expected.sort();
    assert_eq!(tokens, expected);
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_provisioning_and_login_preserve_admin_session_policy() {
    let (_, pool) = setup_full(100).await;
    let points = PausePoints::default();
    let point = points.arm("provision-locked");
    let a_pool = pool.foreground();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        provision_admins(&a_pool, &["bob".into()], Some("password123"), None).await
    }));
    // Promotion verifies the configured password against the stored hash before BEGIN.
    reached_after_hashing(&point).await;
    let b_pool = pool.foreground();
    let b = tokio::spawn(async move { new_session(&b_pool, BOB, 30 * 24 * 3600).await });
    lock_wait(&pool).await;
    point.release();
    finish(a).await.unwrap();
    finish(b).await.unwrap();
    let row: (i64, bool) = sqlx::query_as("SELECT count(*),bool_and(expires_at-created_at<=interval '8 hours') FROM sessions WHERE user_id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(row, (1, true));
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_partial_overrides_preserve_omitted_fields() {
    let (schema, pool) = setup_full(100).await;
    for hide_first in [true, false] {
        sqlx::query("DELETE FROM chapter_override WHERE work_id='w_s1'")
            .execute(&pool)
            .await
            .unwrap();
        let points = PausePoints::default();
        let point = points.arm("override-locked");
        let hide =
            "mutation{setChapterOverride(input:{workId:\"w_s1\",chapterKey:\"100\",hidden:true})}";
        let title = "mutation{setChapterOverride(input:{workId:\"w_s1\",chapterKey:\"100\",title:\"Renamed\"})}";
        let a_schema = schema.clone();
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            exec(
                &a_schema,
                if hide_first { hide } else { title },
                Some("admintok"),
                "override-a",
            )
            .await
        }));
        reached(&point).await;
        let b_schema = schema.clone();
        let b = tokio::spawn(async move {
            exec(
                &b_schema,
                if hide_first { title } else { hide },
                Some("admintok"),
                "override-b",
            )
            .await
        });
        lock_wait(&pool).await;
        point.release();
        successful(finish(a).await);
        successful(finish(b).await);
        let row: (bool, Option<String>) = sqlx::query_as("SELECT hidden,title_override FROM chapter_override WHERE work_id='w_s1' AND chapter_key='100'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(row, (true, Some("Renamed".into())));
    }
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_library_edits_preserve_fields_and_identity() {
    let (schema, pool) = setup_full(100).await;
    sqlx::query("INSERT INTO user_library(user_id,series_id,created_at) VALUES($1,'s1','2020-01-01T00:00:00Z')")
        .bind(crate::db_types::uuid(BOB).unwrap()).execute(&pool).await.unwrap();
    for (first, second) in [
        (
            "setFavorite(seriesId:\"w_s1\",favorite:true)",
            "setLibraryStatus(seriesId:\"w_s1\",status:\"reading\")",
        ),
        (
            "mark(seriesId:\"w_s1\",marked:true)",
            "setFavorite(seriesId:\"w_s1\",favorite:true)",
        ),
    ] {
        let points = PausePoints::default();
        let point = points.arm("library-locked");
        let a_schema = schema.clone();
        let a_query = format!("mutation{{{first}{{id}}}}");
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            exec(&a_schema, &a_query, Some("bobtok"), "library-a").await
        }));
        reached(&point).await;
        let b_schema = schema.clone();
        let b_query = format!("mutation{{{second}{{id}}}}");
        let b =
            tokio::spawn(
                async move { exec(&b_schema, &b_query, Some("bobtok"), "library-b").await },
            );
        lock_wait(&pool).await;
        point.release();
        successful(finish(a).await);
        successful(finish(b).await);
    }
    let rows: Vec<(String, bool, Option<String>)> =
        sqlx::query_as("SELECT series_id,is_favorite,status FROM user_library WHERE user_id=$1")
            .bind(crate::db_types::uuid(BOB).unwrap())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![("w_s1".into(), true, Some("reading".into()))]);
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_consolidation_cannot_overwrite_a_library_edit() {
    let (_, pool) = setup_full(100).await;
    sqlx::query("INSERT INTO user_library(user_id,series_id,created_at) VALUES($1,'s1','2020-01-01T00:00:00Z')")
        .bind(crate::db_types::uuid(BOB).unwrap()).execute(&pool).await.unwrap();
    let points = PausePoints::default();
    let point = points.arm("library-locked");
    let a_pool = pool.foreground();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        consolidate_library_identity(&a_pool, BOB, "s1").await
    }));
    reached(&point).await;
    let b_pool = pool.foreground();
    let b = tokio::spawn(async move {
        write_library_once(&b_pool, BOB, "w_s1", LibraryEdit::Favorite(true)).await
    });
    lock_wait(&pool).await;
    point.release();
    assert_eq!(finish(a).await.unwrap(), "w_s1");
    finish(b).await.unwrap();
    let rows: Vec<(String, bool)> =
        sqlx::query_as("SELECT series_id,is_favorite FROM user_library WHERE user_id=$1")
            .bind(crate::db_types::uuid(BOB).unwrap())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![("w_s1".into(), true)]);
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_progress_restarts_after_source_owner_discovery_changes() {
    let (schema, pool) = setup_full(100).await;
    seed_canonical(&pool, "phase4-progress", "Source", false, "1").await;
    let old = work_id_of(&pool, "phase4-progress").await;
    let points = PausePoints::default();
    let point = points.arm("source-discovered");
    let a = tokio::spawn(
        crate::locks::SOURCE_DISCOVERY_PAUSE.scope(points, async move {
            exec(
                &schema,
                "mutation{setProgress(chapterId:\"phase4-progress-1\",lastPageRead:7,read:false)}",
                Some("bobtok"),
                "progress-a",
            )
            .await
        }),
    );
    reached(&point).await;
    // A has discovered the old work but holds no work or source lock yet. The real
    // catalogue merge retires that work while A's transaction is open.
    catalog::merge_works(&pool, &old, "w_s2").await.unwrap();
    point.release();
    successful(finish(a).await);
    let row: (String, i64) = sqlx::query_as("SELECT work_id,last_page_read FROM canonical_progress WHERE user_id=$1 AND chapter_id='phase4-progress-1'")
        .bind(crate::db_types::uuid(BOB).unwrap()).fetch_one(&pool).await.unwrap();
    assert_eq!(row, ("w_s2".into(), 7));
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_catch_up_and_unread_keep_the_last_committed_state() {
    let (schema, pool) = setup_full(100).await;
    seed_canonical(&pool, "phase4-catch", "Catch", false, "1").await;
    seed_canonical(&pool, "phase4-catch", "Catch", false, "2").await;
    for unread_first in [false, true] {
        let points = PausePoints::default();
        let point = points.arm("progress-locked");
        let complete =
            "mutation{setProgress(chapterId:\"phase4-catch-2\",lastPageRead:8,read:true)}";
        let clear = "mutation{setChapterRead(chapterId:\"phase4-catch-1\",read:false)}";
        let a_schema = schema.clone();
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            exec(
                &a_schema,
                if unread_first { clear } else { complete },
                Some("bobtok"),
                "catch-a",
            )
            .await
        }));
        reached(&point).await;
        let b_schema = schema.clone();
        let b = tokio::spawn(async move {
            exec(
                &b_schema,
                if unread_first { complete } else { clear },
                Some("bobtok"),
                "catch-b",
            )
            .await
        });
        lock_wait(&pool).await;
        point.release();
        successful(finish(a).await);
        successful(finish(b).await);
        let earlier: bool = sqlx::query_scalar(
            "SELECT read FROM canonical_progress WHERE user_id=$1 AND chapter_id='phase4-catch-1'",
        )
        .bind(crate::db_types::uuid(BOB).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(earlier, unread_first);
        let page: i64 = sqlx::query_scalar("SELECT last_page_read FROM canonical_progress WHERE user_id=$1 AND chapter_id='phase4-catch-2'")
            .bind(crate::db_types::uuid(BOB).unwrap()).fetch_one(&pool).await.unwrap();
        assert_eq!(page, 8);
    }
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_notification_ids_and_mark_all_share_sorted_order() {
    let (schema, pool) = setup_full(100).await;
    let ids = [uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2)];
    for all in [false, true] {
        sqlx::query("DELETE FROM notifications")
            .execute(&pool)
            .await
            .unwrap();
        for id in ids {
            sqlx::query(
                "INSERT INTO notifications(id,user_id,kind,created_at) VALUES($1,$2,'reply',now())",
            )
            .bind(id)
            .bind(crate::db_types::uuid(BOB).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        }
        let points = PausePoints::default();
        let point = points.arm("notification-locked");
        let a_schema = schema.clone();
        let a_query = format!(
            "mutation{{markNotificationsRead(ids:[\"{}\",\"{}\",\"{}\"])}}",
            ids[1], ids[0], ids[1]
        );
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            exec(&a_schema, &a_query, Some("bobtok"), "notify-a").await
        }));
        reached(&point).await;
        // The second writer runs one attempt without the retry wrapper. A deadlock
        // from two different lock orders must show in its own result; the public
        // mutation would retry it away.
        let b_pool = pool.foreground();
        let b_ids = (!all).then(|| vec![ids[0], ids[1]]);
        let b = tokio::spawn(async move {
            let now = crate::db_types::format_date(Utc::now());
            mark_notifications_read_once(&b_pool, BOB, b_ids.as_deref(), &now).await
        });
        lock_wait(&pool).await;
        point.release();
        let first = finish(a).await;
        let second = finish(b).await;
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert_eq!(first.data.into_json().unwrap()["markNotificationsRead"], 2);
        assert_eq!(
            second.expect("second writer met a lock error instead of waiting in order"),
            0
        );
        let read: i64 =
            sqlx::query_scalar("SELECT count(*) FROM notifications WHERE read_at IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(read, 2);
    }
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_reviews_and_withdrawal_commit_together() {
    let (schema, pool) = setup_full(100).await;
    for delete_first in [false, true] {
        successful(
            exec(
                &schema,
                "mutation{postReview(input:{seriesId:\"w_s1\",score:6,body:\"old\",hasSpoiler:false}){id}}",
                Some("bobtok"),
                "seed-review",
            )
            .await,
        );
        let post = "mutation{postReview(input:{seriesId:\"w_s1\",score:8,body:\"new\",hasSpoiler:false}){id}}";
        let delete = "mutation{deleteReview(seriesId:\"w_s1\")}";
        let points = PausePoints::default();
        let point = points.arm("review-locked");
        let a_schema = schema.clone();
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            exec(
                &a_schema,
                if delete_first { delete } else { post },
                Some("bobtok"),
                "review-a",
            )
            .await
        }));
        reached(&point).await;
        let b_schema = schema.clone();
        let b = tokio::spawn(async move {
            exec(
                &b_schema,
                if delete_first { post } else { delete },
                Some("bobtok"),
                "review-b",
            )
            .await
        });
        lock_wait(&pool).await;
        point.release();
        successful(finish(a).await);
        successful(finish(b).await);
        let row: Option<i64> =
            sqlx::query_scalar("SELECT score FROM reviews WHERE user_id=$1 AND series_id='w_s1'")
                .bind(crate::db_types::uuid(BOB).unwrap())
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(row, delete_first.then_some(8));
        let activity: i64 = sqlx::query_scalar("SELECT count(*) FROM user_activity WHERE user_id=$1 AND kind='review' AND target_id='w_s1'")
            .bind(crate::db_types::uuid(BOB).unwrap()).fetch_one(&pool).await.unwrap();
        assert_eq!(activity, i64::from(delete_first));
    }
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_metadata_partial_edits_project_current_inputs() {
    let (schema, pool) = setup_full(100).await;
    let points = PausePoints::default();
    let point = points.arm("metadata-locked");
    let a_schema = schema.clone();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(&a_schema, "mutation{updateSeriesMetadata(input:{seriesId:\"w_s1\",title:\"Phase Four Title\",isNsfw:true}){id}}", Some("admintok"), "meta-a").await
    }));
    reached(&point).await;
    let b = tokio::spawn(async move {
        exec(&schema, "mutation{updateSeriesMetadata(input:{seriesId:\"w_s1\",author:\"Pinned Author\",tags:[\"Action\"]}){id}}", Some("admintok"), "meta-b").await
    });
    lock_wait(&pool).await;
    point.release();
    successful(finish(a).await);
    successful(finish(b).await);
    let row: (Option<String>, Option<String>, bool) =
        sqlx::query_as("SELECT title_override,author,is_nsfw_override FROM work WHERE id='w_s1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        row,
        (
            Some("Phase Four Title".into()),
            Some("Pinned Author".into()),
            true
        )
    );
    let copied: bool =
        sqlx::query_scalar("SELECT is_nsfw FROM browse_catalogue WHERE work_id='w_s1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(copied);
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_numeric_catch_up_and_unread_share_the_user_guard() {
    let (schema, pool) = setup_full(100).await;
    // Series 42 is catalogued: its work and source rows are locked as well. Series 43
    // is only in the engine cache, so no source row exists and the user row is the
    // one lock both writers share.
    seed_target_series(&pool, "42").await;
    sqlx::query(
        "INSERT INTO suwayomi_series (id, title, status, source_id, chapter_count, updated_at) \
         VALUES (43, 'Uncatalogued 43', 'ONGOING', 'src', 0, '2020-01-01T00:00:00Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO suwayomi_chapter(id,manga_id,name,chapter_number,updated_at) VALUES(420,42,'one',1,now()),(421,42,'two',2,now()),(430,43,'one',1,now()),(431,43,'two',2,now())")
        .execute(&pool).await.unwrap();
    for series in ["42", "43"] {
        let sources: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM source_series WHERE source_type='suwayomi' AND source_key=$1",
        )
        .bind(series)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sources, i64::from(series == "42"));
        let points = PausePoints::default();
        let point = points.arm("progress-locked");
        let a_schema = schema.clone();
        let a_query =
            format!("mutation{{setProgress(chapterId:\"{series}1\",lastPageRead:9,read:true)}}");
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            exec(&a_schema, &a_query, Some("bobtok"), "numeric-a").await
        }));
        reached(&point).await;
        let b_schema = schema.clone();
        let b_query = format!("mutation{{setChapterRead(chapterId:\"{series}0\",read:false)}}");
        let b =
            tokio::spawn(
                async move { exec(&b_schema, &b_query, Some("bobtok"), "numeric-b").await },
            );
        lock_wait(&pool).await;
        point.release();
        successful(finish(a).await);
        successful(finish(b).await);
        let rows: Vec<(String, bool, i64)> = sqlx::query_as("SELECT chapter_id,read,last_page_read FROM suwayomi_progress WHERE user_id=$1 AND series_id=$2 ORDER BY chapter_id")
            .bind(crate::db_types::uuid(BOB).unwrap()).bind(series).fetch_all(&pool).await.unwrap();
        assert_eq!(
            rows,
            vec![
                (format!("{series}0"), false, 0),
                (format!("{series}1"), true, 9)
            ],
            "series {series}"
        );
    }
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_nsfw_projection_rechecks_locked_work() {
    let (schema, pool) = setup_full(100).await;
    successful(
        exec(
            &schema,
            "mutation{updateSeriesMetadata(input:{seriesId:\"w_s1\",isNsfw:true}){id}}",
            Some("admintok"),
            "nsfw-seed",
        )
        .await,
    );
    let points = PausePoints::default();
    let point = points.arm("nsfw-locked");
    let committed = points.arm("nsfw-committed");
    let a_pool = pool.foreground();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        resync_feed_nsfw(&a_pool, &["w_s1".into()]).await;
    }));
    reached(&point).await;
    let b = tokio::spawn(async move {
        exec(
            &schema,
            "mutation{updateSeriesMetadata(input:{seriesId:\"w_s1\",isNsfw:false}){id}}",
            Some("admintok"),
            "nsfw-b",
        )
        .await
    });
    lock_wait(&pool).await;
    point.release();
    reached(&committed).await;
    committed.release();
    finish(a).await;
    successful(finish(b).await);
    let consistent: bool = sqlx::query_scalar("SELECT b.is_nsfw=COALESCE(w.is_nsfw_override,w.is_nsfw) AND b.is_nsfw=FALSE FROM browse_catalogue b JOIN work w ON w.id=b.work_id WHERE w.id='w_s1'")
        .fetch_one(&pool).await.unwrap();
    assert!(consistent);
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_nsfw_backfill_respects_a_new_safe_rating() {
    let (schema, pool) = setup_full(100).await;
    sqlx::query("INSERT INTO source_extension(source_id,pkg_name,repo_url,is_nsfw,updated_at) VALUES('src','phase4','local',TRUE,now())")
        .execute(&pool).await.unwrap();
    let points = PausePoints::default();
    let point = points.arm("metadata-locked");
    let a_schema = schema.clone();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(
            &a_schema,
            "mutation{updateSeriesMetadata(input:{seriesId:\"w_s1\",contentRating:SAFE}){id}}",
            Some("admintok"),
            "safe-a",
        )
        .await
    }));
    reached(&point).await;
    let b = tokio::spawn(async move {
        exec(
            &schema,
            "mutation{rederiveSuwayomiNsfw}",
            Some("admintok"),
            "safe-b",
        )
        .await
    });
    lock_wait(&pool).await;
    point.release();
    successful(finish(a).await);
    successful(finish(b).await);
    let row: (bool, String) =
        sqlx::query_as("SELECT is_nsfw,content_rating FROM work WHERE id='w_s1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, (false, "safe".into()));
    pool.close().await;
}

async fn seed_thread(pool: &Database) -> (uuid::Uuid, uuid::Uuid) {
    let root = uuid::Uuid::new_v4();
    let reply = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO comments(id,user_id,target_type,target_id,body,created_at) VALUES($1,$2,'series','w_s1','root',now())")
        .bind(root).bind(crate::db_types::uuid(ADMIN).unwrap()).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO comments(id,user_id,target_type,target_id,parent_id,body,created_at) VALUES($1,$2,'series','w_s1',$3,'reply',now())")
        .bind(reply).bind(crate::db_types::uuid(BOB).unwrap()).bind(root).execute(pool).await.unwrap();
    (root, reply)
}

#[tokio::test]
async fn two_sessions_replies_to_different_parents_share_the_thread_cap() {
    let (schema, pool) = setup_full(100).await;
    let (root, reply) = seed_thread(&pool).await;
    sqlx::query("INSERT INTO comments(id,user_id,target_type,target_id,parent_id,body,created_at) SELECT gen_random_uuid(),$1,'series','w_s1',$2,'filler',now() FROM generate_series(1,$3::bigint)")
        .bind(crate::db_types::uuid(BOB).unwrap()).bind(root).bind(MAX_THREAD_REPLIES-2).execute(&pool).await.unwrap();
    let points = PausePoints::default();
    let point = points.arm("thread-locked");
    let a_schema = schema.clone();
    let a_query = format!("mutation{{postComment(input:{{targetType:\"series\",targetId:\"w_s1\",parentId:\"{root}\",body:\"last slot\",hasSpoiler:false}}){{id}}}}");
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(&a_schema, &a_query, Some("bobtok"), "thread-a").await
    }));
    reached(&point).await;
    let b_query = format!("mutation{{postComment(input:{{targetType:\"series\",targetId:\"w_s1\",parentId:\"{reply}\",body:\"overflow\",hasSpoiler:false}}){{id}}}}");
    let b =
        tokio::spawn(async move { exec(&schema, &b_query, Some("admintok"), "thread-b").await });
    lock_wait(&pool).await;
    point.release();
    successful(finish(a).await);
    let loser = finish(b).await;
    assert_eq!(loser.errors.len(), 1);
    assert_eq!(
        loser.errors[0].message,
        "This thread is full. Post a new comment instead of a reply."
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM comments WHERE target_id='w_s1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, MAX_THREAD_REPLIES + 1);
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_delete_and_reply_preserve_tombstone_children() {
    let (schema, pool) = setup_full(100).await;
    let (root, reply) = seed_thread(&pool).await;
    let points = PausePoints::default();
    let point = points.arm("thread-locked");
    let a_schema = schema.clone();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(
            &a_schema,
            &format!("mutation{{deleteComment(commentId:\"{root}\")}}"),
            Some("admintok"),
            "delete-root",
        )
        .await
    }));
    reached(&point).await;
    // A different account replies: the deleting admin holds its own user row, so
    // only the shared Thread(root) key can make this writer wait.
    let b = tokio::spawn(async move {
        exec(&schema, &format!("mutation{{postComment(input:{{targetType:\"series\",targetId:\"w_s1\",parentId:\"{reply}\",body:\"surviving grandchild\",hasSpoiler:false}}){{id}}}}"), Some("bobtok"), "reply-child").await
    });
    lock_wait(&pool).await;
    point.release();
    successful(finish(a).await);
    successful(finish(b).await);
    let state: (String, String) =
        sqlx::query_as("SELECT body,deleted_by FROM comments WHERE id=$1")
            .bind(root)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, (String::new(), "author".into()));
    let children: i64 = sqlx::query_scalar("SELECT count(*) FROM comments WHERE parent_id=$1")
        .bind(reply)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(children, 1);
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_votes_and_deletion_leave_no_vote_or_notification_orphans() {
    let (schema, pool) = setup_full(100).await;
    let (root, _) = seed_thread(&pool).await;
    let points = PausePoints::default();
    let point = points.arm("thread-locked");
    let a_schema = schema.clone();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(
            &a_schema,
            &format!(
                "mutation{{voteComment(commentId:\"{root}\",value:1){{likes dislikes myVote}}}}"
            ),
            Some("bobtok"),
            "vote",
        )
        .await
    }));
    reached(&point).await;
    let b = tokio::spawn(async move {
        exec(
            &schema,
            &format!("mutation{{deleteComment(commentId:\"{root}\")}}"),
            Some("admintok"),
            "delete-voted",
        )
        .await
    });
    lock_wait(&pool).await;
    point.release();
    successful(finish(a).await);
    successful(finish(b).await);
    let row: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM comment_votes WHERE comment_id=$1),(SELECT count(*) FROM notifications WHERE comment_id=$1)")
        .bind(root).fetch_one(&pool).await.unwrap();
    assert_eq!(row, (0, 0));
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_noop_review_acceptance_and_rejection_have_one_winner() {
    let (schema, pool) = setup_full(100).await;
    for accept_first in [false, true] {
        sqlx::query("DELETE FROM merge_candidate")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO merge_candidate(id,source_series_id,candidate_work_id,score,method,status,created_at) VALUES('phase4-review','ss_s1','w_s1',0.8,'title_exact','pending',now())")
            .execute(&pool).await.unwrap();
        let points = PausePoints::default();
        let point = points.arm("candidate-locked");
        let a_schema = schema.clone();
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            exec(
                &a_schema,
                &format!(
                    "mutation{{resolveMergeCandidate(id:\"phase4-review\",accept:{accept_first})}}"
                ),
                Some("admintok"),
                "candidate-a",
            )
            .await
        }));
        reached(&point).await;
        let b_schema = schema.clone();
        let b = tokio::spawn(async move {
            exec(
                &b_schema,
                &format!(
                    "mutation{{resolveMergeCandidate(id:\"phase4-review\",accept:{})}}",
                    !accept_first
                ),
                Some("admintok"),
                "candidate-b",
            )
            .await
        });
        lock_wait(&pool).await;
        point.release();
        successful(finish(a).await);
        let loser = finish(b).await;
        assert_eq!(loser.errors.len(), 1);
        assert_eq!(
            loser.errors[0].message,
            "This merge candidate is already resolved."
        );
        let state: String =
            sqlx::query_scalar("SELECT status FROM merge_candidate WHERE id='phase4-review'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            state,
            if accept_first {
                "confirmed"
            } else {
                "rejected"
            }
        );
    }
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_mangadex_setting_and_memory_publish_in_the_same_order() {
    let (schema, pool, state) = setup_with_state(100).await;
    let points = PausePoints::default();
    let persisted = points.arm("mangadex-persisted");
    let a_schema = schema.clone();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(
            &a_schema,
            "mutation{setMangadexEnabled(enabled:false)}",
            Some("admintok"),
            "switch-a",
        )
        .await
    }));
    reached(&persisted).await;
    let other = PausePoints::default();
    let waiting = other.arm("mangadex-waiting");
    let b = tokio::spawn(GUARD_PAUSE.scope(other, async move {
        exec(
            &schema,
            "mutation{setMangadexEnabled(enabled:true)}",
            Some("admintok"),
            "switch-b",
        )
        .await
    }));
    // The hook fires only after the second request's actual mutex lock future
    // returned Pending. This proves local publication overlap without a sleep.
    reached(&waiting).await;
    waiting.release();
    persisted.release();
    successful(finish(a).await);
    successful(finish(b).await);
    let stored: String =
        sqlx::query_scalar("SELECT value FROM instance_setting WHERE key='mangadex_enabled'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, "true");
    assert!(state.mangadex.is_enabled());
    pool.close().await;
}

/// Retire `w_s1` into `w_s2` with the real catalogue merge.
async fn move_fixture_work(pool: &Database) {
    catalog::merge_works(pool, "w_s1", "w_s2").await.unwrap();
}

#[tokio::test]
async fn two_sessions_library_reresolves_a_retired_work_after_discovery() {
    let (schema, pool) = setup_full(100).await;
    let points = PausePoints::default();
    let point = points.arm("library-discovered");
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(
            &schema,
            "mutation{setFavorite(seriesId:\"w_s1\",favorite:true){id}}",
            Some("bobtok"),
            "retired-library",
        )
        .await
    }));
    reached(&point).await;
    move_fixture_work(&pool).await;
    point.release();
    successful(finish(a).await);
    let rows: Vec<(String, bool)> =
        sqlx::query_as("SELECT series_id,is_favorite FROM user_library WHERE user_id=$1")
            .bind(crate::db_types::uuid(BOB).unwrap())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![("w_s2".into(), true)]);
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_comment_post_reresolves_a_retired_work_after_discovery() {
    let (schema, pool) = setup_full(100).await;
    let points = PausePoints::default();
    let point = points.arm("work-discovered");
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(&schema, "mutation{postComment(input:{targetType:\"series\",targetId:\"w_s1\",body:\"surviving thread\",hasSpoiler:false}){id targetId}}", Some("bobtok"), "retired-comment").await
    }));
    reached(&point).await;
    move_fixture_work(&pool).await;
    point.release();
    successful(finish(a).await);
    let rows: Vec<String> = sqlx::query_scalar("SELECT target_id FROM comments WHERE user_id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows, vec!["w_s2"]);
    pool.close().await;
}

#[tokio::test]
async fn invalid_notification_ids_do_not_expand_to_mark_all() {
    let (schema, pool) = setup_full(100).await;
    sqlx::query(
        "INSERT INTO notifications(id,user_id,kind,created_at) VALUES($1,$2,'reply',now())",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(crate::db_types::uuid(BOB).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let response = exec(
        &schema,
        "mutation{markNotificationsRead(ids:[\"invalid\"])}",
        Some("bobtok"),
        "bad-notification",
    )
    .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(
        response.data.into_json().unwrap()["markNotificationsRead"],
        0
    );
    let unread: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notifications WHERE read_at IS NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(unread, 1);
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_ascii_account_constraints_reject_folded_collisions() {
    for username_collision in [true, false] {
        let (schema, pool) = setup_full(100).await;
        crate::instance::store(&pool, &crate::instance::REGISTRATION_OPEN, "true")
            .await
            .unwrap();
        let query = if username_collision {
            "mutation{register(input:{username:\"raceaccount\",email:\"different@example.com\",password:\"password123\"}){token}}"
        } else {
            "mutation{createUser(input:{username:\"differentuser\",email:\"raceemail@EXAMPLE.COM\",password:\"password123\"}){id}}"
        };
        // register and createUser hash the password before their INSERT. The hash is
        // done before the first writer opens its transaction.
        let (second, hashed) = hashed_first(async move {
            exec(
                &schema,
                query,
                (!username_collision).then_some("admintok"),
                "folded-account",
            )
            .await
        })
        .await;
        let foreground = pool.foreground();
        let mut first = foreground.begin().await.unwrap();
        sqlx::query("INSERT INTO users(id,username,email,password_hash,is_admin,created_at) VALUES($1,'RaceAccount','RaceEmail@example.com','unused',FALSE,now())")
            .bind(uuid::Uuid::new_v4()).execute(&mut *first).await.unwrap();
        // The INSERT waits on the uncommitted row behind the ASCII-folded unique index.
        hashed.release();
        lock_wait(&pool).await;
        first.commit().await.unwrap();
        let response = finish(second).await;
        assert_eq!(response.errors.len(), 1);
        assert_eq!(
            response.errors[0].message,
            "username or email already taken"
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username IN ('RaceAccount','raceaccount','differentuser')")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(rows, 1);
        pool.close().await;
    }
}

#[tokio::test]
async fn two_sessions_tombstone_pruning_and_mark_all_share_notification_order() {
    let (schema, pool) = setup_full(100).await;
    let (root, reply) = seed_thread(&pool).await;
    sqlx::query("UPDATE comments SET body='',deleted_by='author' WHERE id=$1")
        .bind(root)
        .execute(&pool)
        .await
        .unwrap();
    for (notification, comment) in [
        (uuid::Uuid::from_u128(1), root),
        (uuid::Uuid::from_u128(2), reply),
    ] {
        sqlx::query("INSERT INTO notifications(id,user_id,kind,comment_id,created_at) VALUES($1,$2,'reply',$3,now())")
            .bind(notification).bind(crate::db_types::uuid(BOB).unwrap()).bind(comment).execute(&pool).await.unwrap();
    }
    let points = PausePoints::default();
    let point = points.arm("notification-locked");
    let a_schema = schema.clone();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(
            &a_schema,
            "mutation{markNotificationsRead}",
            Some("bobtok"),
            "prune-read",
        )
        .await
    }));
    reached(&point).await;
    let b = tokio::spawn(async move {
        exec(
            &schema,
            &format!("mutation{{deleteComment(commentId:\"{reply}\")}}"),
            Some("bobtok"),
            "prune-child",
        )
        .await
    });
    lock_wait(&pool).await;
    point.release();
    let marked = finish(a).await;
    assert!(marked.errors.is_empty(), "{:?}", marked.errors);
    assert_eq!(marked.data.into_json().unwrap()["markNotificationsRead"], 2);
    successful(finish(b).await);
    let rows: (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM comments WHERE id=ANY($1)),(SELECT count(*) FROM notifications WHERE comment_id=ANY($1))")
        .bind(vec![root, reply]).fetch_one(&pool).await.unwrap();
    assert_eq!(rows, (0, 0));
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_new_admin_provisioning_rechecks_username_after_waiting() {
    let (_, pool) = setup_full(100).await;
    let b_pool = pool.foreground();
    let (b, hashed) = hashed_first(async move {
        provision_admins(&b_pool, &["newadmin".into()], Some("password123"), None).await
    })
    .await;
    let points = PausePoints::default();
    let point = points.arm("provision-new-locked");
    let a_pool = pool.foreground();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        provision_admins(&a_pool, &["newadmin".into()], Some("password123"), None).await
    }));
    // The writer hashes a password before BEGIN.
    reached_after_hashing(&point).await;
    hashed.release();
    lock_wait(&pool).await;
    point.release();
    finish(a).await.unwrap();
    finish(b).await.unwrap();
    let rows: (i64, bool) = sqlx::query_as(
        "SELECT count(*),bool_and(is_admin AND NOT is_banned) FROM users WHERE username='newadmin'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, (1, true));
    pool.close().await;
}

#[tokio::test]
async fn two_sessions_password_reset_rechecks_a_demoted_actor() {
    let (schema, pool) = setup_full(100).await;
    sqlx::query("UPDATE users SET is_admin=TRUE WHERE id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    // resetUserPassword checks the actor and hashes the new password before BEGIN.
    let (b, hashed) = hashed_first(async move {
        exec(
            &schema,
            &format!(
                "mutation{{resetUserPassword(userId:\"{BOB}\",password:\"should-not-change\")}}"
            ),
            Some("admintok"),
            "stale-reset",
        )
        .await
    })
    .await;
    let points = PausePoints::default();
    let point = points.arm("membership-locked");
    let a_pool = pool.foreground();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        set_user_admin_once(&a_pool, BOB, ADMIN, false).await
    }));
    reached(&point).await;
    hashed.release();
    lock_wait(&pool).await;
    point.release();
    finish(a).await.unwrap();
    let rejected = finish(b).await;
    assert_eq!(rejected.errors.len(), 1);
    assert_eq!(rejected.errors[0].message, "Admin access required");
    let current: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id=$1")
        .bind(crate::db_types::uuid(BOB).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(current, hash);
    pool.close().await;
}

/// Owner and existence discovery run inside guarded transactions, twice in every
/// progress write while the user row is locked. Each statement must reach the
/// chapter and series tables through an index: comparing an integer key as text
/// cannot, and would scan the whole table under the lock.
#[tokio::test]
async fn guarded_discovery_statements_use_indexes_and_find_both_stores() {
    let (_, pool) = setup_full(100).await;
    seed_canonical(&pool, "phase4-plan", "Plan", false, "1").await;
    seed_target_series(&pool, "42").await;
    sqlx::query("INSERT INTO suwayomi_chapter(id,manga_id,name,chapter_number,updated_at) VALUES(420,42,'one',1,now())")
        .execute(&pool).await.unwrap();
    let foreground = pool.foreground();
    let mut tx = foreground.begin().await.unwrap();
    assert_eq!(
        progress_sources_on(&mut tx, "420").await.unwrap(),
        vec!["ss_42".to_string()]
    );
    let mangadex: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM source_series WHERE source_type='mangadex' AND source_key='phase4-plan'",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(mangadex.len(), 1);
    assert_eq!(
        progress_sources_on(&mut tx, "phase4-plan-1").await.unwrap(),
        mangadex
    );
    for unknown in ["421", "phase4-plan-2", "", "99999999999999999999999"] {
        assert!(progress_sources_on(&mut tx, unknown)
            .await
            .unwrap()
            .is_empty());
    }
    // With sequential scans disabled the planner uses one only where no index can
    // serve the predicate, whatever the size of these fixture tables.
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .unwrap();
    let progress: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {PROGRESS_SOURCES_SQL}"))
        .bind("420")
        .bind("suwayomi")
        .bind(Some(420_i64))
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    let series: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {KNOWN_SUWAYOMI_SERIES_SQL}"))
        .bind(Some(42_i64))
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    let chapter: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {KNOWN_CHAPTER_SQL}"))
        .bind("420")
        .bind(Some(420_i64))
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    for (name, plan) in [
        ("progress sources", progress),
        ("series", series),
        ("chapter", chapter),
    ] {
        assert!(
            !plan.iter().any(|line| line.contains("Seq Scan")),
            "{name} discovery scans a table: {plan:#?}"
        );
    }
    tx.rollback().await.unwrap();
    pool.close().await;
}

// ---- Integration round ----------------------------------------------------------

/// `LockRows` must sit above the `Sort`: rows are then locked in the sorted order and
/// not in the order the scan happens to find them.
async fn locks_above_sort(
    pool: &Database,
    sql: &str,
    user: uuid::Uuid,
    ids: Option<&[uuid::Uuid]>,
) {
    let foreground = pool.foreground();
    let mut tx = foreground.begin().await.unwrap();
    // Without the ordered index paths the plan needs a real Sort node.
    sqlx::raw_sql(
        "SET LOCAL enable_indexscan=off; SET LOCAL enable_indexonlyscan=off; \
         SET LOCAL enable_bitmapscan=off",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let explain = format!("EXPLAIN (COSTS OFF) {sql}");
    let mut query = sqlx::query_scalar::<_, String>(&explain).bind(user);
    if let Some(ids) = ids {
        query = query.bind(ids);
    }
    let plan = query.fetch_all(&mut *tx).await.unwrap();
    let at = |node: &str| plan.iter().position(|line| line.contains(node));
    assert!(
        matches!((at("LockRows"), at("Sort")), (Some(lock), Some(sort)) if lock < sort),
        "{plan:#?}"
    );
}

/// Check item 2: mark-read used a lock statement and an update per notification. It is
/// one ordered lock statement for the complete set, then one update.
#[tokio::test]
async fn mark_read_locks_its_complete_set_with_one_ordered_statement() {
    let (_, pool) = setup_full(100).await;
    let bob = crate::db_types::uuid(BOB).unwrap();
    let ids: Vec<uuid::Uuid> = (1..=5u128).map(uuid::Uuid::from_u128).collect();
    locks_above_sort(&pool, LOCK_UNREAD_NOTIFICATIONS_SQL, bob, None).await;
    locks_above_sort(&pool, LOCK_LISTED_NOTIFICATIONS_SQL, bob, Some(&ids)).await;
    for listed in [None, Some(3usize)] {
        sqlx::query("DELETE FROM notifications")
            .execute(&pool)
            .await
            .unwrap();
        for id in &ids {
            sqlx::query(
                "INSERT INTO notifications(id,user_id,kind,created_at) VALUES($1,$2,'reply',now())",
            )
            .bind(id)
            .bind(bob)
            .execute(&pool)
            .await
            .unwrap();
        }
        let wanted = listed.unwrap_or(ids.len());
        let points = PausePoints::default();
        let point = points.arm("notification-locked");
        let a_pool = pool.foreground();
        let a_ids = listed.map(|count| ids[..count].to_vec());
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            let now = crate::db_types::format_date(Utc::now());
            mark_notifications_read_once(&a_pool, BOB, a_ids.as_deref(), &now).await
        }));
        reached(&point).await;
        // The writer has not marked anything yet and already holds every row of its
        // set: a second session finds exactly the rows outside the set unlocked.
        let foreground = pool.foreground();
        let mut peer = foreground.begin().await.unwrap();
        let free: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM (SELECT id FROM notifications WHERE user_id=$1 \
             FOR UPDATE SKIP LOCKED) free",
        )
        .bind(bob)
        .fetch_one(&mut *peer)
        .await
        .unwrap();
        peer.rollback().await.unwrap();
        assert_eq!(free as usize, ids.len() - wanted);
        point.release();
        assert_eq!(finish(a).await.unwrap() as usize, wanted);
    }
    pool.close().await;
}

/// Check item 5: `deleteComment` locks several comment rows and was not retried. A
/// writer that holds one of those rows and then asks for the thread key closes a cycle
/// in which the delete is the victim; the mutation must run again and succeed.
#[tokio::test]
async fn delete_comment_runs_again_when_it_is_the_deadlock_victim() {
    let (schema, pool) = setup_full(100).await;
    let (root, reply) = seed_thread(&pool).await;
    let points = PausePoints::default();
    let point = points.arm("thread-locked");
    let a = tokio::spawn(GUARD_PAUSE.scope(
        points,
        crate::db::test_counters::scope(async move {
            exec(
                &schema,
                &format!("mutation{{deleteComment(commentId:\"{reply}\")}}"),
                Some("admintok"),
                "victim",
            )
            .await
        }),
    ));
    // The delete holds its user row and the thread key, and no comment row yet.
    reached(&point).await;
    let foreground = pool.foreground();
    let mut rival = foreground.begin().await.unwrap();
    // Only the delete may find the cycle: it is the last one to start waiting, and
    // this session's own check is far away.
    sqlx::query("SET LOCAL deadlock_timeout='60s'")
        .execute(&mut *rival)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM comments WHERE id=$1 FOR UPDATE")
        .bind(reply)
        .execute(&mut *rival)
        .await
        .unwrap();
    let rival = tokio::spawn(async move {
        crate::locks::advisory(
            &mut rival,
            &[crate::locks::AdvisoryKey::new(
                crate::locks::KeyClass::Thread,
                &[&root.to_string()],
            )],
        )
        .await
        .unwrap();
        rival.commit().await.unwrap();
    });
    lock_wait(&pool).await;
    point.release();
    let (response, seen) = finish(a).await;
    finish(rival).await;
    successful(response);
    assert_eq!((seen.database.deadlocks, seen.database.retries), (1, 1));
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM comments WHERE id=$1 AND deleted_by IS NULL")
            .bind(reply)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(live, 0);
    pool.close().await;
}

/// Asked by the checker for the integrated code: a moderator who has no row on the
/// work deletes a nested comment while the real catalogue merge retires that work.
/// Both commit orders finish without a lock error, and the thread ends on the survivor.
#[tokio::test]
async fn moderator_delete_of_a_nested_comment_and_a_merge_of_its_work_both_finish() {
    for delete_first in [true, false] {
        let (schema, pool) = setup_full(100).await;
        let (root, reply) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let bob = crate::db_types::uuid(BOB).unwrap();
        sqlx::query("INSERT INTO comments(id,user_id,target_type,target_id,body,created_at) VALUES($1,$2,'series','w_s1','root',now())")
            .bind(root).bind(bob).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO comments(id,user_id,target_type,target_id,parent_id,body,created_at) VALUES($1,$2,'series','w_s1',$3,'reply',now())")
            .bind(reply).bind(bob).bind(root).execute(&pool).await.unwrap();
        let query = format!("mutation{{deleteComment(commentId:\"{reply}\")}}");
        let merge_pool = pool.clone();
        let merge = crate::db::test_counters::scope(async move {
            catalog::merge_works(&merge_pool, "w_s1", "w_s2").await
        });
        let delete = crate::db::test_counters::scope(async move {
            exec(&schema, &query, Some("admintok"), "moderator").await
        });
        let points = PausePoints::default();
        let ((merged, merge_seen), (deleted, delete_seen)) = if delete_first {
            let point = points.arm("comment-delete-locked");
            let a = tokio::spawn(GUARD_PAUSE.scope(points, delete));
            reached(&point).await;
            // The merge re-points the rows of the thread, which the delete holds.
            let b = tokio::spawn(merge);
            lock_wait(&pool).await;
            point.release();
            let deleted = finish(a).await;
            (finish(b).await, deleted)
        } else {
            let point = points.arm("guard-held");
            let a = tokio::spawn(catalog::guards::TEST_PAUSE.scope(points, merge));
            reached(&point).await;
            // The merge holds its complete guard set and no comment row. The
            // moderator's delete needs none of those guards and does not wait.
            let deleted = finish(tokio::spawn(delete)).await;
            point.release();
            (finish(a).await, deleted)
        };
        merged.unwrap();
        successful(deleted);
        assert_eq!(
            (
                merge_seen.database.deadlocks,
                delete_seen.database.deadlocks
            ),
            (0, 0)
        );
        let rows: Vec<(uuid::Uuid, String)> =
            sqlx::query_as("SELECT id,target_id FROM comments WHERE deleted_by IS NULL")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, vec![(root, "w_s2".to_string())]);
        assert!(!work_exists(&pool, "w_s1").await);
        pool.close().await;
    }
}

/// Check item 6: the expired-session sweep ran inside the login transaction, with the
/// user row locked, and waited on any expired session another writer held. It runs
/// after the login's COMMIT now and skips rows that are locked.
#[tokio::test]
async fn login_sweeps_expired_sessions_after_its_commit_and_never_waits_for_one() {
    let (_, pool) = setup_full(100).await;
    let expired = auth::hash_token("expiredtok");
    let count = |sql: &'static str| {
        let (pool, expired) = (pool.clone(), expired.clone());
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(expired)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    const EXPIRED: &str = "SELECT count(*) FROM sessions WHERE token=$1";
    const LIVE: &str =
        "SELECT count(*) FROM sessions WHERE token<>$1 AND expires_at>now() AND user_id='ca19534e-7e0c-4aa3-8f2d-6a3799b9c33c'";
    let foreground = pool.foreground();
    let mut holder = foreground.begin().await.unwrap();
    sqlx::query("SELECT token FROM sessions WHERE token=$1 FOR UPDATE")
        .bind(&expired)
        .execute(&mut *holder)
        .await
        .unwrap();
    let login_pool = pool.foreground();
    let login = tokio::spawn(async move { new_session(&login_pool, BOB, 3600).await });
    tokio::time::timeout(STEP, login)
        .await
        .expect("the login waited for an expired session that another writer holds")
        .unwrap()
        .unwrap();
    assert_eq!((count(LIVE).await, count(EXPIRED).await), (2, 1));
    holder.rollback().await.unwrap();
    new_session(&pool.foreground(), BOB, 3600).await.unwrap();
    assert_eq!((count(LIVE).await, count(EXPIRED).await), (3, 0));
    pool.close().await;
}

/// T125: the like-milestone notification is written by the vote's own transaction.
/// Before, the vote committed first and the notification followed by itself.
#[tokio::test]
async fn a_vote_and_its_like_milestone_commit_together() {
    let (schema, pool) = setup_full(100).await;
    let (root, _) = seed_thread(&pool).await;
    let visible = || async {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT (SELECT count(*) FROM comment_votes WHERE comment_id=$1),\
             (SELECT count(*) FROM notifications WHERE comment_id=$1 AND user_id=$2)",
        )
        .bind(root)
        .bind(crate::db_types::uuid(ADMIN).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let points = PausePoints::default();
    let point = points.arm("vote-recorded");
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(
            &schema,
            &format!("mutation{{voteComment(commentId:\"{root}\",value:1){{likes}}}}"),
            Some("bobtok"),
            "first-like",
        )
        .await
    }));
    // The vote and the notification of its first like are written and not committed.
    reached(&point).await;
    assert_eq!(visible().await, (0, 0));
    point.release();
    successful(finish(a).await);
    assert_eq!(visible().await, (1, 1));
    pool.close().await;
}

/// T134, T199, T200: both admin writes of one `series_admin` row take the `Series` key.
/// `setSeriesPaused` changes the pause alone; `updateSeriesAdmin` replaces the whole
/// override state, so an override it does not name is cleared.
#[tokio::test]
async fn series_admin_writes_share_the_series_key() {
    let (_, pool) = setup_full(100).await;
    let replace = |paused| SeriesAdminWrite::Replace {
        override_interval_hours: Some(6.0),
        poll_every_minutes: Some(30),
        paused,
        status: None,
    };
    let points = PausePoints::default();
    let point = points.arm("series-admin-locked");
    let a_pool = pool.foreground();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        write_series_admin_once(&a_pool, 42, "42", replace(None), Utc::now()).await
    }));
    reached(&point).await;
    let b_pool = pool.foreground();
    let b = tokio::spawn(async move {
        write_series_admin_once(
            &b_pool,
            42,
            "42",
            SeriesAdminWrite::Paused(true),
            Utc::now(),
        )
        .await
    });
    lock_wait(&pool).await;
    point.release();
    finish(a).await.unwrap();
    finish(b).await.unwrap();
    let row = || async {
        sqlx::query_as::<_, (Option<f64>, Option<i32>, Option<bool>)>(
            "SELECT override_interval_hours,poll_every_minutes,paused_override \
             FROM series_admin WHERE series_id='42'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(row().await, (Some(6.0), Some(30), Some(true)));
    write_series_admin_once(&pool.foreground(), 42, "42", replace(None), Utc::now())
        .await
        .unwrap();
    assert_eq!(row().await, (Some(6.0), Some(30), None));
    pool.close().await;
}

/// T147: one enriched record is ONE transaction under the identity keys of the real
/// catalogue upsert. Content and completion markers become visible together, and the
/// catalogue's own writer of the same MangaDex id waits for it.
#[tokio::test]
async fn enrichment_commits_content_and_markers_together_behind_the_catalogue_writer() {
    const MANGA: &str = "phase4-enriched";
    let (_, pool) = setup_full(100).await;
    let input = |title: &str| catalog::WorkInput {
        primary_title: Some(title.into()),
        ..Default::default()
    };
    let state = || async {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT count(*), count(w.metadata_synced_at) FROM source_series ss \
             JOIN work w ON w.id=ss.work_id \
             WHERE ss.source_type='mangadex' AND ss.source_key=$1",
        )
        .bind(MANGA)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let points = PausePoints::default();
    let point = points.arm("enrich-upserted");
    let (a_pool, a_input) = (pool.foreground(), input("Enriched"));
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        persist_enriched_work_once(&a_pool, MANGA, &a_input, true).await
    }));
    reached(&point).await;
    assert_eq!(state().await, (0, 0));
    let (b_pool, b_input) = (pool.clone(), input("Enriched"));
    let b =
        tokio::spawn(
            async move { catalog::upsert_work_from_mangadex(&b_pool, MANGA, &b_input).await },
        );
    lock_wait(&pool).await;
    point.release();
    finish(a).await.unwrap();
    finish(b).await.unwrap();
    assert_eq!(state().await, (1, 1));
    pool.close().await;
}

/// T146, T148, asked by the checker for the integrated code: an accepted review of two
/// DIFFERENT works (a real merge) against a rejection of the same row, both orders.
#[tokio::test]
async fn accepted_merge_and_rejection_of_one_review_have_one_winner() {
    for accept_first in [true, false] {
        let (schema, pool) = setup_full(100).await;
        sqlx::query("INSERT INTO merge_candidate(id,source_series_id,candidate_work_id,score,method,status,created_at) VALUES('phase4-pair','ss_s1','w_s2',0.8,'title_exact','pending',now())")
            .execute(&pool).await.unwrap();
        let query = |accept: bool| {
            format!("mutation{{resolveMergeCandidate(id:\"phase4-pair\",accept:{accept})}}")
        };
        let points = PausePoints::default();
        // The merge pauses after it folded the work, the rejection with its pair key
        // and rows held; neither has committed.
        let point = points.arm(if accept_first {
            "merge-pair-merged"
        } else {
            "candidate-locked"
        });
        let (a_schema, a_query) = (schema.clone(), query(accept_first));
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            exec(&a_schema, &a_query, Some("admintok"), "pair-a").await
        }));
        reached(&point).await;
        let b_query = query(!accept_first);
        let b =
            tokio::spawn(async move { exec(&schema, &b_query, Some("admintok"), "pair-b").await });
        lock_wait(&pool).await;
        point.release();
        successful(finish(a).await);
        let loser = finish(b).await;
        assert_eq!(loser.errors.len(), 1, "both decisions were applied");
        assert_eq!(
            loser.errors[0].message,
            "This merge candidate is already resolved."
        );
        assert_eq!(work_exists(&pool, "w_s1").await, !accept_first);
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM merge_candidate WHERE id='phase4-pair'")
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(
            status.as_deref(),
            Some(if accept_first {
                "confirmed"
            } else {
                "rejected"
            })
        );
        pool.close().await;
    }
}

/// T137: `addSeriesAltTitle` decided its merge from reads made before any lock and
/// handed the merge only the works' own columns. The other work loses the shared
/// title before the merge takes its locks; the merge must not happen.
#[tokio::test]
async fn alias_merge_reads_the_shared_title_again_under_the_merge_locks() {
    let (schema, pool) = setup_full(100).await;
    let kept = sweep_work(&pool, "mangadex", &["Gamma Saga"], Some("Kim")).await;
    let other = sweep_work(&pool, "suwayomi", &["Delta Saga", "Beta Saga"], Some("Kim")).await;
    let points = PausePoints::default();
    let point = points.arm("merge-discovered");
    let query = format!("mutation{{addSeriesAltTitle(id:\"{kept}\",title:\"Beta Saga\"){{id}}}}");
    let a = tokio::spawn(catalog::guards::TEST_PAUSE.scope(points, async move {
        exec(&schema, &query, Some("admintok"), "alias").await
    }));
    // The merge is decided and has taken no lock.
    reached(&point).await;
    catalog::remove_work_alias(&pool, &other, "Beta Saga")
        .await
        .unwrap();
    point.release();
    successful(finish(a).await);
    assert!(work_exists(&pool, &kept).await && work_exists(&pool, &other).await);
    pool.close().await;
}

/// T157: the reconcile merged on the matcher's lock-free answer with no precondition
/// at all. The target loses the matching title before the merge takes its locks.
#[tokio::test]
async fn reconcile_runs_the_matcher_again_under_the_merge_locks() {
    let (_, pool) = setup_full(100).await;
    let target = sweep_work(&pool, "mangadex", &["Alpha Saga", "Beta Saga"], Some("Kim")).await;
    let source = sweep_work(&pool, "suwayomi", &["Beta Saga"], Some("Kim")).await;
    let points = PausePoints::default();
    let point = points.arm("merge-discovered");
    let (a_pool, a_source) = (pool.clone(), source.clone());
    let a = tokio::spawn(catalog::guards::TEST_PAUSE.scope(points, async move {
        reconcile_one(&a_pool, None, &a_source).await
    }));
    reached(&point).await;
    // The background lane has one connection and the paused writer holds it.
    catalog::remove_work_alias(&pool.foreground(), &target, "Beta Saga")
        .await
        .unwrap();
    point.release();
    assert!(matches!(finish(a).await.unwrap(), ReconcileAction::Skipped));
    assert!(work_exists(&pool, &target).await && work_exists(&pool, &source).await);
    pool.close().await;
}

/// T153, on the integrated code: the sweep's merge is one guarded unit that reads the
/// pair again under the merge locks and counts a changed pair as stale.
#[tokio::test]
async fn sweep_merge_reads_the_pair_again_under_the_merge_locks() {
    let (_, pool) = setup_full(100).await;
    let spine = sweep_work(
        &pool,
        "mangadex",
        &["Kusuriya no Hitorigoto", "The Apothecary Diaries"],
        Some("Hyuuga Natsu"),
    )
    .await;
    let site = sweep_work(
        &pool,
        "suwayomi",
        &["Apothecary Diaries"],
        Some("Hyuuga Natsu"),
    )
    .await;
    let points = PausePoints::default();
    let point = points.arm("merge-discovered");
    let a_pool = pool.clone();
    let a = tokio::spawn(catalog::guards::TEST_PAUSE.scope(points, async move {
        consolidate_exact_duplicates_from(&a_pool, None, 100, "").await
    }));
    reached(&point).await;
    catalog::remove_work_alias(&pool.foreground(), &spine, "The Apothecary Diaries")
        .await
        .unwrap();
    point.release();
    let out = finish(a).await.unwrap();
    assert!(out.merged.is_empty(), "{:?}", out.merged);
    assert_eq!(out.stale, 1);
    assert!(work_exists(&pool, &spine).await && work_exists(&pool, &site).await);
    pool.close().await;
}

/// T154: the review's source mapping was chosen on the pool and the row inserted in a
/// second transaction. The owning work is merged away in between; the question the
/// rules asked no longer exists and no review may be written for the work that
/// inherited the mapping.
#[tokio::test]
async fn queued_review_chooses_its_source_inside_the_insert_transaction() {
    let (_, pool) = setup_full(100).await;
    let loser = sweep_work(&pool, "suwayomi", &["Queue Loser"], None).await;
    let survivor = sweep_work(&pool, "mangadex", &["Queue Survivor"], None).await;
    let third = sweep_work(&pool, "mangadex", &["Queue Third"], None).await;
    let points = PausePoints::default();
    let point = points.arm("review-queue-keyed");
    let (a_pool, a_loser, a_survivor) = (pool.clone(), loser.clone(), survivor.clone());
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        queue_consolidate_review(&a_pool, &a_loser, &a_survivor, 0.6).await
    }));
    reached(&point).await;
    catalog::merge_works(&pool.foreground(), &loser, &third)
        .await
        .unwrap();
    point.release();
    assert!(!finish(a).await.unwrap(), "a review was reported as queued");
    let reviews: i64 = sqlx::query_scalar("SELECT count(*) FROM merge_candidate")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reviews, 0);
    pool.close().await;
}

/// T155: the review sort closes a review inside one transaction that takes the pair
/// key a person's decision takes. The person's rejection waits and finds it closed.
#[tokio::test]
async fn sorted_dismissal_and_a_persons_decision_have_one_winner() {
    let (schema, pool) = setup_full(100).await;
    // The second row asks the question of the first one again, the other way round.
    for (id, source, work) in [("p4-a", "ss_s1", "w_s2"), ("p4-b", "ss_s2", "w_s1")] {
        sqlx::query("INSERT INTO merge_candidate(id,source_series_id,candidate_work_id,score,method,status,created_at) VALUES($1,$2,$3,0.8,'title_exact','pending',now())")
            .bind(id).bind(source).bind(work).execute(&pool).await.unwrap();
    }
    let points = PausePoints::default();
    let point = points.arm("review-dismiss-locked");
    let a_pool = pool.clone();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        dismiss_review_once(&a_pool, "p4-b", "w_s2", "w_s1").await
    }));
    reached(&point).await;
    let b = tokio::spawn(async move {
        exec(
            &schema,
            "mutation{resolveMergeCandidate(id:\"p4-b\",accept:false)}",
            Some("admintok"),
            "person",
        )
        .await
    });
    lock_wait(&pool).await;
    point.release();
    assert!(finish(a).await.unwrap());
    let loser = finish(b).await;
    assert_eq!(loser.errors.len(), 1);
    assert_eq!(
        loser.errors[0].message,
        "This merge candidate is already resolved."
    );
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT id,status FROM merge_candidate ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        statuses,
        vec![
            ("p4-a".to_string(), "pending".to_string()),
            ("p4-b".to_string(), "dismissed".to_string())
        ]
    );
    pool.close().await;
}

fn enrolled(id: i64, source: &str) -> crate::suwayomi::SuwayomiManga {
    crate::suwayomi::SuwayomiManga {
        author: Some("Kim".into()),
        ..suwayomi_manga(id, "Twin Enrolment Saga", &["Action"], source)
    }
}

/// T159 with the catalogue's T024 and T093: an enrolment is ONE transaction under the
/// source claim keys and the match domain of its title. Two enrolments of one new
/// title from two sources used to both read "no such work" and mint two works; two
/// enrolments of the same series raced for the claim and deleted the loser's work
/// afterwards.
#[tokio::test]
async fn two_enrolments_of_one_title_share_one_work() {
    for same_series in [false, true] {
        let (_, pool) = setup_full(100).await;
        let points = PausePoints::default();
        let point = points.arm("enrol-keys-held");
        let a_pool = pool.clone();
        let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
            add_source_series_core(&a_pool, &enrolled(9001, "src-a"), None).await
        }));
        reached(&point).await;
        // Its own lane: the background lane has one connection, which the first
        // enrolment holds, and this writer must reach PostgreSQL to wait there.
        let b_pool = pool.foreground();
        let b = tokio::spawn(async move {
            let second = if same_series {
                enrolled(9001, "src-a")
            } else {
                enrolled(9002, "src-b")
            };
            add_source_series_core(&b_pool, &second, None).await
        });
        lock_wait(&pool).await;
        point.release();
        let first = finish(a).await.unwrap();
        let second = finish(b).await.unwrap();
        assert_eq!(first.decision, "new");
        assert_eq!(
            second.decision,
            if same_series {
                "existing"
            } else {
                "auto_merge"
            }
        );
        assert_eq!(second.work_id, first.work_id);
        let rows: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM work WHERE primary_title='Twin Enrolment Saga'),\
             (SELECT count(*) FROM source_series WHERE work_id=$1)",
        )
        .bind(&first.work_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, (1, if same_series { 1 } else { 2 }));
        pool.close().await;
    }
}

/// T159: a provisional work, its source mapping and its review are written by one
/// transaction. Before, they were three commits and a failure between them left a
/// work without a mapping, or a mapping without the review the matcher asked for.
#[tokio::test]
async fn a_provisional_enrolment_commits_its_work_mapping_and_review_together() {
    let (_, pool) = setup_full(100).await;
    // Same title, no author on either side: the matcher asks a person.
    let known = sweep_work(&pool, "mangadex", &["Twin Enrolment Saga"], None).await;
    let state = || async {
        sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT (SELECT count(*) FROM work WHERE primary_title='Twin Enrolment Saga'),\
             (SELECT count(*) FROM source_series WHERE source_key='9003'),\
             (SELECT count(*) FROM merge_candidate WHERE candidate_work_id=$1)",
        )
        .bind(&known)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let points = PausePoints::default();
    let point = points.arm("enrol-written");
    let a_pool = pool.clone();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        let series = suwayomi_manga(9003, "Twin Enrolment Saga", &["Action"], "src-a");
        add_source_series_core(&a_pool, &series, None).await
    }));
    reached(&point).await;
    assert_eq!(state().await, (1, 0, 0));
    point.release();
    let result = finish(a).await.unwrap();
    assert_eq!(result.decision, "review");
    assert_eq!(result.matched_work_id.as_deref(), Some(known.as_str()));
    assert_eq!(state().await, (2, 1, 1));
    pool.close().await;
}

/// T105, T106: the callers of the chapter cache write it after the upstream reply,
/// outside any transaction, as a deferred job. A reader gets its chapters while a
/// writer of the same series holds the `Series` key, and the cache write lands once
/// that writer commits.
#[tokio::test]
async fn chapter_cache_fill_never_holds_the_reader_behind_the_series_key() {
    let router = axum::Router::new().route(
        "/api/graphql",
        axum::routing::post(
            |axum::Json(body): axum::Json<serde_json::Value>| async move {
                let id = body["variables"]["id"].as_i64().unwrap();
                axum::Json(serde_json::json!({"data": {"fetchMangaAndChapters": {
                    "manga": {"id": id, "title": "Fixture 42", "status": "ONGOING",
                        "inLibrary": true, "sourceId": "src", "genre": []},
                    "chapters": [{"id": id * 10, "mangaId": id, "name": "Fetched",
                        "chapterNumber": 1.0, "isRead": false, "isBookmarked": false,
                        "isDownloaded": false, "lastPageRead": 0, "pageCount": 12}]
                }}}))
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let engine = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (_, pool, state) = setup_with_engine(100, &engine).await;
    let points = PausePoints::default();
    let point = points.arm("series-admin-locked");
    let a_pool = pool.foreground();
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        write_series_admin_once(
            &a_pool,
            42,
            "42",
            SeriesAdminWrite::Paused(true),
            Utc::now(),
        )
        .await
    }));
    reached(&point).await;
    let fetched = tokio::time::timeout(STEP, resolve_chapters_cached(&state, 42))
        .await
        .expect("the reader waited for the cache write")
        .unwrap();
    assert_eq!(fetched[0].name, "Fetched");
    // The deferred cache write is the one waiting for the key.
    lock_wait(&pool).await;
    point.release();
    finish(a).await.unwrap();
    pool.flush_deferred().await;
    let cached: i64 = sqlx::query_scalar("SELECT count(*) FROM suwayomi_chapter WHERE manga_id=42")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cached, 1);
    server.abort();
    pool.close().await;
}

/// The retention sweep against the real `deleteComment`: the delete holds the
/// notification rows of its comment; the sweep takes the rows that are free, leaves
/// the held one and does not wait.
#[tokio::test]
async fn notification_gc_passes_a_row_that_delete_comment_holds() {
    let (schema, pool) = setup_full(100).await;
    let (_, reply) = seed_thread(&pool).await;
    for (id, comment) in [(1u128, Some(reply)), (2, None)] {
        sqlx::query("INSERT INTO notifications(id,user_id,kind,comment_id,created_at,read_at) VALUES($1,$2,'reply',$3,'2020-01-01T00:00:00Z','2020-01-02T00:00:00Z')")
            .bind(uuid::Uuid::from_u128(id)).bind(crate::db_types::uuid(BOB).unwrap()).bind(comment)
            .execute(&pool).await.unwrap();
    }
    let points = PausePoints::default();
    let point = points.arm("comment-delete-locked");
    let a = tokio::spawn(GUARD_PAUSE.scope(points, async move {
        exec(
            &schema,
            &format!("mutation{{deleteComment(commentId:\"{reply}\")}}"),
            Some("admintok"),
            "delete",
        )
        .await
    }));
    reached(&point).await;
    let (_stop, shutdown) = tokio::sync::watch::channel(false);
    let cutoff = crate::db_types::format_date(Utc::now());
    let pruned = tokio::time::timeout(
        STEP,
        crate::gc::prune_read_notifications(&pool, &cutoff, &shutdown),
    )
    .await
    .expect("the sweep waited for a row that deleteComment holds")
    .unwrap();
    assert_eq!(pruned, 1);
    point.release();
    successful(finish(a).await);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    pool.close().await;
}

/// Foundation residue (R2-4): a read helper that cannot get a reader connection is a
/// "busy" answer counted on the read lane. It used to be counted as a write-lane lock
/// error, which moves the write gate for a failure no writer had.
#[tokio::test]
async fn read_helper_pool_timeout_is_counted_on_the_read_lane() {
    let (_, pool, state) = setup_with_state(100).await;
    assert_eq!(pool.read().options().get_max_connections(), 1);
    let reader = pool.read().acquire().await.unwrap();
    let (result, seen) = crate::db::test_counters::scope(reload_series(&state, BOB, "w_s1")).await;
    drop(reader);
    let Err(error) = result else {
        panic!("the read succeeded without a reader connection");
    };
    assert_eq!(error.message, "Server busy; retry soon");
    assert_eq!(
        seen,
        crate::db::test_counters::Counters {
            database: crate::db::DatabaseCounters {
                pool_timeouts: 1,
                ..Default::default()
            },
            lock_errors: 0,
            write_lane_events: 0,
            read_lane_events: 1,
        }
    );
    pool.close().await;
}
