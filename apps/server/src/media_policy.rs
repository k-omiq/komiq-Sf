//! Media policy (M5 S5): the policy epoch, operator takedowns and the in-memory denied set.
//!
//! Three separate things decide what the owner serves (design section 3). Preferences
//! (NSFW, a reader's own blocks) never reach here. A **takedown** stops serving an object
//! to everyone: a work, or one copy (one upload of one chapter by one source). Site-wide
//! block rules close chapter pages too, and also count as policy.
//!
//! * [`bump`] moves the epoch inside the caller's transaction. Every change that can make
//!   a public object unservable calls it: a site-wide block rule change, a takedown added
//!   or lifted, a merge or split that carries a takedown.
//! * [`Policy`] keeps the denied set in memory. A request reads it without touching the
//!   database; the epoch is re-read at most every [`EPOCH_CHECK`] and the set is rebuilt
//!   when it moved, and every [`REBUILD_EVERY`] anyway (a work's tags can change without
//!   a rule change).
//! * [`Snapshot::lease`] is the signed policy lease body for serving caches (M6).
//!
//! Takedowns never shrink on catalogue changes: a work takedown also records a copy row
//! for each copy the work has at that moment, a merge moves the losing work's takedown
//! onto the survivor ([`carry_merge`]), a split copies it to the new work
//! ([`carry_split`]), and the denied works include every old id that redirects to a
//! denied work (cover URLs keep the original id).

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use sqlx::{Postgres, Transaction};


use crate::db::Database;
use crate::media_verify::LeaseBody;

/// How long a signed policy lease is valid. Exact objects carry the remaining life of the
/// current lease as `s-maxage`, so no chain of caches serves a takedown longer than this.
pub(crate) const LEASE_SECS: i64 = 600;
/// How often a request re-reads the epoch (one primary-key read).
pub(crate) const EPOCH_CHECK: Duration = Duration::from_secs(5);
/// How often the set is rebuilt even when the epoch did not move.
pub(crate) const REBUILD_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Work,
    Copy,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::Work => "work",
            Kind::Copy => "copy",
        }
    }
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw {
            "work" => Some(Kind::Work),
            "copy" => Some(Kind::Copy),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct Takedown {
    pub subject_kind: String,
    pub subject_id: String,
    pub epoch: i64,
    pub reason: String,
    pub created_by: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub via_work: Option<String>,
}

/// The current epoch: 0 before the first policy change.
pub(crate) async fn epoch<'e, E>(executor: E) -> Result<i64>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(
        sqlx::query_scalar::<_, i64>("SELECT epoch FROM media_policy WHERE singleton")
            .fetch_optional(executor)
            .await?
            .unwrap_or(0),
    )
}

/// Hold the epoch row until publication commits, so a policy change sees its copy.
pub(crate) async fn hold_epoch(tx: &mut Transaction<'_, Postgres>) -> Result<i64> {
    let epoch = sqlx::query_scalar::<_, i64>(
        "SELECT epoch FROM media_policy WHERE singleton FOR SHARE",
    )
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(epoch) = epoch {
        return Ok(epoch);
    }
    sqlx::query(
        "INSERT INTO media_policy (singleton, epoch) VALUES (true, 0) \
         ON CONFLICT (singleton) DO NOTHING",
    )
    .execute(&mut **tx)
    .await?;
    Ok(sqlx::query_scalar(
        "SELECT epoch FROM media_policy WHERE singleton FOR SHARE",
    )
    .fetch_one(&mut **tx)
    .await?)
}

/// Move the epoch inside the caller's transaction and return the new value. The row lock
/// orders concurrent policy changes. The new epoch is at least the current time in
/// milliseconds, so it keeps rising after an import resets the table.
pub(crate) async fn bump(tx: &mut Transaction<'_, Postgres>) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "INSERT INTO media_policy (singleton, epoch) VALUES (true, $1) \
         ON CONFLICT (singleton) DO UPDATE SET epoch = GREATEST(media_policy.epoch + 1, $1) \
         RETURNING epoch",
    )
    .bind(chrono::Utc::now().timestamp_millis())
    .fetch_one(&mut **tx)
    .await?)
}

/// Generations whose stored keys still describe the catalogue's numeric series ids: the
/// active one, and a quarantined one (its engine is gone, its mappings are not stale).
const LIVE_GENERATION: &str = "g.state IN ('active', 'quarantined')";

