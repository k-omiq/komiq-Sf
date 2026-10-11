//! Transaction lock protocol for every foreground and background writer.
//!
//! Global order: advisory **Membership** → users (sorted UUID) → all other
//! advisory keys (sorted `(class, digest)`) → works (sorted text ID) →
//! source_series (sorted text ID) → child rows (sorted natural key).
//!
//! Acquire the complete set at each tier before advancing. Do not discover a
//! smaller key and add it after taking a larger key, and never acquire a parent
//! after a child. Re-read identities, redirects, authority and predicates after
//! waiting. If discovery changes the required set, roll back and restart with
//! that complete set. Never hold these locks during HTTP, hashing or decoding.
//!
//! Parent helpers use FOR NO KEY UPDATE: protocol writers exclude each other
//! while implicit foreign-key FOR KEY SHARE holders remain compatible. For
//! DELETE/key changes use the for_delete variants (FOR UPDATE on works, users
//! or source rows). Multi-statement child writers lock the parent first.
//!
//! A single atomic child statement may rely on its foreign key ONLY where the
//! schema has a real one (23503 after merge/delete then means re-resolve and
//! try once more, not a server error). Real: source_series→work,
//! chapter→source_series, every work_* child→work, comments/votes/media/
//! sessions/library/progress→users, notifications→comments and users. Logical
//! only, never covered by that exception: user_library.series_id→work,
//! canonical_progress→work/chapter, comment and review targets. A write through
//! a logical parent needs the work/redirect guard: it succeeds silently after a
//! merge instead of failing with 23503.
//!
//! The explicit order does not make implicit FK/cascade locks acyclic. Every
//! transaction that can be the deadlock victim of such a cycle runs in
//! db::retry_transaction, not only the deleting one: user deletion against a
//! notification insert on that user's comment, work deletion against a child
//! insert. Either side may get 40P01.
//!
//! The background session lease precedes this order. Foreground writers never
//! acquire it. Advisory locks use PostgreSQL's two-i32 keyspace, disjoint from
//! the one-i64 session lease. Each class has its own first key; SHA-256 hashes
//! length-prefixed components into the second key. Hash collisions within a
//! class only cause extra serialization; classes cannot collide.

// Public guard-wave API; unused tiers are intentionally available before that wave lands.
#![allow(dead_code)]

use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};

/// Stable database protocol values. Append a new variant with a new number;
/// never renumber/reuse a class while old processes may still run.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(i32)]
pub enum KeyClass {
    Membership = 0x4b01,
    ExternalIdentity = 0x4b02,
    SourceIdentity = 0x4b03,
    LibraryProgress = 0x4b04,
    BlockRule = 0x4b05,
    ReviewPair = 0x4b06,
    Username = 0x4b07,
    Email = 0x4b08,
    SourcePolicy = 0x4b09,
    Series = 0x4b0a,
    Thread = 0x4b0b,
    Job = 0x4b0c,
    InstanceSetting = 0x4b0d,
    MatchDomain = 0x4b0e,
    Cover = 0x4b0f,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AdvisoryKey {
    class: KeyClass,
    digest: i32,
}

impl AdvisoryKey {
    /// New classes use this constructor. Components are length-prefixed, so
    /// `(ab,c)` and `(a,bc)` cannot be confused before hashing.
    pub fn new(class: KeyClass, components: &[&str]) -> Self {
        let mut hash = Sha256::new();
        for component in components {
            hash.update((component.len() as u64).to_be_bytes());
            hash.update(component.as_bytes());
        }
        let digest = hash.finalize();
        Self {
            class,
            digest: i32::from_be_bytes(digest[..4].try_into().expect("four digest bytes")),
        }
    }

    /// The protocol's two-i32 key for shared or session advisory lock forms.
    pub(crate) fn integers(self) -> (i32, i32) {
        (self.class as i32, self.digest)
    }

