//! Parent-first catalogue guards. Callers retain the transaction through publication.
use anyhow::Result;
use sqlx::PgConnection;

use crate::locks::{AdvisoryKey, KeyClass, RestartTransaction};

/// The identity keys of one source claim, without the source policy key.
pub(crate) fn source_identity_keys(kind: &str, source: &str, key: &str) -> Vec<AdvisoryKey> {
    let mut keys = vec![AdvisoryKey::source_identity(source, key)];
    // Suwayomi's key is globally unique, including when its previously unknown
    // source_id is learned. All three natural-key conflict branches must agree.
    if kind == "suwayomi" {
        keys.push(AdvisoryKey::source_identity("suwayomi", key));
    }
    keys
}

/// Identity keys plus the source policy key, all for the EXCLUSIVE `locks::advisory`
/// call of a unit that is built from the public `*_keys` functions. Correct for every
/// caller. The claim facades of this module take the policy key in shared mode instead
/// ([`lock_source_claim`]), so that two claims of one source do not wait for each other.
pub(crate) fn source_keys(kind: &str, source: &str, key: &str) -> Vec<AdvisoryKey> {
    let mut keys = source_identity_keys(kind, source, key);
    keys.push(AdvisoryKey::new(KeyClass::SourcePolicy, &[source]));
    keys
}

// A claim facade takes its identity keys first and the policy key last. That is the
// order one sorted `locks::advisory` call would use, because the policy class sorts
// after both identity classes.
const _: () = assert!(
    (KeyClass::ExternalIdentity as i32) < (KeyClass::SourcePolicy as i32)
        && (KeyClass::SourceIdentity as i32) < (KeyClass::SourcePolicy as i32)
);

/// The foundation's two-i32 key, shared by every advisory lock form.
pub(crate) fn advisory_ints(class: KeyClass, components: &[&str]) -> (i32, i32) {
    AdvisoryKey::new(class, components).integers()
}

fn source_policy_ints(source: &str) -> (i32, i32) {
    advisory_ints(KeyClass::SourcePolicy, &[source])
}

/// The advisory tier of a source CLAIM: the identity keys exclusively, in sorted order,
/// then `SourcePolicy(source)` in SHARED mode.
///
/// A claim only reads the source's policy (the admin NSFW pin). The writers of the
/// policy (`set_source_nsfw_pin`, the outage cutoff and its recovery) take the key
/// exclusively, so a claim still never overlaps a policy change. Claims of DIFFERENT
/// works of one source no longer wait for each other: with the exclusive form every
/// MangaDex upsert (one source id, `mangadex`) queued behind every other one, in the
/// foreground too.
///
/// This is the unit's whole advisory tier: call it once, before any row lock.
pub(crate) async fn lock_source_claim(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    identity_keys: &[AdvisoryKey],
    policy_source: &str,
) -> Result<()> {
    crate::locks::advisory(tx, identity_keys).await?;
    let (class, digest) = source_policy_ints(policy_source);
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1, $2)")
        .bind(class)
        .bind(digest)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn work(conn: &mut PgConnection, id: &str) -> Result<()> {
    sqlx::query("SELECT id FROM work WHERE id = $1 FOR NO KEY UPDATE")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    held().await;
    Ok(())
}

/// Resolve before locking and again after waiting. The whole transaction must
/// restart rather than adding a newly discovered work to a locked parent tier.
pub(crate) async fn resolve_work(conn: &mut PgConnection, id: &str) -> Result<String> {
    resolve_work_optional(conn, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no such work: {id}"))
}

/// Maintenance facades retain their historical no-op for a permanently absent
/// work while restarting discovery if a merge changed its canonical owner.
pub(crate) async fn resolve_work_optional(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<String>> {
    const RESOLVE: &str =
        "SELECT COALESCE((SELECT new_id FROM work_redirect WHERE old_id = $1), $1)";
    let owner: String = sqlx::query_scalar(RESOLVE)
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    let present: Option<String> =
        sqlx::query_scalar("SELECT id FROM work WHERE id = $1 FOR NO KEY UPDATE")
            .bind(&owner)
            .fetch_optional(&mut *conn)
            .await?;
    let current: String = sqlx::query_scalar(RESOLVE)
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;
    if current != owner {
        return Err(RestartTransaction.into());
    }
    held().await;
    Ok(present)
}

/// Connection variant for existing public helpers whose signatures cannot change.
/// Requires a caller-owned transaction, complete parent set and whole-unit retry.
pub(crate) async fn source(conn: &mut PgConnection, id: &str) -> Result<Option<String>> {
    let owner: Option<String> =
        sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(owner) = &owner {
        sqlx::query("SELECT id FROM work WHERE id = $1 FOR NO KEY UPDATE")
            .bind(owner)
            .fetch_optional(&mut *conn)
            .await?;
    }
    let current: Option<String> =
        sqlx::query_scalar("SELECT work_id FROM source_series WHERE id = $1 FOR NO KEY UPDATE")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    if owner != current {
        return Err(RestartTransaction.into());
    }
    held().await;
    Ok(current)
}

// ── Contract assertions of the transaction-taking (`_on`) helpers ─────────────────
//
// An `_on` helper neither begins nor commits; the guards it documents are the caller's
// to take. Documentation alone does not stop a caller from breaking that, and the
// result would be a lock-order inversion that only shows as a rare deadlock. The three
// functions below turn each documented contract into a panic in debug and test builds.
// A release build compiles them to nothing: no statement, no round trip.

/// The caller must be inside a transaction: a write helper that ran on a bare pooled
/// connection would commit each statement by itself and hold no lock at all.
#[cfg_attr(not(debug_assertions), allow(unused_variables))]
pub(crate) fn assert_in_transaction(conn: &PgConnection, helper: &str) {
    debug_assert!(
        sqlx::Connection::is_in_transaction(conn),
        "{helper} must be called inside the caller's transaction"
    );
}

/// The two-integer advisory locks this backend holds. The background lane's session
/// lease is a one-bigint key (`objsubid = 1`) and is not counted.
#[cfg(debug_assertions)]
async fn advisory_locks_held(conn: &mut PgConnection) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks \
         WHERE locktype = 'advisory' AND pid = pg_backend_pid() AND objsubid = 2 AND granted",
    )
    .fetch_one(&mut *conn)
    .await?)
}