/// Record a copy takedown, owned by `work`'s takedown, for every copy the work has now.
/// Called inside the transaction that takes the work down or carries its takedown.
async fn record_work_copies(
    tx: &mut Transaction<'_, Postgres>,
    work: &str,
    epoch: i64,
) -> Result<()> {
    sqlx::query(&format!(
        "INSERT INTO media_takedown \
           (subject_kind, subject_id, epoch, reason, created_by, created_at, via_work) \
         SELECT DISTINCT 'copy', cp.id, $2, t.reason, t.created_by, now(), $1 \
         FROM media_takedown t, media_copy cp \
         JOIN engine_manga_key m ON m.source_id = cp.source_id AND m.manga_key = cp.manga_key \
         JOIN engine_generation g ON g.id = m.generation AND {LIVE_GENERATION} \
         JOIN source_series ss ON ss.source_type = 'suwayomi' \
           AND ss.source_key = m.manga_id::text AND ss.work_id = $1 \
         WHERE t.subject_kind = 'work' AND t.subject_id = $1 \
         ON CONFLICT (subject_kind, subject_id) DO NOTHING"
    ))
    .bind(work)
    .bind(epoch)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Record a new copy under its work's takedown while the caller holds the epoch row.
pub(crate) async fn inherit_for_copy(
    tx: &mut Transaction<'_, Postgres>,
    copy_id: &str,
) -> Result<Option<i64>> {
    let takedown: Option<(String, String, Option<String>)> = sqlx::query_as(&format!(
        "SELECT t.subject_id, t.reason, t.created_by FROM media_copy cp \
         JOIN engine_manga_key m ON m.source_id = cp.source_id AND m.manga_key = cp.manga_key \
         JOIN engine_generation g ON g.id = m.generation AND {LIVE_GENERATION} \
         JOIN source_series ss ON ss.source_type = 'suwayomi' \
           AND ss.source_key = m.manga_id::text \
         JOIN media_takedown t ON t.subject_kind = 'work' AND t.subject_id = ss.work_id \
         WHERE cp.id = $1 AND NOT EXISTS (SELECT 1 FROM media_takedown \
           WHERE subject_kind = 'copy' AND subject_id = $1) LIMIT 1"
    ))
    .bind(copy_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((work, reason, created_by)) = takedown else {
        return Ok(None);
    };
    let epoch = bump(tx).await?;
    sqlx::query(
        "INSERT INTO media_takedown \
           (subject_kind, subject_id, epoch, reason, created_by, created_at, via_work) \
         VALUES ('copy', $1, $2, $3, $4, now(), $5) \
         ON CONFLICT (subject_kind, subject_id) DO NOTHING",
    )
    .bind(copy_id)
    .bind(epoch)
    .bind(reason)
    .bind(created_by)
    .bind(work)
    .execute(&mut **tx)
    .await?;
    Ok(Some(epoch))
}

/// Whether an engine series belongs to a taken-down work.
pub(crate) async fn engine_series_taken_down<'e, E>(executor: E, manga_id: i64) -> Result<bool>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM source_series ss \
         JOIN media_takedown t ON t.subject_kind = 'work' AND t.subject_id = ss.work_id \
         WHERE ss.source_type = 'suwayomi' AND ss.source_key = $1)",
    )
    .bind(manga_id.to_string())
    .fetch_one(executor)
    .await?)
}

