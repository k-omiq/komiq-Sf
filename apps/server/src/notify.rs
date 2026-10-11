//! Inbound notifications — something ANOTHER user did to the recipient's content.
//!
//! Distinct from `user_activity` (the recipient's own OUTBOUND action log). Two kinds:
//!   - `reply`          — someone replied to the recipient's comment (one per reply).
//!   - `like_milestone` — the recipient's comment crossed a like-count milestone.
//!
//! A `reply` notification follows the fire-and-forget pattern of `log_activity`: it is
//! written after the comment has committed, so it must never fail the mutation that
//! triggered it, and errors are logged and swallowed. A `like_milestone` is written
//! INSIDE the vote transaction ([`record_like_milestone_on`]): the vote and the
//! notification it earns commit together, and an error there is the vote's error.

use crate::db::Database;

/// Like counts that produce a `like_milestone` notification. The first like (1) doubles
/// as the "someone liked your comment" signal; the rest celebrate growth without a
/// notification per like.
pub const LIKE_MILESTONES: &[i64] = &[1, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000];

/// Insert one notification for `recipient_id`. `actor_id` is the triggering user (the
/// replier), NULL for an aggregate milestone. `comment_id` is the new reply for a
/// reply notification, or the recipient's comment for a milestone. Thread keys allow
/// deep-linking; `count` is the milestone value (`like_milestone` only).
#[allow(clippy::too_many_arguments)]
pub async fn record(
    pool: &Database,
    recipient_id: &str,
    kind: &str,
    actor_id: Option<&str>,
    comment_id: Option<&str>,
    target_type: Option<&str>,
    target_id: Option<&str>,
    count: Option<i64>,
) {
    let (Ok(recipient_id), Ok(actor_id), Ok(comment_id)) = (
        crate::db_types::uuid(recipient_id),
        crate::db_types::optional_uuid(actor_id),
        crate::db_types::optional_uuid(comment_id),
    ) else {
        tracing::warn!(kind, "invalid notification identity");
        return;
    };
    let res = sqlx::query(
        "INSERT INTO notifications \
           (id, user_id, kind, actor_id, comment_id, target_type, target_id, count, created_at) \
         SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9 \
         WHERE $5::uuid IS NULL OR EXISTS \
           (SELECT 1 FROM comments WHERE id = $5 AND deleted_by IS NULL FOR SHARE)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(recipient_id)
    .bind(kind)
    .bind(actor_id)
    .bind(comment_id)
    .bind(target_type)
    .bind(target_id)
    .bind(count)
    .bind(chrono::Utc::now())
    .execute(pool)
    .await;
    if let Err(e) = res {
        crate::db::record_database_error(&e);
        tracing::warn!(error = %e, kind, "failed to record notification");
    }
}

/// Record a `like_milestone` notification on the caller's connection, idempotent per
/// (comment, count): the partial UNIQUE index `idx_notifications_like_milestone` makes a
/// second identical milestone a no-op via `ON CONFLICT DO NOTHING`, so a like that
/// re-crosses a milestone after an unlike notifies exactly once.
///
/// It does not begin, commit or roll back, and it RETURNS the error: the vote
/// transaction inserts the milestones it crossed before its own COMMIT. It does not
/// count the error either: the caller's operation boundary (`gql_err`, a retry attempt)
/// observes it once. This is the only writer of milestones; the earlier fire-and-forget
/// wrapper that committed by itself after the vote is gone, because two paths for one
/// row meant a counted like could lose its notification.
///
/// Guards the caller must hold, in the global lock order, BEFORE calling:
///
/// * its complete advisory tier (the comment's `Thread` key in the vote path) and every
///   user/work parent lock the transaction needs. This statement takes a child-tier lock,
///   so no advisory key or parent row may be acquired after it;
/// * the comment row, locked by the caller in the strength its own decision needs (the
///   vote path's `SELECT ... FROM comments WHERE id = $1 FOR UPDATE`). The statement's
///   own `FOR SHARE` on that row is then a re-lock by the same transaction. It is kept so
///   that a caller without the row lock still cannot insert a milestone for a comment
///   whose tombstone commits concurrently: the comment is read live, and a deleted or
///   missing comment inserts nothing and returns `Ok(())`;
/// * the like count that crossed `count` must have been computed in this transaction,
///   under those guards. Two voters of one comment are serialised by the `Thread` key;
///   the unique index is the second line (the later insert waits for the earlier one's
///   outcome and then inserts nothing), not this function re-counting.
///
/// The insert takes a key-share lock on the recipient's user row (foreign key) while
/// the caller holds the comment row. An account deletion locks in the other order, so
/// the caller runs its unit in `db::retry_transaction`.
///
/// An invalid recipient or comment UUID is an error.
pub async fn record_like_milestone_on(
    conn: &mut sqlx::PgConnection,
    recipient_id: &str,
    comment_id: &str,
    target_type: &str,
    target_id: &str,
    count: i64,
) -> anyhow::Result<()> {
    use anyhow::Context;
    let recipient_id =
        crate::db_types::uuid(recipient_id).context("invalid notification identity")?;
    let comment_id = crate::db_types::uuid(comment_id).context("invalid notification identity")?;
    sqlx::query(
        "INSERT INTO notifications \
           (id, user_id, kind, actor_id, comment_id, target_type, target_id, count, created_at) \
         SELECT $1, $2, 'like_milestone', NULL, $3, $4, $5, $6, $7 \
         FROM comments WHERE id = $3 AND deleted_by IS NULL FOR SHARE ON CONFLICT DO NOTHING",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(recipient_id)
    .bind(comment_id)
    .bind(target_type)
    .bind(target_id)
    .bind(count)
    .bind(chrono::Utc::now())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTHOR: &str = "0c1f1f0e-52a4-4f0b-9d59-0d1f3a2b7c01";
    const COMMENT: &str = "5b0d3c52-5d0c-4a52-8a63-2f6f6b1f9a02";
    const DELETED: &str = "5b0d3c52-5d0c-4a52-8a63-2f6f6b1f9a03";

    async fn pool() -> Database {
        let pool = crate::database::TestPoolOptions::new()
            .max_connections(8)
            .connect("sqlite::memory:")
            .await
            .unwrap()
            .foreground();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        sqlx::query("INSERT INTO users(id,username,email,password_hash,created_at) VALUES ($1,'milestone-author','milestone@example.test','unused',now())")
            .bind(crate::db_types::uuid(AUTHOR).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        for (id, deleted_by) in [(COMMENT, None), (DELETED, Some("author"))] {
            sqlx::query("INSERT INTO comments(id,target_type,target_id,user_id,body,created_at,deleted_by) VALUES ($1,'series','s1',$2,'text',now(),$3)")
                .bind(crate::db_types::uuid(id).unwrap())
                .bind(crate::db_types::uuid(AUTHOR).unwrap())
                .bind(deleted_by)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    /// Committed milestone rows of one comment: (count, recipient, kind, actor, thread).
    async fn milestones(
        pool: &Database,
        comment: &str,
    ) -> Vec<(i64, String, String, bool, String)> {
        sqlx::query_as(
            "SELECT count, user_id::text, kind, actor_id IS NULL, target_type || ':' || target_id \
             FROM notifications WHERE comment_id = $1 ORDER BY count",
        )
        .bind(crate::db_types::uuid(comment).unwrap())
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// One milestone in a transaction of its own, as a vote that crosses it writes it.
    async fn committed(
        pool: &Database,
        recipient: &str,
        comment: &str,
        count: i64,
    ) -> anyhow::Result<()> {
        let mut tx = pool.begin().await?;
        record_like_milestone_on(&mut tx, recipient, comment, "series", "s1", count).await?;
        tx.commit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn like_milestone_on_shares_the_fate_of_the_caller_transaction() {
        let pool = pool().await;
        let expected = |count: i64| {
            (
                count,
                AUTHOR.to_owned(),
                "like_milestone".to_owned(),
                true,
                "series:s1".to_owned(),
            )
        };

        // Rolled back with the caller: nothing is published.
        let mut tx = pool.begin().await.unwrap();
        record_like_milestone_on(&mut tx, AUTHOR, COMMENT, "series", "s1", 5)
            .await
            .unwrap();
        let inside: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notifications")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(inside, 1, "the caller's own transaction sees the milestone");
        assert!(
            milestones(&pool, COMMENT).await.is_empty(),
            "the helper must not commit by itself"
        );
        tx.rollback().await.unwrap();
        assert!(milestones(&pool, COMMENT).await.is_empty());

        // Committed with the caller, once per (comment, count).
        let mut tx = pool.begin().await.unwrap();
        for count in [1, 5, 5] {
            record_like_milestone_on(&mut tx, AUTHOR, COMMENT, "series", "s1", count)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        assert_eq!(milestones(&pool, COMMENT).await, [expected(1), expected(5)]);
        committed(&pool, AUTHOR, COMMENT, 5).await.unwrap();
        committed(&pool, AUTHOR, COMMENT, 10).await.unwrap();
        assert_eq!(
            milestones(&pool, COMMENT).await,
            [expected(1), expected(5), expected(10)]
        );

        // A deleted comment and a missing comment insert nothing and are not errors.
        let mut tx = pool.begin().await.unwrap();
        record_like_milestone_on(&mut tx, AUTHOR, DELETED, "series", "s1", 1)
            .await
            .unwrap();
        record_like_milestone_on(
            &mut tx,
            AUTHOR,
            "5b0d3c52-5d0c-4a52-8a63-2f6f6b1f9aff",
            "series",
            "s1",
            1,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(milestones(&pool, DELETED).await.is_empty());

        // Errors come back to the caller: a bad identity, and a database error
        // (a recipient that does not exist violates the foreign key).
        let mut tx = pool.begin().await.unwrap();
        assert!(
            record_like_milestone_on(&mut tx, "not-a-uuid", COMMENT, "series", "s1", 25)
                .await
                .is_err()
        );
        let error = record_like_milestone_on(
            &mut tx,
            "0c1f1f0e-52a4-4f0b-9d59-0d1f3a2b7cff",
            COMMENT,
            "series",
            "s1",
            25,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<sqlx::Error>()
                .and_then(sqlx::Error::as_database_error)
                .and_then(|e| e.code())
                .as_deref(),
            Some("23503")
        );
        drop(tx);
        // A failed unit leaves nothing behind.
        assert!(
            committed(&pool, "0c1f1f0e-52a4-4f0b-9d59-0d1f3a2b7cff", COMMENT, 25)
                .await
                .is_err()
        );
        assert_eq!(milestones(&pool, COMMENT).await.len(), 3);
    }

    /// Two vote transactions cross the same milestone. The second waits for the first
    /// one's outcome on the partial unique index: after a commit it inserts nothing,
    /// after a rollback it inserts the row itself. Either way exactly one notification.
    #[tokio::test]
    async fn two_transactions_crossing_one_like_milestone_notify_exactly_once() {
        for first_commits in [true, false] {
            let pool = pool().await;
            let mut first = pool.begin().await.unwrap();
            record_like_milestone_on(&mut first, AUTHOR, COMMENT, "series", "s1", 5)
                .await
                .unwrap();
            let writer = pool.clone();
            let second = tokio::spawn(async move {
                let mut tx = writer.begin().await?;
                record_like_milestone_on(&mut tx, AUTHOR, COMMENT, "series", "s1", 5).await?;
                tx.commit().await?;
                Ok::<_, anyhow::Error>(())
            });
            crate::cover::wait_for_writer_lock(&pool).await;
            if first_commits {
                first.commit().await.unwrap();
            } else {
                first.rollback().await.unwrap();
            }
            second
                .await
                .unwrap()
                .expect("the second voter must not fail on the first one's milestone");
            let rows = milestones(&pool, COMMENT).await;
            assert_eq!(rows.len(), 1, "first_commits={first_commits}: {rows:?}");
            assert_eq!(rows[0].0, 5);
        }
    }
}