    pub fn membership() -> Self {
        Self::new(KeyClass::Membership, &["active-admins"])
    }
    pub fn external_identity(provider: &str, external_id: &str) -> Self {
        Self::new(KeyClass::ExternalIdentity, &[provider, external_id])
    }
    pub fn source_identity(source_id: &str, key: &str) -> Self {
        Self::new(KeyClass::SourceIdentity, &[source_id, key])
    }
    pub fn library_progress(user: &str, work: &str) -> Self {
        Self::new(KeyClass::LibraryProgress, &[user, work])
    }
    /// None means the site scope; Some is the canonical user UUID spelling.
    pub fn block_rule(user: Option<&str>) -> Self {
        match user {
            None => Self::new(KeyClass::BlockRule, &["site"]),
            Some(user) => Self::new(KeyClass::BlockRule, &["user", user]),
        }
    }
    /// Broad work pair, independent of source siblings and caller orientation.
    pub fn review_pair(left: &str, right: &str) -> Self {
        let (left, right) = if left <= right {
            (left, right)
        } else {
            (right, left)
        };
        Self::new(KeyClass::ReviewPair, &[left, right])
    }
    /// Preserve SQLite NOCASE: fold ASCII A-Z only, never Unicode case-fold.
    pub fn username(value: &str) -> Self {
        Self::new(KeyClass::Username, &[&value.to_ascii_lowercase()])
    }
    pub fn email(value: &str) -> Self {
        Self::new(KeyClass::Email, &[&value.to_ascii_lowercase()])
    }
}

/// Acquire membership separately, before users. Pass all nonmembership keys in
/// one call before any work lock. All locks are released on commit/rollback.
pub async fn advisory(
    tx: &mut Transaction<'_, Postgres>,
    keys: &[AdvisoryKey],
) -> Result<(), sqlx::Error> {
    let mut keys = keys.to_vec();
    keys.sort_unstable();
    keys.dedup();
    for key in keys {
        let result = sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
            .bind(key.class as i32)
            .bind(key.digest)
            .execute(&mut **tx)
            .await;
        result?;
    }
    Ok(())
}

/// Both sets are sorted and deduplicated. Missing parents are never silently
/// treated as locked; callers must abort, resolve redirects or adopt an identity.
#[derive(Debug, Eq, PartialEq)]
pub struct LockedRows {
    pub locked: Vec<String>,
    pub missing: Vec<String>,
}

async fn lock_text_rows<T: AsRef<str>>(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[T],
    sql: &str,
) -> Result<LockedRows, sqlx::Error> {
    let mut ids: Vec<String> = ids.iter().map(|id| id.as_ref().to_owned()).collect();
    ids.sort_unstable();
    ids.dedup();
    let mut result = LockedRows {
        locked: Vec::new(),
        missing: Vec::new(),
    };
    // Individual statements make acquisition order explicit even if a planner
    // changes a batch query's row order. Batches should remain bounded by caller.
    for id in ids {
        let row = sqlx::query_scalar::<_, String>(sql)
            .bind(&id)
            .fetch_optional(&mut **tx)
            .await;
        if row?.is_some() {
            result.locked.push(id);
        } else {
            result.missing.push(id);
        }
    }
    Ok(result)
}

/// Acquire all work parents in sorted ID order, before source/child rows.
pub async fn lock_works<T: AsRef<str>>(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[T],
) -> Result<LockedRows, sqlx::Error> {
    lock_text_rows(
        tx,
        ids,
        "SELECT id FROM work WHERE id = $1 FOR NO KEY UPDATE",
    )
    .await
}

/// DELETE/key changes require FOR UPDATE and db::retry_transaction because
/// foreign-key key-share holders can conflict despite explicit lock ordering.
pub async fn lock_works_for_delete<T: AsRef<str>>(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[T],
) -> Result<LockedRows, sqlx::Error> {
    lock_text_rows(tx, ids, "SELECT id FROM work WHERE id = $1 FOR UPDATE").await
}

/// Lock users before identity/work locks. Invalid UUIDs are errors, not misses.
pub async fn lock_users<T: AsRef<str>>(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[T],
) -> Result<LockedRows, sqlx::Error> {
    lock_users_with_strength(
        tx,
        ids,
        "SELECT id FROM users WHERE id = $1 FOR NO KEY UPDATE",
    )
    .await
}

/// DELETE/key changes require stronger parent locking and whole-unit retry.
pub async fn lock_users_for_delete<T: AsRef<str>>(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[T],
) -> Result<LockedRows, sqlx::Error> {
    lock_users_with_strength(tx, ids, "SELECT id FROM users WHERE id = $1 FOR UPDATE").await
}
async fn lock_users_with_strength<T: AsRef<str>>(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[T],
    sql: &str,
) -> Result<LockedRows, sqlx::Error> {
    let mut ids = ids
        .iter()
        .map(|id| {
            uuid::Uuid::parse_str(id.as_ref())
                .map_err(|_| sqlx::Error::Protocol("invalid user UUID in lock_users".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    ids.sort_unstable();
    ids.dedup();
    let mut result = LockedRows {
        locked: Vec::new(),
        missing: Vec::new(),
    };
    for id in ids {
        let row = sqlx::query_scalar::<_, uuid::Uuid>(sql)
            .bind(id)
            .fetch_optional(&mut **tx)
            .await;
        if row?.is_some() {
            result.locked.push(id.to_string());
        } else {
            result.missing.push(id.to_string());
        }
    }
    Ok(result)
}

pub async fn lock_user(tx: &mut Transaction<'_, Postgres>, id: &str) -> Result<bool, sqlx::Error> {
    Ok(lock_users(tx, &[id]).await?.missing.is_empty())
}

/// Discovery changed while acquiring locks. Roll back the complete unit and
/// rediscover on a fresh attempt; retry_transaction recognizes this type only.
#[derive(Debug)]
pub struct RestartTransaction;
impl std::fmt::Display for RestartTransaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("lock discovery changed; restart transaction")
    }
}
impl std::error::Error for RestartTransaction {}

#[cfg(test)]
tokio::task_local! {
    pub static SOURCE_DISCOVERY_PAUSE: pause_points::PausePoints;
}

/// Lock parents first, then source rows in sorted order. Call before locking any
/// source/child row, with the entire source set. This helper locks the work
/// parents ITSELF: call it once with the complete source set, never after a
/// partial work lock (a parent it discovers below an already locked work would
/// be taken out of order). A caller that already holds works must hold ALL of
/// these parents, or roll back and restart with the complete set.
/// A moved source after discovery requires rollback/restart, never locking its
/// newly discovered parent out of order. The error is the typed
/// RestartTransaction: return it intact (anyhow, `?`, `.context`) so
/// db::retry_transaction can recognise it.
pub async fn lock_source_series<T: AsRef<str>>(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[T],
) -> anyhow::Result<LockedRows> {
    lock_source_series_with_strength(
        tx,
        ids,
        "SELECT id FROM source_series WHERE id = $1 FOR NO KEY UPDATE",
    )
    .await
}

/// `lock_source_series` for deleting source rows or changing their key: the
/// same parent-first discovery, work parents with NO KEY UPDATE (they are not
/// deleted), source rows with FOR UPDATE so a concurrent child insert (chapter,
/// merge_candidate: real foreign keys) waits instead of racing the delete.
/// Run the unit, and every child writer that can meet it, in
/// db::retry_transaction: implicit key-share holders can still deadlock.
pub async fn lock_source_series_for_delete<T: AsRef<str>>(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[T],
) -> anyhow::Result<LockedRows> {
    lock_source_series_with_strength(
        tx,
        ids,
        "SELECT id FROM source_series WHERE id = $1 FOR UPDATE",
    )
    .await
}

async fn lock_source_series_with_strength<T: AsRef<str>>(
    tx: &mut Transaction<'_, Postgres>,
    ids: &[T],
    source_sql: &str,
) -> anyhow::Result<LockedRows> {
    let ids: Vec<String> = ids.iter().map(|id| id.as_ref().to_owned()).collect();
    let parents: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT work_id FROM source_series WHERE id = ANY($1) ORDER BY work_id",
    )
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?;
    #[cfg(test)]
    if let Ok(points) = SOURCE_DISCOVERY_PAUSE.try_with(Clone::clone) {
        points.hit("source-discovered").await;
    }
    let parents = lock_works(tx, &parents).await?;
    if !parents.missing.is_empty() {
        return Err(RestartTransaction.into());
    }
    let rows = lock_text_rows(tx, &ids, source_sql).await?;
    let current: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT work_id FROM source_series WHERE id = ANY($1) ORDER BY work_id",
    )
    .bind(&rows.locked)
    .fetch_all(&mut **tx)
    .await?;
    if current.iter().any(|id| !parents.locked.contains(id)) {
        return Err(RestartTransaction.into());
    }
    Ok(rows)
}

/// Independent test-owned named pauses. No globals or production pause points.
/// Guard tests pass one facility to both writers; names cannot cross test cases.
#[cfg(test)]
pub mod pause_points {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tokio::sync::Semaphore;