fn valid_copy_id(id: &str) -> bool {
    id.strip_prefix("mc_").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Take down a work or a copy. A work id that was merged away is resolved to its
/// survivor. A work takedown also records a copy row for each copy the work has now.
/// Returns the subject actually recorded, and whether anything changed (an existing
/// takedown is left as it is). The caller commits, then deletes the work's cover
/// ([`after_work_takedown`]).
pub(crate) async fn add(
    tx: &mut Transaction<'_, Postgres>,
    kind: Kind,
    subject: &str,
    reason: &str,
    created_by: Option<&str>,
) -> Result<(String, bool)> {
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("a takedown needs a reason");
    }
    if reason.chars().count() > 500 {
        bail!("a takedown reason is at most 500 characters");
    }
    let subject = match kind {
        Kind::Work => {
            let id: String = sqlx::query_scalar(
                "SELECT COALESCE((SELECT new_id FROM work_redirect WHERE old_id = $1), $1)",
            )
            .bind(subject)
            .fetch_one(&mut **tx)
            .await?;
            // The work row lock orders this with a merge or split of the same work, so
            // neither can miss the other's takedown.
            if !crate::locks::lock_works(tx, &[id.as_str()])
                .await?
                .missing
                .is_empty()
            {
                bail!("no such work: {subject}");
            }
            id
        }
        Kind::Copy => {
            let exists = valid_copy_id(subject)
                && sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM media_copy WHERE id = $1)",
                )
                .bind(subject)
                .fetch_one(&mut **tx)
                .await?;
            if !exists {
                bail!("no such copy: {subject}");
            }
            subject.to_string()
        }
    };
    let already: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM media_takedown WHERE subject_kind = $1 AND subject_id = $2)",
    )
    .bind(kind.as_str())
    .bind(&subject)
    .fetch_one(&mut **tx)
    .await?;
    if already {
        return Ok((subject, false));
    }
    let epoch = bump(tx).await?;
    let inserted = sqlx::query(
        "INSERT INTO media_takedown \
           (subject_kind, subject_id, epoch, reason, created_by, created_at) \
         VALUES ($1, $2, $3, $4, $5, now()) \
         ON CONFLICT (subject_kind, subject_id) DO NOTHING",
    )
    .bind(kind.as_str())
    .bind(&subject)
    .bind(epoch)
    .bind(reason)
    .bind(created_by)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Ok((subject, false));
    }
    if kind == Kind::Work {
        // Its copies as they are now, so a later catalogue change (a new engine, a
        // mapping moved) cannot reopen them.
        record_work_copies(tx, &subject, epoch).await?;
    }
    Ok((subject, true))
}

/// Lift a takedown. Lifting a work also lifts the copy rows recorded for it. Returns
/// whether anything was lifted; the caller rolls back when nothing was, so the epoch
/// only moves for a real change.
/// Lock order: work rows, then the epoch row, then takedown rows.
pub(crate) async fn remove(
    tx: &mut Transaction<'_, Postgres>,
    kind: Kind,
    subject: &str,
) -> Result<bool> {
    if kind == Kind::Work {
        crate::locks::lock_works(tx, &[subject]).await?;
    }
    bump(tx).await?;
    let lifted = sqlx::query(
        "DELETE FROM media_takedown WHERE (subject_kind = $1 AND subject_id = $2) \
            OR ($1 = 'work' AND subject_kind = 'copy' AND via_work = $2)",
    )
    .bind(kind.as_str())
    .bind(subject)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(lifted > 0)
}

pub(crate) async fn list(pool: &Database) -> Result<Vec<Takedown>> {
    Ok(sqlx::query_as(
        "SELECT subject_kind, subject_id, epoch, reason, created_by, created_at, via_work \
         FROM media_takedown ORDER BY created_at DESC, subject_kind, subject_id",
    )
    .fetch_all(pool)
    .await?)
}

/// Inside `merge_works_on`, after the sources moved and the redirect is written: the
/// losing work's takedown moves onto the survivor, copy rows recorded for it follow, and
/// the copies the survivor absorbed are recorded. Bumps the epoch when either side is
/// taken down (the losing id now redirects to a denied work).
pub(crate) async fn carry_merge(
    tx: &mut Transaction<'_, Postgres>,
    source_work: &str,
    target_work: &str,
) -> Result<()> {
    let denied: Vec<String> = sqlx::query_scalar(
        "SELECT subject_id FROM media_takedown \
         WHERE subject_kind = 'work' AND subject_id IN ($1, $2)",
    )
    .bind(source_work)
    .bind(target_work)
    .fetch_all(&mut **tx)
    .await?;
    if denied.is_empty() {
        return Ok(());
    }
    let epoch = bump(tx).await?;
    if denied.iter().any(|id| id == source_work) {
        if denied.iter().any(|id| id == target_work) {
            sqlx::query(
                "DELETE FROM media_takedown WHERE subject_kind = 'work' AND subject_id = $1",
            )
            .bind(source_work)
            .execute(&mut **tx)
            .await?;
        } else {
            sqlx::query(
                "UPDATE media_takedown SET subject_id = $2, epoch = $3 \
                 WHERE subject_kind = 'work' AND subject_id = $1",
            )
            .bind(source_work)
            .bind(target_work)
            .bind(epoch)
            .execute(&mut **tx)
            .await?;
        }
        sqlx::query("UPDATE media_takedown SET via_work = $2 WHERE via_work = $1")
            .bind(source_work)
            .bind(target_work)
            .execute(&mut **tx)
            .await?;
    }
    // The survivor is taken down now: the copies it absorbed are recorded too.
    record_work_copies(tx, target_work, epoch).await?;
    Ok(())
}