/// The caller must already hold every key of `keys` (its single `locks::advisory` call).
/// Taking a key the transaction holds is a no-op, so the keys are taken again and the
/// number of advisory locks held must not grow.
#[cfg_attr(not(debug_assertions), allow(unused_variables, clippy::unused_async))]
pub(crate) async fn assert_keys_held(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    keys: &[AdvisoryKey],
    helper: &str,
) -> Result<()> {
    #[cfg(debug_assertions)]
    {
        let before = advisory_locks_held(tx).await?;
        crate::locks::advisory(tx, keys).await?;
        let after = advisory_locks_held(tx).await?;
        assert_eq!(
            before, after,
            "{helper}: the caller must hold the helper's advisory keys before the call \
             (one sorted locks::advisory call, before any row lock)"
        );
    }
    Ok(())
}

/// A source claim helper: the caller must hold the identity keys (exclusively) and the
/// source policy key in either mode ([`lock_source_claim`] takes it shared, a unit
/// built from the public `*_keys` functions takes it exclusively).
#[cfg_attr(not(debug_assertions), allow(unused_variables, clippy::unused_async))]
pub(crate) async fn assert_source_claim_held(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    identity_keys: &[AdvisoryKey],
    policy_source: &str,
    helper: &str,
) -> Result<()> {
    assert_keys_held(tx, identity_keys, helper).await?;
    #[cfg(debug_assertions)]
    {
        let (class, digest) = source_policy_ints(policy_source);
        // pg_locks shows the two keys as unsigned 32-bit values (classid, objid).
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_locks \
             WHERE locktype = 'advisory' AND pid = pg_backend_pid() AND objsubid = 2 AND granted \
               AND classid::bigint = ($1::bigint & 4294967295) \
               AND objid::bigint = ($2::bigint & 4294967295))",
        )
        .bind(class)
        .bind(digest)
        .fetch_one(&mut **tx)
        .await?;
        assert!(
            held,
            "{helper}: the caller must hold SourcePolicy({policy_source}) before the call"
        );
    }
    Ok(())
}

/// The helper takes a whole guard set starting at the users tier, so it must be the
/// first lock acquisition of its transaction: no advisory key and no row lock or write
/// (either assigns the transaction id) may precede it.
#[cfg_attr(not(debug_assertions), allow(unused_variables, clippy::unused_async))]
pub(crate) async fn assert_first_lock(conn: &mut PgConnection, helper: &str) -> Result<()> {
    assert_in_transaction(conn, helper);
    #[cfg(debug_assertions)]
    {
        let wrote: bool = sqlx::query_scalar("SELECT pg_current_xact_id_if_assigned() IS NOT NULL")
            .fetch_one(&mut *conn)
            .await?;
        let advisory = advisory_locks_held(conn).await?;
        assert!(
            !wrote && advisory == 0,
            "{helper} must be the first lock acquisition of its transaction \
             (row lock or write before it: {wrote}; advisory keys held: {advisory})"
        );
    }
    Ok(())
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEST_PAUSE: crate::locks::pause_points::PausePoints;
}

pub(crate) async fn held() {
    #[cfg(test)]
    pause("guard-held").await;
}

#[cfg(test)]
pub(crate) async fn pause(name: &str) {
    if let Ok(points) = TEST_PAUSE.try_with(Clone::clone) {
        points.hit(name).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared-mode policy lock and the refresh job lock compute a key's integers
    /// themselves. They must be the integers of the foundation's key, or a claim and a
    /// pin (or two refreshes) would take different locks.
    #[test]
    fn advisory_ints_match_the_foundation_keys() {
        for source in ["mangadex", "", "1234567890123456789", "ソース"] {
            let (class, digest) = source_policy_ints(source);
            assert_eq!(class, KeyClass::SourcePolicy as i32);
            assert_eq!(
                format!("{:?}", AdvisoryKey::new(KeyClass::SourcePolicy, &[source])),
                format!("AdvisoryKey {{ class: SourcePolicy, digest: {digest} }}"),
                "source {source:?}"
            );
        }
        let components = ["catalogue-refresh", "browse_catalogue"];
        let (class, digest) = advisory_ints(KeyClass::Job, &components);
        assert_eq!(class, KeyClass::Job as i32);
        assert_eq!(
            format!("{:?}", AdvisoryKey::new(KeyClass::Job, &components)),
            format!("AdvisoryKey {{ class: Job, digest: {digest} }}")
        );
        assert_ne!(
            advisory_ints(KeyClass::Job, &["catalogue-refresh", "feed_updates"]),
            (class, digest)
        );
    }
}