    #[derive(Default, Clone)]
    pub struct PausePoints(Arc<Mutex<HashMap<String, Arc<Point>>>>);
    struct Point {
        reached: Semaphore,
        release: Semaphore,
    }
    pub struct ArmedPoint(Arc<Point>);
    impl PausePoints {
        pub fn arm(&self, name: &str) -> ArmedPoint {
            let point = Arc::new(Point {
                reached: Semaphore::new(0),
                release: Semaphore::new(0),
            });
            assert!(
                self.0
                    .lock()
                    .unwrap()
                    .insert(name.into(), point.clone())
                    .is_none(),
                "point already armed"
            );
            ArmedPoint(point)
        }
        pub async fn hit(&self, name: &str) {
            let point = self.0.lock().unwrap().remove(name);
            if let Some(point) = point {
                point.reached.add_permits(1);
                point.release.acquire().await.unwrap().forget();
            }
        }
    }
    impl ArmedPoint {
        pub async fn reached(&self) {
            self.0.reached.acquire().await.unwrap().forget();
        }
        pub fn release(&self) {
            self.0.release.add_permits(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn advisory_keys_preserve_ascii_identity_and_pair_orientation() {
        assert_eq!(AdvisoryKey::username("AbCÄ"), AdvisoryKey::username("abcÄ"));
        assert_ne!(AdvisoryKey::username("Ä"), AdvisoryKey::username("ä"));
        assert_ne!(AdvisoryKey::username("same"), AdvisoryKey::email("same"));
        assert_eq!(
            AdvisoryKey::review_pair("w_b", "w_a"),
            AdvisoryKey::review_pair("w_a", "w_b")
        );
        assert_ne!(
            AdvisoryKey::new(KeyClass::SourceIdentity, &["ab", "c"]),
            AdvisoryKey::new(KeyClass::SourceIdentity, &["a", "bc"])
        );
    }
    #[tokio::test]
    async fn named_pause_points_block_and_release_once() {
        let points = pause_points::PausePoints::default();
        let armed = points.arm("after-read");
        let writer = tokio::spawn(async move {
            points.hit("after-read").await;
            points.hit("after-read").await;
        });
        armed.reached().await;
        assert!(!writer.is_finished());
        armed.release();
        writer.await.unwrap();
    }
}