/// Inside `split_source_series`, after the new work exists: a takedown of the original
/// is copied to the new work. Lifting either side later is a separate admin act.
pub(crate) async fn carry_split(
    tx: &mut Transaction<'_, Postgres>,
    work_id: &str,
    new_work_id: &str,
) -> Result<()> {
    let denied: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM media_takedown \
         WHERE subject_kind = 'work' AND subject_id = $1)",
    )
    .bind(work_id)
    .fetch_one(&mut **tx)
    .await?;
    if !denied {
        return Ok(());
    }
    let epoch = bump(tx).await?;
    sqlx::query(
        "INSERT INTO media_takedown \
           (subject_kind, subject_id, epoch, reason, created_by, created_at) \
         SELECT 'work', $2, $3, 'split from ' || $1 || ': ' || reason, created_by, now() \
         FROM media_takedown WHERE subject_kind = 'work' AND subject_id = $1 \
         ON CONFLICT (subject_kind, subject_id) DO NOTHING",
    )
    .bind(work_id)
    .bind(new_work_id)
    .bind(epoch)
    .execute(&mut **tx)
    .await?;
    // The copies that moved now belong to the new work's takedown, so lifting either
    // side lifts that side's copies only.
    sqlx::query(&format!(
        "UPDATE media_takedown SET via_work = $2 \
         WHERE subject_kind = 'copy' AND via_work = $1 AND subject_id IN ( \
           SELECT cp.id FROM media_copy cp \
           JOIN engine_manga_key m ON m.source_id = cp.source_id AND m.manga_key = cp.manga_key \
           JOIN engine_generation g ON g.id = m.generation AND {LIVE_GENERATION} \
           JOIN source_series ss ON ss.source_type = 'suwayomi' \
             AND ss.source_key = m.manga_id::text AND ss.work_id = $2)"
    ))
    .bind(work_id)
    .bind(new_work_id)
    .execute(&mut **tx)
    .await?;
    record_work_copies(tx, new_work_id, epoch).await?;
    Ok(())
}

/// After a work takedown commits: delete the work's cover blobs, under its id and every
/// old id that redirects to it, and the engine covers of its series. Cross-database, so
/// best effort: the cover routes refuse a denied work whether or not this ran.
pub(crate) async fn after_work_takedown(
    pool: &Database,
    covers: &crate::db::SqliteDatabase,
    work_id: &str,
) -> Result<()> {
    let mut works: Vec<String> =
        sqlx::query_scalar("SELECT old_id FROM work_redirect WHERE new_id = $1")
            .bind(work_id)
            .fetch_all(pool)
            .await?;
    works.push(work_id.to_string());
    let manga: Vec<String> = sqlx::query_scalar(
        "SELECT source_key FROM source_series \
         WHERE source_type = 'suwayomi' AND work_id = ANY($1)",
    )
    .bind(&works)
    .fetch_all(pool)
    .await?;
    let manga: Vec<i64> = manga.iter().filter_map(|key| key.parse().ok()).collect();
    crate::cover::delete_for_takedown(pool, covers, &works, &manga).await
}

