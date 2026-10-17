//! Whole-job ownership for background jobs, across every server on one database.
//!
//! A [`JobLease`] is a session advisory lock `Job["job-lease", family, ...]` taken on
//! a connection of its own and held for the whole pass. The writer lanes
//! (`database.rs`) only order write turns; they never said which server runs a job.
//! In-process flags said it for one process only. The lease says it for every
//! process: a second server, a manual run or a second loop finds it held and skips
//! or refuses, as the flags made them do inside one process.
//!
//! The lock lives as long as the lease's session. Dropping the lease closes the
//! connection, and a crashed server's session ends with its socket, so a crash never
//! strands a job. A lease does not notice a lost session while its job runs (a
//! database restart): the job finishes its pass, and another owner may start beside
//! it in that window. The jobs keep their row-level guards for that reason.
//!
//! The lease connection is outside every pool, so a long pass never holds a writer
//! connection just to stay the owner.

use sqlx::Connection;

use crate::db::Database;

/// The jobs that must not overlap themselves. One family is one key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobFamily<'a> {
    /// The MangaDex catalogue cycle, the chapter cycle, the admin reseed and the
    /// catalogue gap backfill. They share the catalogue and chapter cursors.
    MangadexCatalogue,
    /// The cover drainer's crawl and the admin's full crawl.
    CoverCrawl,
    /// The one-time copy of the SQLite cover cache into files.
    CoverCopy,
    /// Provisional reconciliation and duplicate consolidation, from the admin and
    /// after the gap backfill.
    Reconcile,
    /// One extension update pass, automatic or manual.
    ExtensionUpdate,
    /// The scheduled source-sync pass.
    SourceSync,
    /// One LATEST-diff discovery pass.
    Discovery,
    /// The ingest job of one source. Held from before the job row exists until
    /// after it is finished, so a `running` row whose lease is free has no owner.
    SourceIngest(&'a str),
}

impl JobFamily<'_> {
    fn key(self) -> (i32, i32) {
        let components: &[&str] = match self {
            Self::MangadexCatalogue => &["job-lease", "mangadex-catalogue"],
            Self::CoverCrawl => &["job-lease", "cover-crawl"],
            Self::CoverCopy => &["job-lease", "cover-copy"],
            Self::Reconcile => &["job-lease", "reconcile"],
            Self::ExtensionUpdate => &["job-lease", "extension-update"],
            Self::SourceSync => &["job-lease", "source-sync"],
            Self::Discovery => &["job-lease", "discovery"],
            Self::SourceIngest(source) => &["job-lease", "source-ingest", source],
        };
        crate::catalog::guards::advisory_ints(crate::locks::KeyClass::Job, components)
    }
}

/// Ownership of one job family. See the module comment.
#[must_use = "the job is owned only while the lease is held"]
pub(crate) struct JobLease {
    connection: Option<sqlx::PgConnection>,
}

impl std::fmt::Debug for JobLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobLease").finish_non_exhaustive()
    }
}