/// Take down `subject` and, for a work, delete its covers once the takedown is durable.
/// Shared by the admin mutation and the CLI. Returns the subject recorded and whether
/// anything changed.
pub(crate) async fn take_down(
    pool: &Database,
    covers: &crate::db::SqliteDatabase,
    kind: Kind,
    subject: &str,
    reason: &str,
    created_by: Option<&str>,
) -> Result<(String, bool)> {
    let (subject, changed) = crate::db::retry_transaction(|| async {
        let mut tx = pool.begin().await?;
        let outcome = add(&mut tx, kind, subject, reason, created_by).await?;
        if outcome.1 {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        Ok(outcome)
    })
    .await?;
    if kind == Kind::Work {
        // Also when nothing changed: a retry deletes what an earlier failed run left.
        if let Err(error) = after_work_takedown(pool, covers, &subject).await {
            tracing::warn!(error = %error, work = %subject, "takedown: cover delete failed");
        }
    }
    Ok((subject, changed))
}

/// Lift a takedown. Shared by the admin mutation and the CLI.
pub(crate) async fn lift(pool: &Database, kind: Kind, subject: &str) -> Result<bool> {
    crate::db::retry_transaction(|| async {
        let mut tx = pool.begin().await?;
        let lifted = remove(&mut tx, kind, subject).await?;
        if lifted {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        Ok(lifted)
    })
    .await
}

const CLI_USAGE: &str = "usage: komika-server takedown add work|copy <id> --reason <text>\n\
       komika-server takedown remove work|copy <id>\n\
       komika-server takedown list";

/// `komika-server takedown add|remove|list`. A running server sees the change within
/// `EPOCH_CHECK`.
pub(crate) async fn run_cli(
    pool: &Database,
    covers: &crate::db::SqliteDatabase,
    args: &[String],
) -> Result<()> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["list"] => {
            for t in list(pool).await? {
                println!(
                    "{}\t{}\t{}\t{}\t{}{}",
                    t.subject_kind,
                    t.subject_id,
                    t.created_at.to_rfc3339(),
                    t.created_by.as_deref().unwrap_or("-"),
                    t.reason,
                    t.via_work
                        .map(|work| format!(" (for work {work})"))
                        .unwrap_or_default(),
                );
            }
            Ok(())
        }
        ["add", kind, id, "--reason", reason] => {
            let kind = Kind::parse(kind).ok_or_else(|| anyhow::anyhow!(CLI_USAGE))?;
            let (subject, changed) = take_down(pool, covers, kind, id, reason, Some("cli")).await?;
            if changed {
                println!("taken down: {} {subject}", kind.as_str());
            } else {
                println!("already taken down: {} {subject}", kind.as_str());
            }
            Ok(())
        }
        ["remove", kind, id] => {
            let kind = Kind::parse(kind).ok_or_else(|| anyhow::anyhow!(CLI_USAGE))?;
            if lift(pool, kind, id).await? {
                println!("lifted: {} {id}", kind.as_str());
            } else {
                println!("no takedown: {} {id}", kind.as_str());
            }
            Ok(())
        }
        _ => bail!(CLI_USAGE),
    }
}

/// The denied set at one epoch. Built from the database by [`Snapshot::load`].
#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    pub epoch: i64,
    /// The engine generation (id and state) the numeric sets below were read under.
    generation: Option<String>,
    /// Taken-down works, plus every old id that redirects to one.
    pub denied_works: BTreeSet<String>,
    /// Taken-down copies, plus every copy of a series whose pages are refused.
    pub denied_copies: BTreeSet<String>,
    /// Engine series of taken-down works: no pages, no covers.
    takedown_manga: HashSet<i64>,
    /// Engine series a site-wide block rule closes: no pages (covers stay open).
    blocked_manga: HashSet<i64>,
    /// Engine chapters of taken-down copies, and the series they are in.
    denied_chapters: HashSet<i64>,
    copy_manga: HashSet<i64>,
}

/// The epoch and the current engine generation: what [`Policy::current`] compares every
/// [`EPOCH_CHECK`]. A new or quarantined generation renumbers the engine's ids, so it
/// rebuilds the numeric sets as an epoch change does.
async fn marker(pool: &Database) -> Result<(i64, Option<String>)> {
    Ok(sqlx::query_as(
        "SELECT COALESCE((SELECT epoch FROM media_policy WHERE singleton), 0), \
                (SELECT id::text || ':' || state FROM engine_generation \
                 WHERE state <> 'detached' LIMIT 1)",
    )
    .fetch_one(pool)
    .await?)
}

impl Snapshot {
    /// Reads the epoch first: a policy change that commits while the sets are read is
    /// then picked up by the next epoch check, never hidden under a newer epoch.
    /// With exact media off (`exact_media` false) the copy tables are read only when a
    /// copy takedown exists, so a server that never made a copy never reads them.
    pub(crate) async fn load(pool: &Database, exact_media: bool) -> Result<Self> {
        let (epoch, generation) = marker(pool).await?;
        let site_rules = crate::block::load(pool, None).await?;
        let works: Vec<String> = sqlx::query_scalar(
            "SELECT subject_id FROM media_takedown WHERE subject_kind = 'work' \
             UNION SELECT r.old_id FROM work_redirect r JOIN media_takedown t \
               ON t.subject_kind = 'work' AND t.subject_id = r.new_id",
        )
        .fetch_all(pool)
        .await?;
        let parse = |keys: Vec<String>| -> HashSet<i64> {
            keys.iter().filter_map(|key| key.parse().ok()).collect()
        };
        let takedown_manga = parse(
            sqlx::query_scalar(
                "SELECT source_key FROM source_series \
                 WHERE source_type = 'suwayomi' AND work_id = ANY($1)",
            )
            .bind(&works)
            .fetch_all(pool)
            .await?,
        );
        let blocked_manga = match site_rules.blocked_sql("ss.work_id", None) {
            Some(blocked) => parse(
                sqlx::query_scalar(&format!(
                    "SELECT ss.source_key FROM source_series ss \
                     WHERE ss.source_type = 'suwayomi' AND {blocked}"
                ))
                .fetch_all(pool)
                .await?,
            ),
            None => HashSet::new(),
        };
        let mut denied_copies: BTreeSet<String> = sqlx::query_scalar(
            "SELECT subject_id FROM media_takedown WHERE subject_kind = 'copy'",
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
        let closed: Vec<i64> = takedown_manga.union(&blocked_manga).copied().collect();
        if exact_media && !closed.is_empty() {
            // For the lease and the manifest route: every copy of a closed series.
            denied_copies.extend(
                sqlx::query_scalar::<_, String>(&format!(
                    "SELECT cp.id FROM media_copy cp \
                     JOIN engine_manga_key m ON m.source_id = cp.source_id \
                       AND m.manga_key = cp.manga_key \
                     JOIN engine_generation g ON g.id = m.generation AND {LIVE_GENERATION} \
                     WHERE m.manga_id = ANY($1)",
                ))
                .bind(&closed)
                .fetch_all(pool)
                .await?,
            );
        }
        let chapters: Vec<(i64, i64)> = if denied_copies.is_empty() {
            Vec::new()
        } else {
            sqlx::query_as(
            "SELECT c.chapter_id, c.manga_id FROM media_takedown t \
             JOIN media_copy cp ON cp.id = t.subject_id \
             JOIN engine_manga_key m ON m.source_id = cp.source_id AND m.manga_key = cp.manga_key \
             JOIN engine_generation g ON g.id = m.generation AND g.state = 'active' \
             JOIN engine_chapter_key c ON c.generation = m.generation \
               AND c.manga_id = m.manga_id AND c.chapter_key = cp.chapter_key \
             WHERE t.subject_kind = 'copy'",
            )
            .fetch_all(pool)
            .await?
        };
        Ok(Self {
            epoch,
            generation,
            denied_works: works.into_iter().collect(),
            denied_copies,
            takedown_manga,
            blocked_manga,
            denied_chapters: chapters.iter().map(|(chapter, _)| *chapter).collect(),
            copy_manga: chapters.iter().map(|(_, manga)| *manga).collect(),
        })
    }

    /// Any work taken down: the GraphQL list gates then add their takedown clause.
    pub(crate) fn has_work_takedowns(&self) -> bool {
        !self.denied_works.is_empty()
    }
    pub(crate) fn denies_work(&self, work_id: &str) -> bool {
        self.denied_works.contains(work_id)
    }
    pub(crate) fn denies_copy(&self, copy_id: &str) -> bool {
        self.denied_copies.contains(copy_id)
    }
    /// No chapter page of this engine series: taken down, or closed by a site-wide rule.
    pub(crate) fn denies_pages(&self, manga_id: i64) -> bool {
        self.takedown_manga.contains(&manga_id) || self.blocked_manga.contains(&manga_id)
    }
    /// Whether any engine series has its pages closed.
    pub(crate) fn closes_any_series(&self) -> bool {
        !self.takedown_manga.is_empty() || !self.blocked_manga.is_empty()
    }
    /// No cover of this engine series: only a takedown closes covers.
    pub(crate) fn denies_engine_cover(&self, manga_id: i64) -> bool {
        self.takedown_manga.contains(&manga_id)
    }
    pub(crate) fn denies_chapter(&self, chapter_id: i64) -> bool {
        self.denied_chapters.contains(&chapter_id)
    }
    /// A legacy page path names its chapter by index, which a source re-order moves
    /// without any policy change. So it is refused for the whole series while one of its
    /// chapter copies is taken down; the chapter-id path (`/media/v1/c`) is exact.
    pub(crate) fn denies_index_path(&self, manga_id: i64) -> bool {
        self.copy_manga.contains(&manga_id)
    }

    /// The lease body for `issued_at` (the start of the current minute). Unsigned;
    /// `kid` is filled by the signer's caller.
    pub(crate) fn lease(&self, issued_at: i64, kid: &str) -> LeaseBody {
        LeaseBody {
            v: 1,
            epoch: self.epoch,
            issued_at,
            expires_at: issued_at + LEASE_SECS,
            denied_copies: self.denied_copies.iter().cloned().collect(),
            denied_works: self.denied_works.iter().cloned().collect(),
            kid: kid.to_string(),
        }
    }
}

struct Held {
    snapshot: Arc<Snapshot>,
    loaded: Instant,
    checked: Instant,
}

/// The owner's denied set, shared by every request. See the module comment.
#[derive(Default)]
pub(crate) struct Policy {
    /// Exact media on (`KOMIKA_MEDIA_MANIFESTS`): copies of closed series are listed.
    exact_media: bool,
    held: Mutex<Option<Held>>,
    reload: tokio::sync::Mutex<()>,
    /// The signed lease for (epoch, minute).
    lease: Mutex<Option<(i64, i64, Arc<Vec<u8>>)>>,
}

impl Policy {
    pub(crate) fn new(exact_media: bool) -> Self {
        Self {
            exact_media,
            ..Self::default()
        }
    }