impl JobLease {
    /// Take the family's lease, or `None` while another session holds it. Never
    /// waits for the holder.
    pub(crate) async fn try_acquire(
        db: &Database,
        family: JobFamily<'_>,
    ) -> anyhow::Result<Option<Self>> {
        let mut connection = sqlx::PgConnection::connect_with(&db.connect_options()).await?;
        crate::db::apply_session_limits(&mut connection, crate::db::Lane::Background).await?;
        let (class, digest) = family.key();
        let taken: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, $2)")
            .bind(class)
            .bind(digest)
            .fetch_one(&mut connection)
            .await?;
        if !taken {
            let _ = connection.close().await;
            return Ok(None);
        }
        Ok(Some(Self {
            connection: Some(connection),
        }))
    }

    /// [`Self::try_acquire`] for a recurring loop: a failure to reach the database
    /// is logged and treated like a held lease (skip this pass, try the next).
    pub(crate) async fn try_take(db: &Database, family: JobFamily<'_>) -> Option<Self> {
        match Self::try_acquire(db, family).await {
            Ok(lease) => lease,
            Err(error) => {
                crate::db::observe_database_error(&error);
                tracing::warn!(?family, %error, "job lease: could not ask the database; skipping this pass");
                None
            }
        }
    }

    /// Whether the family's lease is free right now (nobody owns the job). The
    /// answer can change as soon as it is given.
    #[cfg(test)]
    pub(crate) async fn is_free(db: &Database, family: JobFamily<'_>) -> anyhow::Result<bool> {
        Ok(match Self::try_acquire(db, family).await? {
            Some(lease) => {
                lease.release().await;
                true
            }
            None => false,
        })
    }

    /// Unlock and close the session now: the lock is free when this returns.
    /// Dropping the lease frees it a moment later (the socket closes and PostgreSQL
    /// ends the session).
    pub(crate) async fn release(mut self) {
        if let Some(mut connection) = self.connection.take() {
            let _ = sqlx::query("SELECT pg_advisory_unlock_all()")
                .execute(&mut connection)
                .await;
            let _ = connection.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Two servers on one database are two `Database` handles with their own pools,
    /// as two `AppState`s are.
    async fn second_server(first: &Database) -> Database {
        Database::connect_with((*first.connect_options()).clone(), 2)
            .await
            .unwrap()
    }

    async fn eventually_free(db: &Database, family: JobFamily<'_>) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !JobLease::is_free(db, family).await.unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok()
    }

    #[tokio::test]
    async fn a_lease_excludes_the_same_family_on_another_server_until_released() {
        let one = Database::connect("sqlite::memory:").await.unwrap();
        let two = second_server(&one).await;
        let held = JobLease::try_acquire(&one, JobFamily::MangadexCatalogue)
            .await
            .unwrap()
            .expect("free at first");
        assert!(
            JobLease::try_acquire(&two, JobFamily::MangadexCatalogue)
                .await
                .unwrap()
                .is_none(),
            "the other server must see the job owned"
        );
        assert!(
            JobLease::try_acquire(&one, JobFamily::MangadexCatalogue)
                .await
                .unwrap()
                .is_none(),
            "a second loop in the same server must see it owned too"
        );
        // Other families and other sources are independent.
        let cover = JobLease::try_acquire(&two, JobFamily::CoverCrawl)
            .await
            .unwrap()
            .expect("another family is free");
        let a = JobLease::try_acquire(&two, JobFamily::SourceIngest("a"))
            .await
            .unwrap()
            .expect("source a is free");
        assert!(JobLease::try_acquire(&one, JobFamily::SourceIngest("a"))
            .await
            .unwrap()
            .is_none());
        assert!(JobLease::try_acquire(&one, JobFamily::SourceIngest("b"))
            .await
            .unwrap()
            .is_some());
        held.release().await;
        assert!(JobLease::try_acquire(&two, JobFamily::MangadexCatalogue)
            .await
            .unwrap()
            .is_some());
        drop((cover, a));
    }

    #[tokio::test]
    async fn dropping_a_lease_frees_it_for_the_other_server() {
        let one = Database::connect("sqlite::memory:").await.unwrap();
        let two = second_server(&one).await;
        let held = JobLease::try_acquire(&one, JobFamily::Discovery)
            .await
            .unwrap()
            .unwrap();
        assert!(!JobLease::is_free(&two, JobFamily::Discovery).await.unwrap());
        // A panicking or cancelled job drops its lease without `release`.
        drop(held);
        assert!(eventually_free(&two, JobFamily::Discovery).await);
    }

    /// The admin reseed refuses beside a running catalogue cycle, chapter cycle or gap
    /// backfill, on this server or another one, and starts nothing.
    #[tokio::test]
    async fn a_reseed_refuses_while_another_server_runs_the_catalogue_family() {
        let one = Database::connect("sqlite::memory:").await.unwrap();
        let two = second_server(&one).await;
        let cycle = JobLease::try_acquire(&one, JobFamily::MangadexCatalogue)
            .await
            .unwrap()
            .unwrap();
        let client = std::sync::Arc::new(crate::mangadex::MangaDexClient::new(
            "job-lease-test",
            100.0,
            100.0,
        ));
        // Refused before anything is spawned, so the client is never used.
        assert!(!crate::mangadex::spawn_resync(two, client, false)
            .await
            .unwrap());
        cycle.release().await;
    }

    #[tokio::test]
    async fn a_lease_holds_no_pool_connection() {
        let limits = crate::config::DatabaseLimits {
            background_connections: 1,
            ..Default::default()
        };
        let db =
            Database::connect_with_limits(crate::database::DatabaseOptions::Disposable, limits)
                .await
                .unwrap();
        let _held = JobLease::try_acquire(&db, JobFamily::SourceSync)
            .await
            .unwrap()
            .unwrap();
        // The single Serial writer is still free for the job's own writes.
        tokio::time::timeout(Duration::from_secs(2), sqlx::query("SELECT 1").execute(&db))
            .await
            .expect("the lease must not occupy the background writer")
            .unwrap();
    }
}