    fn fresh(&self) -> Option<Arc<Snapshot>> {
        let held = self.held.lock().unwrap();
        held.as_ref()
            .filter(|held| held.checked.elapsed() < EPOCH_CHECK)
            .map(|held| held.snapshot.clone())
    }

    /// The denied set, re-checked against the epoch at most every [`EPOCH_CHECK`]. An
    /// error means the set could not be read: callers fail closed.
    pub(crate) async fn current(&self, pool: &Database) -> Result<Arc<Snapshot>> {
        if let Some(snapshot) = self.fresh() {
            return Ok(snapshot);
        }
        let _reload = self.reload.lock().await;
        if let Some(snapshot) = self.fresh() {
            return Ok(snapshot);
        }
        let (epoch, generation) = marker(pool).await?;
        {
            let mut held = self.held.lock().unwrap();
            if let Some(held) = held.as_mut().filter(|held| {
                held.snapshot.epoch == epoch
                    && held.snapshot.generation == generation
                    && held.loaded.elapsed() < REBUILD_EVERY
            })
            {
                held.checked = Instant::now();
                return Ok(held.snapshot.clone());
            }
        }
        let snapshot = Arc::new(Snapshot::load(pool, self.exact_media).await?);
        let now = Instant::now();
        *self.held.lock().unwrap() = Some(Held {
            snapshot: snapshot.clone(),
            loaded: now,
            checked: now,
        });
        Ok(snapshot)
    }

    /// After a policy change in this process commits: the next request re-reads the
    /// epoch instead of waiting for [`EPOCH_CHECK`].
    pub(crate) fn invalidate(&self) {
        if let Some(held) = self.held.lock().unwrap().as_mut() {
            held.checked = Instant::now() - EPOCH_CHECK;
        }
    }

    /// The signed lease, memoized per epoch and per minute. `now` is unix seconds.
    pub(crate) fn signed_lease(
        &self,
        snapshot: &Snapshot,
        signer: &crate::media_manifest::Signer,
        now: i64,
    ) -> Result<Arc<Vec<u8>>> {
        let minute = now.div_euclid(60);
        let mut memo = self.lease.lock().unwrap();
        if let Some((epoch, at, bytes)) = memo.as_ref() {
            if *epoch == snapshot.epoch && *at == minute {
                return Ok(bytes.clone());
            }
        }
        let bytes = Arc::new(signer.sign(&snapshot.lease(minute * 60, &signer.kid))?);
        *memo = Some((snapshot.epoch, minute, bytes.clone()));
        Ok(bytes)
    }
}

/// The remaining life of the lease the owner issues at `now` (unix seconds): what an
/// exact object carries as `s-maxage`.
pub(crate) fn lease_remaining(now: i64) -> i64 {
    now.div_euclid(60) * 60 + LEASE_SECS - now
}
