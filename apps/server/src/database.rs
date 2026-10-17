//! Shared readers, background writer lanes, and concurrent foreground writers.
//!
//! Background writes take one of two lanes. The Serial lane is
//! one connection holding the database-wide session lease, so its writers exclude
//! each other across handles and processes, as every background write did before M9.
//! The Parallel lane is a pool without that lease: its jobs overlap each other, the
//! Serial lane and foreground writers, and rely only on the transaction lock
//! protocol (`locks.rs`). Only the jobs listed in [`ParallelJob`] take it, inside
//! [`Database::job_scope`]. `KOMIKA_BACKGROUND_WRITERS` sizes the two lanes together;
//! with 1 the Parallel lane is the Serial connection.
pub use crate::sqlite_database::SqliteDatabase;
#[cfg(test)]
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt, TryStreamExt};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgQueryResult, PgRow, PgTypeInfo};
use sqlx::{Describe, Either, Error, Execute, Executor, PgPool, Postgres, Transaction};

#[derive(Clone, Debug)]
pub struct Database {
    readers: PgPool,
    writer: PgPool,
    // None when KOMIKA_BACKGROUND_WRITERS is 1: the Parallel lane is `writer`.
    parallel_writer: Option<PgPool>,
    foreground_writer: PgPool,
    lane: WriteLane,
    deferred: Arc<DeferredWrites>,
    pub(crate) engine_fence:
        Arc<std::sync::OnceLock<std::sync::Weak<crate::engine_generation::Fence>>>,
    // Keep the disposable native PostgreSQL database alive for every pool clone.
    #[cfg(test)]
    _temporary: Option<Arc<crate::test_postgres::DisposableDatabase>>,
    // Statements sent through this handle and its clones, for tests that pin a
    // resolver's round trips. Explicit transactions and raw pools are not counted.
    #[cfg(test)]
    statements: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WriteLane {
    Background,
    Parallel,
    Foreground,
}

/// The background jobs that write on the Parallel lane. Every job not named here
/// writes on the Serial lane: scanner chapter persistence, spine and ledger,
/// catalogue sync, merge and dedup, rebuild publication, cover publication, source
/// sync and ingest, discovery and the rest.
///
/// A job joins this list only with pairwise overlap tests (real PostgreSQL lock
/// overlap, `catalog::phase4_tests::overlap`) against every job that writes the same
/// tables. The backfill drains of later slices (work provenance W2, the work chapter
/// list W3a, cover files CF) are added here as their own variants, each with those
/// tests, and run their passes in [`Database::job_scope`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParallelJob {
    /// `series_views`, `series_view_bucket` (views.rs).
    ViewFlush,
    /// `source_health_bucket` (source_health.rs).
    SourceHealthFlush,
    /// Staged comment media, old notifications, old view and health buckets, cover
    /// blobs (gc.rs).
    Gc,
    /// Planner statistics only (db.rs).
    Analyze,
    /// `engine_manga_key`, `engine_chapter_key` (engine_keys.rs).
    EngineKeyBackfill,
    /// `work_stats` and Browse's mirrored statistics (work_stats.rs).
    StatsBackfill,
    /// `chapter.readable_at`, `chapter.external_url` (mangadex.rs).
    ReadableBackfill,
    /// `chapter.scanlator` (mangadex.rs).
    GroupBackfill,
    /// Exact-media copies, objects and revisions (media_manifest/ingest.rs).
    MediaIngest,
    /// Old media revisions and unreferenced objects (media_manifest/store.rs).
    MediaPrune,
}

// Shared AppState contains the background handle. Request execution explicitly
// scopes it to foreground; spawned tasks do not inherit Tokio task locals.
// An explicit background scope wins even for a captured foreground clone.
tokio::task_local! {
    static WRITE_SCOPE: WriteLane;
}

// An instance-owned, one-shot test/benchmark pause after a real BEGIN.
// Production builds contain neither the hook nor its task-local state.
#[cfg(test)]
tokio::task_local! {
    pub static TRANSACTION_OPEN_PAUSE: crate::locks::pause_points::PausePoints;
}

/// Roll back a transaction block that a raw `BEGIN` caller left open on a
/// released connection, and issue nothing on an idle session. A bare `ROLLBACK`
/// there makes the server raise WARNING 25P01 ("there is no transaction in
/// progress"), once per background transaction, in the application log and in
/// the server log.
///
/// SQLx reads the server's transaction status from every ReadyForQuery but
/// publishes it only through `begin_with`: that fails with `BeginFailed` when
/// its statement did not leave the session inside a transaction block. A plain
/// `SELECT 1` therefore asks the question without changing anything.
async fn rollback_leftover_transaction(
    connection: &mut sqlx::PgConnection,
) -> Result<(), sqlx::Error> {
    use sqlx::Connection;
    match connection.begin_with("SELECT 1").await {
        Err(sqlx::Error::BeginFailed) => return Ok(()),
        Ok(open) => return open.rollback().await,
        // A failed transaction block refuses the question itself (25P02).
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("25P02") => {}
        Err(error) => return Err(error),
    }
    sqlx::query("ROLLBACK")
        .execute(&mut *connection)
        .await
        .map(|_| ())
}

/// Every session the server opens, on every PostgreSQL it runs on (the hosted
/// kit, a self-hosted server, the desktop app's own cluster), so a setting the
/// statements depend on belongs here and not in one deployment's
/// `postgresql.conf`.
///
/// JIT is off: it compiles a plan again on every execution once the planner's
/// cost estimate passes `jit_above_cost`, and the estimate says nothing about
/// the real work. Title search was estimated at 2.3 million and compiled for
/// 2 to 5 seconds on a production-size catalogue, inside a 2 second budget,
/// for a statement that runs in milliseconds.
pub(crate) async fn configure_session(
    connection: &mut sqlx::PgConnection,
    limits: crate::config::DatabaseSessionLimits,
) -> Result<(), Error> {
    sqlx::query(
        "SELECT set_config('TimeZone', 'UTC', false), \
        set_config('statement_timeout', $1, false), \
        set_config('lock_timeout', $2, false), \
        set_config('idle_in_transaction_session_timeout', $3, false), \
        set_config('jit', 'off', false)",
    )
    .bind(format!("{}ms", limits.statement_ms))
    .bind(format!("{}ms", limits.lock_ms))
    .bind(format!("{}ms", limits.idle_transaction_ms))
    .execute(connection)
    .await?;
    Ok(())
}

struct DeferredWrite {
    job: BoxFuture<'static, anyhow::Result<()>>,
    // The test-owned counter sink of the task that queued the job, so the
    // worker's observation is attributed to that test and to no other.
    #[cfg(test)]
    counters: Option<crate::db::test_counters::Sink>,
}

#[derive(Debug)]
struct DeferredWrites {
    sender: tokio::sync::mpsc::Sender<DeferredWrite>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    drained: Arc<tokio::sync::Notify>,
    accepting: std::sync::atomic::AtomicBool,
}

pub enum DatabaseOptions {
    Postgres(Box<PgConnectOptions>),
    #[cfg(test)]
    Disposable,
    #[cfg(test)]
    EmptyDisposable,
}
impl From<PgConnectOptions> for DatabaseOptions {
    fn from(options: PgConnectOptions) -> Self {
        Self::Postgres(Box::new(options))
    }
}
#[cfg(test)]
impl From<sqlx::sqlite::SqliteConnectOptions> for DatabaseOptions {
    fn from(_: sqlx::sqlite::SqliteConnectOptions) -> Self {
        Self::Disposable
    }
}

impl Database {
    pub async fn connect_with(
        options: impl Into<DatabaseOptions>,
        read_connections: u32,
    ) -> Result<Self, Error> {
        let mut limits = crate::config::DatabaseLimits::from_env();
        limits.read_connections = read_connections.max(1);
        // Test databases keep a Parallel pool so the overlap tests see real overlap.
        // The one-connection default is covered by tests that pass their own limits.
        #[cfg(test)]
        {
            limits.background_connections = limits.background_connections.max(3);
        }
        Self::connect_with_limits(options, limits).await
    }

    pub async fn connect_with_limits(
        options: impl Into<DatabaseOptions>,
        limits: crate::config::DatabaseLimits,
    ) -> Result<Self, Error> {
        Self::connect_with_retention(options, limits, false).await
    }

    /// Explicit instrument-only policy: retain labelled sessions for the entire
    /// measured profile. Production constructors retain SQLx's normal defaults.
    /// Acquisition and PostgreSQL session limits are identical in both modes.
    #[cfg(test)]
    pub(crate) async fn connect_for_benchmark_with_limits(
        options: impl Into<DatabaseOptions>,
        limits: crate::config::DatabaseLimits,
    ) -> Result<Self, Error> {
        Self::connect_with_retention(options, limits, true).await
    }

    async fn connect_with_retention(
        options: impl Into<DatabaseOptions>,
        limits: crate::config::DatabaseLimits,
        retain_labelled_sessions: bool,
    ) -> Result<Self, Error> {
        let pool_options = || {
            let options = PgPoolOptions::new();
            if retain_labelled_sessions {
                options.idle_timeout(None).max_lifetime(None)
            } else {
                options
            }
        };
        #[cfg(test)]
        let (options, temporary) = match options.into() {
            DatabaseOptions::Postgres(options) => (*options, None),
            DatabaseOptions::Disposable => {
                let database = crate::test_postgres::clone_database().map_err(Error::Io)?;
                let options = PgConnectOptions::from_str(&database.url)?;
                (options, Some(Arc::new(database)))
            }
            DatabaseOptions::EmptyDisposable => {
                let database = crate::test_postgres::empty_database().map_err(Error::Io)?;
                let options = PgConnectOptions::from_str(&database.url)?;
                (options, Some(Arc::new(database)))
            }
        };
        #[cfg(not(test))]
        let DatabaseOptions::Postgres(options) = options.into();
        #[cfg(not(test))]
        let options = *options;
        // The Serial lane: a database-scoped session lease keeps its writers
        // exclusive across handles, processes and raw write() callers.
        // Lazy/min=0 is essential: idle newly-created connections must not take
        // this lease through after_connect without after_release running.
        let writer = pool_options()
            .max_connections(1)
            .min_connections(0)
            .acquire_timeout(Duration::from_secs(10))
            .after_connect(move |connection, _| {
                Box::pin(async move {
                    configure_session(connection, limits.background).await?;
                    sqlx::query("SELECT pg_advisory_lock(4930189972093101::bigint)")
                        .execute(&mut *connection)
                        .await?;
                    Ok(())
                })
            })
            .before_acquire(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SELECT pg_advisory_lock(4930189972093101::bigint)")
                        .execute(&mut *connection)
                        .await?;
                    Ok(true)
                })
            })
            .after_release(|connection, _| {
                Box::pin(async move {
                    // Raw BEGIN callers do not have Transaction::drop cleanup.
                    // Complete rollback while the lease still excludes other writers.
                    rollback_leftover_transaction(connection).await?;
                    let unlocked: bool =
                        sqlx::query_scalar("SELECT pg_advisory_unlock(4930189972093101::bigint)")
                            .fetch_one(&mut *connection)
                            .await?;
                    Ok(unlocked)
                })
            })
            .connect_lazy_with(options.clone());
        // The Parallel lane: the rest of the background connections, without the
        // lease. Raw BEGIN callers still get the leftover-transaction rollback.
        let parallel_writer = (limits.background_connections > 1).then(|| {
            pool_options()
                .max_connections(limits.background_connections - 1)
                .min_connections(0)
                .acquire_timeout(Duration::from_secs(10))
                .after_connect(move |connection, _| {
                    Box::pin(async move { configure_session(connection, limits.background).await })
                })
                .after_release(|connection, _| {
                    Box::pin(async move {
                        rollback_leftover_transaction(connection).await?;
                        Ok(true)
                    })
                })
                .connect_lazy_with(options.clone())
        });
        let foreground_writer = pool_options()
            .max_connections(limits.foreground_connections.max(1))
            .min_connections(0)
            .acquire_timeout(Duration::from_secs(10))
            .after_connect(move |connection, _| {
                Box::pin(async move { configure_session(connection, limits.foreground).await })
            })
            .connect_lazy_with(options.clone());
        let readers = pool_options()
            .max_connections(limits.read_connections.max(1))
            .acquire_timeout(Duration::from_secs(3))
            .after_connect(move |connection, _| {
                Box::pin(async move {
                    configure_session(connection, limits.readers).await?;
                    sqlx::query("SET default_transaction_read_only = on")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET TIME ZONE 'UTC'")
                        .execute(&mut *connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await;
        let readers = match readers {
            Ok(readers) => readers,
            Err(error) => {
                futures::join!(writer.close(), foreground_writer.close(), async {
                    if let Some(parallel) = &parallel_writer {
                        parallel.close().await
                    }
                });
                return Err(error);
            }
        };
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<DeferredWrite>(64);
        let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drained = Arc::new(tokio::sync::Notify::new());
        let worker_pending = pending.clone();
        let worker_drained = drained.clone();
        tokio::spawn(async move {
            while let Some(deferred) = receiver.recv().await {
                let run = async {
                    match Self::background_scope(
                        std::panic::AssertUnwindSafe(deferred.job).catch_unwind(),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            crate::db::observe_database_error(&error);
                            tracing::warn!(%error, "deferred cache write failed; a later miss will retry")
                        }
                        Err(_) => {
                            tracing::error!(
                                "deferred cache write panicked; a later miss will retry"
                            )
                        }
                    }
                };
                #[cfg(test)]
                crate::db::test_counters::within(deferred.counters, run).await;
                #[cfg(not(test))]
                run.await;
                worker_pending.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                worker_drained.notify_waiters();
            }
        });
        let deferred = Arc::new(DeferredWrites {
            sender,
            pending,
            drained,
            accepting: std::sync::atomic::AtomicBool::new(true),
        });
        Ok(Self {
            readers,
            writer,
            parallel_writer,
            foreground_writer,
            lane: WriteLane::Background,
            deferred,
            engine_fence: Arc::default(),
            #[cfg(test)]
            _temporary: temporary,
            #[cfg(test)]
            statements: Arc::default(),
        })
    }

    #[cfg(test)]
    pub async fn connect(url: &str) -> Result<Self, Error> {
        if url.starts_with("sqlite:") {
            Self::connect_with(DatabaseOptions::Disposable, 4).await
        } else {
            Self::connect_with(PgConnectOptions::from_str(url)?, 4).await
        }
    }

    #[cfg(test)]
    pub fn test_url(&self) -> String {
        self._temporary
            .as_ref()
            .expect("owned disposable test database")
            .url
            .clone()
    }
    /// Statements sent through `&Database` so far (see the field).
    #[cfg(test)]
    pub fn statements_sent(&self) -> u64 {
        self.statements.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(test)]
    fn count_statement(&self) {
        self.statements
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(test)]
    pub async fn empty_for_test(read_connections: u32) -> Result<Self, Error> {
        Self::connect_with(DatabaseOptions::EmptyDisposable, read_connections).await
    }
    pub fn read(&self) -> &PgPool {
        &self.readers
    }
    /// Explicit lane for statements returning rows while changing data (RETURNING),
    /// schema maintenance, and helpers which need a raw SQLx pool.
    pub fn write(&self) -> &PgPool {
        match WRITE_SCOPE.try_with(|lane| *lane).unwrap_or(self.lane) {
            WriteLane::Background => &self.writer,
            WriteLane::Parallel => self.parallel_writer.as_ref().unwrap_or(&self.writer),
            WriteLane::Foreground => &self.foreground_writer,
        }
    }

    /// Cheap foreground clone: same readers, deferred queue, and pool lifecycle.
    pub fn foreground(&self) -> Self {
        let mut handle = self.clone();
        handle.lane = WriteLane::Foreground;
        handle
    }

    /// Explicit background clone for detached jobs and CLI work.
    pub fn background(&self) -> Self {
        let mut handle = self.clone();
        handle.lane = WriteLane::Background;
        handle
    }

    /// Route shared AppState handles for one request. Detached tokio::spawn tasks
    /// retain the safe background default. Never encompass background job work.
    pub async fn foreground_scope<F: std::future::Future>(work: F) -> F::Output {
        WRITE_SCOPE.scope(WriteLane::Foreground, work).await
    }

    /// Deferred work always uses the leased writer, including captured clones.
    pub async fn background_scope<F: std::future::Future>(work: F) -> F::Output {
        WRITE_SCOPE.scope(WriteLane::Background, work).await
    }

    /// Run one pass of a [`ParallelJob`] on the Parallel lane. Writes of tasks it
    /// spawns are not covered (task locals do not cross `tokio::spawn`) and take the
    /// Serial lane, the safe default.
    /// The job names its lane for the reader of the call site and for tests.
    pub async fn job_scope<F: std::future::Future>(_job: ParallelJob, work: F) -> F::Output {
        WRITE_SCOPE.scope(WriteLane::Parallel, work).await
    }
    pub fn connect_options(&self) -> Arc<PgConnectOptions> {
        self.readers.connect_options()
    }
    pub fn size(&self) -> u32 {
        self.readers.size()
    }
    pub fn num_idle(&self) -> usize {
        self.readers.num_idle()
    }
    /// Best-effort derived cache persistence never delays a public read. A full
    /// queue leaves the miss uncached, allowing the next request to retry.
    pub fn defer(
        &self,
        job: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let stats = &crate::telemetry::QUEUE_DEFERRED_WRITES;
        if !self.deferred.accepting.load(Ordering::Acquire) {
            stats.refused();
            return false;
        }
        self.deferred.pending.fetch_add(1, Ordering::AcqRel);
        let deferred = DeferredWrite {
            job: Box::pin(job),
            #[cfg(test)]
            counters: crate::db::test_counters::current(),
        };
        if !self.deferred.accepting.load(Ordering::Acquire)
            || self.deferred.sender.try_send(deferred).is_err()
        {
            self.deferred.pending.fetch_sub(1, Ordering::AcqRel);
            self.deferred.drained.notify_waiters();
            stats.refused();
            return false;
        }
        // Committed occupancy only: jobs sitting in the channel after this send,
        // never a concurrent submission that is about to be refused.
        stats.accepted(self.deferred_queued());
        true
    }
    /// Deferred jobs waiting in the bounded channel (not the one running), for
    /// telemetry.
    pub fn deferred_queued(&self) -> usize {
        let sender = &self.deferred.sender;
        sender.max_capacity() - sender.capacity()
    }
    pub async fn acquire(&self) -> Result<sqlx::pool::PoolConnection<Postgres>, Error> {
        self.readers.acquire().await
    }
    pub async fn begin(&self) -> Result<Transaction<'static, Postgres>, Error> {
        self.begin_with("BEGIN").await
    }
    pub async fn begin_with(
        &self,
        statement: &'static str,
    ) -> Result<Transaction<'static, Postgres>, Error> {
        let started = Instant::now();
        let statement = if statement == "BEGIN IMMEDIATE" {
            "BEGIN"
        } else {
            statement
        };
        let result = self.write().begin_with(statement).await;
        #[cfg(test)]
        if result.is_ok() {
            if let Ok(points) = TRANSACTION_OPEN_PAUSE.try_with(Clone::clone) {
                points.hit("transaction-open").await;
            }
        }
        crate::db::record_write_wait(started.elapsed());
        result
    }
    pub async fn flush_deferred(&self) {
        use std::sync::atomic::Ordering;
        loop {
            let drained = self.deferred.drained.notified();
            if self.deferred.pending.load(Ordering::Acquire) == 0 {
                break;
            }
            drained.await;
        }
    }
    pub async fn close(&self) {
        self.deferred
            .accepting
            .store(false, std::sync::atomic::Ordering::Release);
        self.flush_deferred().await;
        futures::join!(
            self.readers.close(),
            self.writer.close(),
            self.foreground_writer.close(),
            async {
                if let Some(parallel) = &self.parallel_writer {
                    parallel.close().await
                }
            }
        );
    }
}

/// Query methods that fetch rows default to the query-only lane. An accidental
/// INSERT/UPDATE/DELETE RETURNING fails instead of taking the writer unnoticed.
/// `execute` is the common, centralized path for statements without result rows.
impl<'c> Executor<'c> for &'c Database {
    type Database = Postgres;

    fn execute<'e, 'q: 'e, E>(self, query: E) -> BoxFuture<'e, Result<PgQueryResult, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Postgres>,
    {
        #[cfg(test)]
        self.count_statement();
        Box::pin(async move {
            let started = Instant::now();
            let acquired = self.write().acquire().await;
            crate::db::record_write_wait(started.elapsed());
            let mut connection = acquired?;
            let result = (&mut *connection).execute(query).await;
            result
        })
    }

    fn execute_many<'e, 'q: 'e, E>(self, query: E) -> BoxStream<'e, Result<PgQueryResult, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Postgres>,
    {
        #[cfg(test)]
        self.count_statement();
        Box::pin(
            self.write()
                .fetch_many(query)
                .try_filter_map(|step| async move {
                    Ok(match step {
                        Either::Left(result) => Some(result),
                        Either::Right(_) => None,
                    })
                }),
        )
    }

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<'e, Result<Either<PgQueryResult, PgRow>, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Postgres>,
    {
        #[cfg(test)]
        self.count_statement();
        Box::pin(self.readers.fetch_many(query))
    }

    fn fetch_optional<'e, 'q: 'e, E>(self, query: E) -> BoxFuture<'e, Result<Option<PgRow>, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Postgres>,
    {
        #[cfg(test)]
        self.count_statement();
        Box::pin(async move {
            let result = self.readers.fetch_optional(query).await;
            result
        })
    }

    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        parameters: &'e [PgTypeInfo],
    ) -> BoxFuture<'e, Result<sqlx::postgres::PgStatement<'q>, Error>>
    where
        'c: 'e,
    {
        self.readers.prepare_with(sql, parameters)
    }

    fn describe<'e, 'q: 'e>(self, sql: &'q str) -> BoxFuture<'e, Result<Describe<Postgres>, Error>>
    where
        'c: 'e,
    {
        self.readers.describe(sql)
    }
}

#[cfg(test)]
pub struct TestPoolOptions {
    read_connections: u32,
}

#[cfg(test)]
impl TestPoolOptions {
    pub fn new() -> Self {
        Self {
            read_connections: 4,
        }
    }
    pub fn max_connections(mut self, count: u32) -> Self {
        self.read_connections = count;
        self
    }
    pub async fn connect(self, url: &str) -> Result<Database, Error> {
        self.connect_with(sqlx::sqlite::SqliteConnectOptions::from_str(url)?)
            .await
    }
    pub async fn connect_with(
        self,
        options: sqlx::sqlite::SqliteConnectOptions,
    ) -> Result<Database, Error> {
        Database::connect_with(options, self.read_connections).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn postgres_readers_do_not_wait_for_writer_and_reject_writes() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE example (value BIGINT)")
            .execute(&database)
            .await
            .unwrap();
        let mut transaction = database.begin().await.unwrap();
        sqlx::query("INSERT INTO example VALUES (1)")
            .execute(&mut *transaction)
            .await
            .unwrap();
        let count: i64 = tokio::time::timeout(
            Duration::from_millis(100),
            sqlx::query_scalar("SELECT COUNT(*) FROM example").fetch_one(&database),
        )
        .await
        .expect("read waited for the writer")
        .unwrap();
        assert_eq!(count, 0, "read sees the last committed PostgreSQL snapshot");
        let error = sqlx::query("INSERT INTO example VALUES (2)")
            .execute(database.read())
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("25006")
        );
        let queued_database = database.clone();
        let mut queued = tokio::spawn(async move {
            sqlx::query("INSERT INTO example VALUES (3)")
                .execute(&queued_database)
                .await
        });
        assert!(tokio::time::timeout(Duration::from_millis(20), &mut queued)
            .await
            .is_err());
        transaction.commit().await.unwrap();
        queued.await.unwrap().unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM example")
            .fetch_one(&database)
            .await
            .unwrap();
        assert_eq!(count, 2);
        database.close().await;
    }

    #[tokio::test]
    async fn writer_lease_serializes_raw_pool_writes_across_handles_and_unlocks() {
        // The session lease as pg_locks shows it: a bigint advisory key is split
        // into classid (high half) and objid (low half) with objsubid 1.
        let lease = |alias: &str| {
            format!(
                "{alias}.locktype = 'advisory' AND {alias}.objsubid = 1 AND \
                 (({alias}.classid::bigint << 32) | {alias}.objid::bigint) = 4930189972093101 \
                 AND {alias}.database = \
                     (SELECT oid FROM pg_database WHERE datname = current_database())"
            )
        };
        let database = Database::connect("sqlite::memory:").await.unwrap();
        let external = Database::connect_with((*database.connect_options()).clone(), 2)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE lease_test(value bigint)")
            .execute(&database)
            .await
            .unwrap();
        let mut held = database.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *held)
            .await
            .unwrap();
        let pool = external.write().clone();
        let mut queued = tokio::spawn(async move {
            sqlx::query("INSERT INTO lease_test VALUES(1)")
                .execute(&pool)
                .await
        });
        // Prove the queued writer's own backend waits on the lease the holder
        // owns. A sleep would only show that it has not finished yet.
        let waiting = format!(
            "SELECT waiter.pid FROM pg_locks waiter \
             JOIN pg_stat_activity activity ON activity.pid = waiter.pid \
             WHERE NOT waiter.granted AND {} \
               AND activity.wait_event_type = 'Lock' AND activity.wait_event = 'advisory' \
               AND EXISTS (SELECT 1 FROM pg_locks owner \
                           WHERE owner.granted AND owner.pid = $1 AND {})",
            lease("waiter"),
            lease("owner")
        );
        let waiter: i32 = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let pid: Option<i32> = sqlx::query_scalar(&waiting)
                    .bind(holder)
                    .fetch_optional(database.read())
                    .await
                    .unwrap();
                if let Some(pid) = pid {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("queued raw-pool writer never waited on the lease");
        assert_ne!(waiter, holder);
        assert!(!queued.is_finished());
        let visible: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lease_test")
            .fetch_one(&database)
            .await
            .unwrap();
        assert_eq!(visible, 0, "queued insert ran while the lease was held");
        held.commit().await.unwrap();
        let inserted = tokio::time::timeout(Duration::from_secs(5), &mut queued)
            .await
            .expect("queued writer did not proceed after the holder released")
            .unwrap()
            .unwrap();
        assert_eq!(inserted.rows_affected(), 1);
        let rows: Vec<i64> = sqlx::query_scalar("SELECT value FROM lease_test")
            .fetch_all(&database)
            .await
            .unwrap();
        assert_eq!(rows, [1]);
        // The queued statement returns before its connection's release hook
        // (ROLLBACK, then unlock) has run, so the lease may still be held for a
        // moment. Probe until it is free, on ONE connection outside every pool:
        // that same connection keeps the lease it acquires and gives it back.
        use sqlx::Connection;
        let mut probe = sqlx::PgConnection::connect_with(database.connect_options().as_ref())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let acquired: bool =
                    sqlx::query_scalar("SELECT pg_try_advisory_lock(4930189972093101::bigint)")
                        .fetch_one(&mut probe)
                        .await
                        .unwrap();
                if acquired {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a writer connection kept its session lease after release");
        let owners: Vec<i32> = sqlx::query_scalar(&format!(
            "SELECT owner.pid FROM pg_locks owner WHERE owner.granted AND {}",
            lease("owner")
        ))
        .fetch_all(&mut probe)
        .await
        .unwrap();
        let own: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut probe)
            .await
            .unwrap();
        assert_eq!(owners, [own], "only the probe owns the lease now");
        let released: bool =
            sqlx::query_scalar("SELECT pg_advisory_unlock(4930189972093101::bigint)")
                .fetch_one(&mut probe)
                .await
                .unwrap();
        assert!(released, "the probe releases the lease it acquired itself");
        probe.close().await.unwrap();
        external.close().await;
        database.close().await;
    }

    #[tokio::test]
    async fn parallel_view_flush_bypasses_the_serial_lease_unless_background_has_one_connection() {
        for background_connections in [3, 1] {
            let database = Database::connect_with_limits(
                DatabaseOptions::Disposable,
                crate::config::DatabaseLimits {
                    background_connections,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            sqlx::query("CREATE TABLE bg_lane_test(value bigint PRIMARY KEY)")
                .execute(&database)
                .await
                .unwrap();
            let mut serial = database.background().begin().await.unwrap();
            sqlx::query("INSERT INTO bg_lane_test VALUES (1)")
                .execute(&mut *serial)
                .await
                .unwrap();
            let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *serial)
                .await
                .unwrap();
            // A bigint advisory key appears as classid/objid, with objsubid 1.
            let lease_sql = "SELECT EXISTS(SELECT 1 FROM pg_locks owner \
                WHERE owner.pid=$1 AND owner.granted AND owner.locktype='advisory' \
                AND owner.objsubid=1 \
                AND ((owner.classid::bigint << 32) | owner.objid::bigint)=4930189972093101 \
                AND owner.database=(SELECT oid FROM pg_database WHERE datname=current_database()))";
            let leased: bool = sqlx::query_scalar(lease_sql)
                .bind(holder)
                .fetch_one(database.read())
                .await
                .unwrap();
            assert!(leased);
            let writer = database.clone();
            let (started, starting) = tokio::sync::oneshot::channel();
            let mut parallel =
                tokio::spawn(Database::job_scope(ParallelJob::ViewFlush, async move {
                    started.send(()).unwrap();
                    let mut tx = writer.begin().await?;
                    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                        .fetch_one(&mut *tx)
                        .await?;
                    let leased: bool = sqlx::query_scalar(lease_sql)
                        .bind(pid)
                        .fetch_one(&mut *tx)
                        .await?;
                    sqlx::query("INSERT INTO bg_lane_test VALUES (2)")
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await?;
                    Ok::<_, sqlx::Error>((pid, leased))
                }));
            starting.await.unwrap();
            if background_connections == 3 {
                let (pid, leased) = tokio::time::timeout(Duration::from_secs(2), &mut parallel)
                    .await
                    .expect("Parallel write waited for the Serial lease")
                    .unwrap()
                    .unwrap();
                assert_ne!(pid, holder);
                assert!(
                    !leased,
                    "Parallel session acquired the Serial session lease"
                );
                let leased: bool = sqlx::query_scalar(lease_sql)
                    .bind(pid)
                    .fetch_one(database.read())
                    .await
                    .unwrap();
                assert!(!leased);
                let visible: Vec<i64> =
                    sqlx::query_scalar("SELECT value FROM bg_lane_test ORDER BY value")
                        .fetch_all(database.read())
                        .await
                        .unwrap();
                assert_eq!(visible, [2], "Parallel committed before Serial released");
                serial.commit().await.unwrap();
            } else {
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), &mut parallel)
                        .await
                        .is_err(),
                    "one connection must retain the Serial queue"
                );
                let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM bg_lane_test")
                    .fetch_one(database.read())
                    .await
                    .unwrap();
                assert_eq!(visible, 0);
                serial.commit().await.unwrap();
                let (_, leased) = tokio::time::timeout(Duration::from_secs(5), parallel)
                    .await
                    .expect("queued write did not resume")
                    .unwrap()
                    .unwrap();
                assert!(
                    leased,
                    "the one-connection fallback must use the leased lane"
                );
            }
            let rows: Vec<i64> =
                sqlx::query_scalar("SELECT value FROM bg_lane_test ORDER BY value")
                    .fetch_all(database.read())
                    .await
                    .unwrap();
            assert_eq!(rows, [1, 2]);
            database.close().await;
        }
    }

    #[tokio::test]
    async fn deferred_cache_queue_is_bounded_and_drains_before_close() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE example (value BIGINT)")
            .execute(&database)
            .await
            .unwrap();
        let held = database.begin().await.unwrap();
        let mut accepted = 0;
        for _ in 0..100 {
            let writer = database.clone();
            if database.defer(async move {
                sqlx::query("INSERT INTO example VALUES (1)")
                    .execute(&writer)
                    .await?;
                Ok(())
            }) {
                accepted += 1;
            }
        }
        assert!(accepted <= 65, "one running job plus 64 buffered jobs");
        assert!(accepted > 0);
        // Process-global counters (other tests defer too): lower bounds only.
        let (_, refused, _, depth_max) = crate::telemetry::QUEUE_DEFERRED_WRITES.snapshot();
        assert!(refused >= 100 - accepted as u64);
        assert!(depth_max >= 1);
        assert!(database.deferred_queued() >= 1);
        assert!(
            database.deferred_queued() <= 64,
            "never past the channel bound"
        );
        held.commit().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), database.close())
            .await
            .unwrap();
        assert!(!database.defer(async { Ok(()) }));
    }

    #[tokio::test]
    async fn retry_restarts_the_transaction_after_serialization_failure() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql("CREATE TABLE example (value BIGINT); INSERT INTO example VALUES (0)")
            .execute(&database)
            .await
            .unwrap();
        use sqlx::Connection;
        let external = std::sync::Arc::new(tokio::sync::Mutex::new(
            sqlx::PgConnection::connect_with(database.connect_options().as_ref())
                .await
                .unwrap(),
        ));
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let observed = crate::db::retry_transaction(|| async {
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Deliberately reproduce the old deferred read-to-write upgrade.
            let mut transaction = database
                .begin_with("BEGIN ISOLATION LEVEL SERIALIZABLE")
                .await?;
            let value: i64 = sqlx::query_scalar("SELECT value FROM example")
                .fetch_one(&mut *transaction)
                .await?;
            if attempt == 0 {
                sqlx::query("UPDATE example SET value = 1")
                    .execute(&mut *external.lock().await)
                    .await?;
            }
            sqlx::query("UPDATE example SET value = 2")
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
            Ok(value)
        })
        .await
        .unwrap();
        assert_eq!(observed, 1, "retry reads the newly committed snapshot");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
        drop(external);
        database.close().await;
    }

    #[tokio::test]
    async fn retry_uses_error_codes_and_does_not_retry_message_matches() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let result: anyhow::Result<()> = crate::db::retry_transaction(|| async {
            attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            anyhow::bail!("database is locked: this is only application text")
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}

#[cfg(test)]
mod phase4_tests {
    use super::*;
    use crate::db::test_counters::{self, Counters};
    use crate::db::DatabaseCounters;
    use crate::{config::DatabaseLimits, locks};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Exact counter assertions use `test_counters::scope`: the deltas of this
    // test's own task tree. The process globals move under every other test.
    fn counted(database: DatabaseCounters, lock_errors: u64, write: u64, read: u64) -> Counters {
        Counters {
            database,
            lock_errors,
            write_lane_events: write,
            read_lane_events: read,
        }
    }

    async fn configured(limits: DatabaseLimits) -> Database {
        Database::connect_with_limits(DatabaseOptions::Disposable, limits)
            .await
            .unwrap()
    }

    fn code(error: &sqlx::Error) -> Option<String> {
        error
            .as_database_error()
            .and_then(|e| e.code())
            .map(|c| c.into_owned())
    }

    async fn wait_for(pool: &Database, pid: i32, wait: bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let seen: bool = sqlx::query_scalar(if wait {
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock')"
                } else {
                    "SELECT NOT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1)"
                }).bind(pid).fetch_one(pool.read()).await.unwrap();
                if seen { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.unwrap();
    }

    async fn example(pool: &Database) {
        sqlx::raw_sql("CREATE TABLE phase4_example(id bigint PRIMARY KEY, value bigint NOT NULL); INSERT INTO phase4_example VALUES(1,0),(2,0)")
            .execute(pool).await.unwrap();
    }

    #[tokio::test]
    async fn benchmark_retains_sessions_without_changing_normal_pool_or_lane_policy() {
        let limits = DatabaseLimits::default();
        let normal = configured(limits).await;
        let benchmark = Database::connect_for_benchmark_with_limits(
            normal.connect_options().as_ref().clone(),
            limits,
        )
        .await
        .unwrap();
        let normal_foreground = normal.foreground();
        let measured_foreground = benchmark.foreground();
        let normal_background = normal.background();
        let measured_background = benchmark.background();
        for (ordinary, measured) in [
            (normal.read(), benchmark.read()),
            (normal_foreground.write(), measured_foreground.write()),
            (normal_background.write(), measured_background.write()),
        ] {
            assert_eq!(
                ordinary.options().get_idle_timeout(),
                Some(Duration::from_secs(600))
            );
            assert_eq!(
                ordinary.options().get_max_lifetime(),
                Some(Duration::from_secs(1800))
            );
            assert_eq!(measured.options().get_idle_timeout(), None);
            assert_eq!(measured.options().get_max_lifetime(), None);
            assert_eq!(
                ordinary.options().get_max_connections(),
                measured.options().get_max_connections()
            );
            assert_eq!(
                ordinary.options().get_acquire_timeout(),
                measured.options().get_acquire_timeout()
            );
            let settings = "SELECT current_setting('statement_timeout'), current_setting('lock_timeout'), current_setting('idle_in_transaction_session_timeout'), current_setting('default_transaction_read_only')";
            let actual: (String, String, String, String) =
                sqlx::query_as(settings).fetch_one(measured).await.unwrap();
            let expected: (String, String, String, String) =
                sqlx::query_as(settings).fetch_one(ordinary).await.unwrap();
            assert_eq!(actual, expected);
        }
        benchmark.close().await;
        normal.close().await;
    }

    // PostgreSQL's default is `jit = on`, and no deployment's configuration file
    // is in play here: the server's own session setup must turn it off, on every
    // lane and on a standalone session.
    #[tokio::test]
    async fn every_session_has_jit_off() {
        use sqlx::Connection;
        let pool = configured(DatabaseLimits::default()).await;
        let mut plain = sqlx::PgConnection::connect_with(pool.connect_options().as_ref())
            .await
            .unwrap();
        let jit = "SELECT current_setting('jit')";
        let default: String = sqlx::query_scalar(jit).fetch_one(&mut plain).await.unwrap();
        assert_eq!(default, "on", "the cluster default this test relies on");
        let foreground = pool.foreground();
        for lane in [pool.read(), foreground.write(), pool.write()] {
            let setting: String = sqlx::query_scalar(jit).fetch_one(lane).await.unwrap();
            assert_eq!(setting, "off");
        }
        crate::db::apply_session_limits(&mut plain, crate::db::Lane::Readers)
            .await
            .unwrap();
        let standalone: String = sqlx::query_scalar(jit).fetch_one(&mut plain).await.unwrap();
        assert_eq!(standalone, "off");
        plain.close().await.unwrap();
        pool.close().await;
    }

    // Third check, weak test: no test read the session settings a lane really
    // has, so handing the readers the background limits (or the reverse) passed
    // everything. Nine distinct values; each lane must report its own three.
    #[tokio::test]
    async fn each_lane_session_has_its_own_three_timeouts() {
        use crate::config::DatabaseSessionLimits;
        let limits = DatabaseLimits {
            readers: DatabaseSessionLimits {
                statement_ms: 31_001,
                lock_ms: 5_003,
                idle_transaction_ms: 61_007,
            },
            foreground: DatabaseSessionLimits {
                statement_ms: 15_013,
                lock_ms: 4_999,
                idle_transaction_ms: 30_011,
            },
            background: DatabaseSessionLimits {
                statement_ms: 1_800_017,
                lock_ms: 60_013,
                idle_transaction_ms: 900_007,
            },
            ..Default::default()
        };
        let pool = configured(limits).await;
        async fn session(connection: &mut sqlx::PgConnection) -> (String, String, String, String) {
            sqlx::query_as(
                "SELECT current_setting('statement_timeout'), current_setting('lock_timeout'), \
                 current_setting('idle_in_transaction_session_timeout'), \
                 current_setting('default_transaction_read_only')",
            )
            .fetch_one(connection)
            .await
            .unwrap()
        }
        let expected = |lane: DatabaseSessionLimits, read_only: &str| {
            (
                format!("{}ms", lane.statement_ms),
                format!("{}ms", lane.lock_ms),
                format!("{}ms", lane.idle_transaction_ms),
                read_only.to_owned(),
            )
        };
        let mut reader = pool.read().acquire().await.unwrap();
        assert_eq!(session(&mut reader).await, expected(limits.readers, "on"));
        // The Executor's row-returning path uses the same readers.
        let through_handle: String = sqlx::query_scalar("SELECT current_setting('lock_timeout')")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(through_handle, "5003ms");
        let foreground = pool.foreground();
        let mut writer = foreground.write().acquire().await.unwrap();
        assert_eq!(
            session(&mut writer).await,
            expected(limits.foreground, "off")
        );
        let mut transaction = foreground.begin().await.unwrap();
        assert_eq!(
            session(&mut transaction).await,
            expected(limits.foreground, "off")
        );
        transaction.rollback().await.unwrap();
        // The leased background writer, raw and through begin().
        let mut leased = pool.write().acquire().await.unwrap();
        assert_eq!(
            session(&mut leased).await,
            expected(limits.background, "off")
        );
        drop(leased);
        let mut transaction = pool.background().begin().await.unwrap();
        assert_eq!(
            session(&mut transaction).await,
            expected(limits.background, "off")
        );
        transaction.rollback().await.unwrap();
        // A request scope routes the shared (background-default) handle to the
        // foreground limits, and a background scope inside it back again.
        let scoped = Database::foreground_scope(async {
            let mut connection = pool.write().acquire().await.unwrap();
            let inside = session(&mut connection).await;
            drop(connection);
            let nested = Database::background_scope(async {
                let mut connection = pool.write().acquire().await.unwrap();
                session(&mut connection).await
            })
            .await;
            (inside, nested)
        })
        .await;
        assert_eq!(scoped.0, expected(limits.foreground, "off"));
        assert_eq!(scoped.1, expected(limits.background, "off"));
        drop(writer);
        drop(reader);
        // A standalone session gets a lane's policy from the public helper (the
        // helper reads the process environment, so compare with that).
        use sqlx::Connection;
        let environment = DatabaseLimits::from_env();
        for (lane, policy) in [
            (crate::db::Lane::Readers, environment.readers),
            (crate::db::Lane::Foreground, environment.foreground),
            (crate::db::Lane::Background, environment.background),
        ] {
            let mut standalone = sqlx::PgConnection::connect_with(pool.connect_options().as_ref())
                .await
                .unwrap();
            crate::db::apply_session_limits(&mut standalone, lane)
                .await
                .unwrap();
            let milliseconds: (i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT setting::bigint FROM pg_settings WHERE name='statement_timeout'), \
                 (SELECT setting::bigint FROM pg_settings WHERE name='lock_timeout'), \
                 (SELECT setting::bigint FROM pg_settings \
                   WHERE name='idle_in_transaction_session_timeout')",
            )
            .fetch_one(&mut standalone)
            .await
            .unwrap();
            assert_eq!(
                milliseconds,
                (
                    policy.statement_ms as i64,
                    policy.lock_ms as i64,
                    policy.idle_transaction_ms as i64
                )
            );
            standalone.close().await.unwrap();
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn foreground_pool_timeout_leaves_rest_boundary_once_with_retry_after() {
        let pool = configured(DatabaseLimits {
            foreground_connections: 1,
            ..Default::default()
        })
        .await;
        let foreground = pool.foreground();
        let held = foreground.begin().await.unwrap();
        let (response, seen) = test_counters::scope(async {
            let error = tokio::time::timeout(Duration::from_secs(12), foreground.begin())
                .await
                .unwrap()
                .unwrap_err();
            assert!(matches!(error, sqlx::Error::PoolTimedOut));
            crate::database_error_response(error, "database unavailable")
        })
        .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "1");
        assert_eq!(
            seen,
            counted(
                DatabaseCounters {
                    pool_timeouts: 1,
                    ..Default::default()
                },
                1,
                1,
                0
            )
        );
        held.rollback().await.unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn raw_begin_release_rolls_back_before_independent_background_lease() {
        let pool = configured(DatabaseLimits::default()).await;
        example(&pool).await;
        let other = Database::connect(&pool.test_url()).await.unwrap();
        let mut raw = pool.write().acquire().await.unwrap();
        sqlx::query("BEGIN").execute(&mut *raw).await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=99 WHERE id=1")
            .execute(&mut *raw)
            .await
            .unwrap();
        drop(raw);
        let mut second = tokio::time::timeout(Duration::from_secs(2), other.begin())
            .await
            .unwrap()
            .unwrap();
        let value: i64 =
            sqlx::query_scalar("SELECT value FROM phase4_example WHERE id=1 FOR UPDATE")
                .fetch_one(&mut *second)
                .await
                .unwrap();
        assert_eq!(
            value, 0,
            "raw transaction must roll back before releasing lease"
        );
        second.rollback().await.unwrap();
        other.close().await;
        pool.close().await;
    }

    // Third fix round: the release hook sent a bare ROLLBACK after every
    // background transaction. On an idle session the server answers that with
    // WARNING 25P01, which SQLx logs at WARN: one false database warning per
    // background transaction, in the application log and in the server log.
    #[tokio::test]
    async fn background_release_rolls_back_only_an_open_block_and_raises_no_server_warning() {
        let pool = configured(DatabaseLimits::default()).await;
        example(&pool).await;
        // One thread: the scoped subscriber sees the release hook's own task.
        let capture = crate::postgres_contention::LogCapture::default();
        let _scoped = tracing::subscriber::set_default(capture.subscriber());
        let background = pool.background();
        const VALUE: &str = "SELECT value FROM phase4_example WHERE id=1";
        // The pool has one connection: an acquire waits for the previous release.
        // Committed, rolled back, and never in a transaction at all.
        let mut committed = background.begin().await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=1 WHERE id=1")
            .execute(&mut *committed)
            .await
            .unwrap();
        committed.commit().await.unwrap();
        background.begin().await.unwrap().rollback().await.unwrap();
        drop(background.write().acquire().await.unwrap());
        // A raw transaction block left open is rolled back.
        let mut raw = background.write().acquire().await.unwrap();
        sqlx::query("BEGIN").execute(&mut *raw).await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=98 WHERE id=1")
            .execute(&mut *raw)
            .await
            .unwrap();
        drop(raw);
        let mut next = background.write().acquire().await.unwrap();
        let value: i64 = sqlx::query_scalar(VALUE)
            .fetch_one(&mut *next)
            .await
            .unwrap();
        assert_eq!(value, 1, "the open raw block was not rolled back");
        // So is a raw block that has already failed, and the session is usable.
        sqlx::query("BEGIN").execute(&mut *next).await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=99 WHERE id=1")
            .execute(&mut *next)
            .await
            .unwrap();
        let failed = sqlx::query("SELECT 1/0").execute(&mut *next).await;
        assert_eq!(code(&failed.unwrap_err()).as_deref(), Some("22012"));
        let pid: i32 = {
            drop(next);
            let mut after = background.write().acquire().await.unwrap();
            let value: i64 = sqlx::query_scalar(VALUE)
                .fetch_one(&mut *after)
                .await
                .unwrap();
            assert_eq!(value, 1, "the failed raw block was not rolled back");
            sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *after)
                .await
                .unwrap()
        };
        // The same session all along: no release closed it, and it is left idle
        // with the lease given back.
        let mut last = background.write().acquire().await.unwrap();
        let (same, open_blocks): (bool, i64) = sqlx::query_as(
            "SELECT pg_backend_pid() = $1, (SELECT COUNT(*) FROM pg_stat_activity \
             WHERE pid <> pg_backend_pid() AND datname = current_database() \
               AND state LIKE 'idle in transaction%')",
        )
        .bind(pid)
        .fetch_one(&mut *last)
        .await
        .unwrap();
        assert!(same, "a release hook failed and the session was replaced");
        assert_eq!(open_blocks, 0);
        drop(last);
        let logs = capture.take();
        assert_eq!(
            (logs.warnings, logs.errors),
            (0, 0),
            "{:?} {:?}",
            logs.warn_events,
            logs.first_errors
        );
        drop(_scoped);
        pool.close().await;
    }

    #[tokio::test]
    async fn merge_library_foreign_key_cycle_uses_compatible_parent_locks() {
        let pool = configured(DatabaseLimits::default()).await;
        sqlx::raw_sql("CREATE SCHEMA phase4_fk; CREATE TABLE phase4_fk.users(id uuid PRIMARY KEY); CREATE TABLE phase4_fk.work(id text PRIMARY KEY); CREATE TABLE phase4_fk.library(user_id uuid REFERENCES phase4_fk.users, work_id text REFERENCES phase4_fk.work, PRIMARY KEY(user_id,work_id)); INSERT INTO phase4_fk.users VALUES('00000000-0000-4000-8000-000000000001'); INSERT INTO phase4_fk.work VALUES('w')")
            .execute(&pool).await.unwrap();
        let foreground = pool.foreground();
        for strong in [true, false] {
            let mut merge = foreground.begin().await.unwrap();
            let mut library = foreground.begin().await.unwrap();
            for tx in [&mut merge, &mut library] {
                sqlx::raw_sql("SET LOCAL search_path=phase4_fk; SET LOCAL lock_timeout='4s'")
                    .execute(&mut **tx)
                    .await
                    .unwrap();
            }
            if strong {
                locks::lock_works_for_delete(&mut merge, &["w"])
                    .await
                    .unwrap();
                locks::lock_users_for_delete(
                    &mut library,
                    &["00000000-0000-4000-8000-000000000001"],
                )
                .await
                .unwrap();
            } else {
                locks::lock_works(&mut merge, &["w"]).await.unwrap();
                locks::lock_user(&mut library, "00000000-0000-4000-8000-000000000001")
                    .await
                    .unwrap();
            }
            locks::advisory(
                &mut library,
                &[locks::AdvisoryKey::library_progress(
                    "00000000-0000-4000-8000-000000000001",
                    "w",
                )],
            )
            .await
            .unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *library)
                .await
                .unwrap();
            let writer = tokio::spawn(async move {
                let result = locks::lock_works(&mut library, &["w"]).await;
                library.rollback().await.unwrap();
                result
            });
            wait_for(&pool, pid, true).await;
            let inserted = sqlx::query(
                "INSERT INTO library VALUES('00000000-0000-4000-8000-000000000001','w')",
            )
            .execute(&mut *merge)
            .await;
            merge.rollback().await.unwrap();
            let locked = writer.await.unwrap();
            if strong {
                assert!(
                    inserted.as_ref().err().and_then(code).as_deref() == Some("40P01")
                        || locked.as_ref().err().and_then(code).as_deref() == Some("40P01")
                );
            } else {
                inserted.unwrap();
                locked.unwrap();
            }
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn source_discovery_move_restarts_and_permanent_mover_exhausts_five_attempts() {
        let pool = configured(DatabaseLimits::default()).await;
        sqlx::raw_sql("CREATE SCHEMA phase4_move; CREATE TABLE phase4_move.work(id text PRIMARY KEY); CREATE TABLE phase4_move.source_series(id text PRIMARY KEY, work_id text REFERENCES phase4_move.work); INSERT INTO phase4_move.work VALUES('a'),('b'); INSERT INTO phase4_move.source_series VALUES('s','a')")
            .execute(&pool).await.unwrap();
        let foreground = pool.foreground();
        for permanent in [false, true] {
            let attempts = AtomicUsize::new(0);
            let (result, seen) = test_counters::scope(crate::db::retry_transaction(|| async {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                let points = locks::pause_points::PausePoints::default();
                let armed = points.arm("source-discovered");
                let mut tx = foreground.begin().await?;
                sqlx::query("SET LOCAL search_path=phase4_move").execute(&mut *tx).await?;
                let rival = foreground.clone();
                let mover = tokio::spawn(async move {
                    armed.reached().await;
                    if permanent || attempt == 0 {
                        sqlx::query("UPDATE phase4_move.source_series SET work_id=CASE work_id WHEN 'a' THEN 'b' ELSE 'a' END WHERE id='s'")
                            .execute(&rival).await.unwrap();
                    }
                    armed.release();
                });
                let locked = locks::SOURCE_DISCOVERY_PAUSE.scope(points, locks::lock_source_series(&mut tx, &["s"])).await;
                mover.await.unwrap();
                locked?;
                tx.commit().await?;
                Ok(())
            })).await;
            if permanent {
                let error = result.unwrap_err();
                assert!(error.downcast_ref::<locks::RestartTransaction>().is_some());
                assert!(error.to_string().contains("five attempts"));
                assert_eq!(attempts.load(Ordering::SeqCst), 5);
                // Four re-executions, then exhaustion (which is a lock error).
                assert_eq!(
                    seen,
                    counted(
                        DatabaseCounters {
                            transaction_restarts: 4,
                            retries: 4,
                            retries_exhausted: 1,
                            ..Default::default()
                        },
                        1,
                        0,
                        0
                    )
                );
            } else {
                result.unwrap();
                assert_eq!(attempts.load(Ordering::SeqCst), 2);
                assert_eq!(
                    seen,
                    counted(
                        DatabaseCounters {
                            transaction_restarts: 1,
                            retries: 1,
                            ..Default::default()
                        },
                        0,
                        0,
                        0
                    )
                );
            }
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn source_delete_lock_is_parent_first_and_excludes_child_inserts() {
        let pool = configured(DatabaseLimits::default()).await;
        sqlx::raw_sql(
            "CREATE SCHEMA phase4_delete; \
             CREATE TABLE phase4_delete.work(id text PRIMARY KEY); \
             CREATE TABLE phase4_delete.source_series(id text PRIMARY KEY, \
                 work_id text NOT NULL REFERENCES phase4_delete.work); \
             CREATE TABLE phase4_delete.chapter(id text PRIMARY KEY, source_series_id text \
                 NOT NULL REFERENCES phase4_delete.source_series ON DELETE CASCADE); \
             INSERT INTO phase4_delete.work VALUES('a'),('b'); \
             INSERT INTO phase4_delete.source_series VALUES('s','a'),('t','a')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let foreground = pool.foreground();
        async fn scoped(pool: &Database) -> (Transaction<'static, Postgres>, i32) {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SET LOCAL search_path=phase4_delete")
                .execute(&mut *tx)
                .await
                .unwrap();
            let pid = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            (tx, pid)
        }

        // Parent first: while a rival holds the work, the helper waits on that
        // work and has not locked the source row yet.
        let (mut rival, _) = scoped(&foreground).await;
        locks::lock_works(&mut rival, &["a"]).await.unwrap();
        let (mut deleter, deleter_pid) = scoped(&foreground).await;
        let waiting = tokio::spawn(async move {
            let rows = locks::lock_source_series_for_delete(&mut deleter, &["s"]).await;
            (deleter, rows)
        });
        wait_for(&pool, deleter_pid, true).await;
        let (mut probe, _) = scoped(&foreground).await;
        sqlx::query("SELECT id FROM source_series WHERE id='s' FOR UPDATE NOWAIT")
            .execute(&mut *probe)
            .await
            .expect("the source row was locked before its parent work");
        probe.rollback().await.unwrap();
        rival.rollback().await.unwrap();
        let (mut deleter, rows) = waiting.await.unwrap();
        assert_eq!(rows.unwrap().locked, ["s"]);

        // Delete strength on the source row: a child insert (real foreign key,
        // KEY SHARE on the source) waits for the deleter.
        let (mut child, child_pid) = scoped(&foreground).await;
        let insert = tokio::spawn(async move {
            let result = sqlx::query("INSERT INTO chapter VALUES('c1','s')")
                .execute(&mut *child)
                .await;
            child.rollback().await.unwrap();
            result
        });
        wait_for(&pool, child_pid, true).await;
        assert!(!insert.is_finished());
        // The work parent is not being deleted and stays NO KEY UPDATE: a sibling
        // source insert (KEY SHARE on the work) is not blocked.
        let (mut sibling, _) = scoped(&foreground).await;
        tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query("INSERT INTO source_series VALUES('u','a')").execute(&mut *sibling),
        )
        .await
        .expect("the delete variant must not take FOR UPDATE on the work parent")
        .unwrap();
        sibling.rollback().await.unwrap();
        sqlx::query("DELETE FROM source_series WHERE id='s'")
            .execute(&mut *deleter)
            .await
            .unwrap();
        deleter.commit().await.unwrap();
        // The waiting child sees the committed delete: 23503, not an orphan row.
        let error = insert.await.unwrap().unwrap_err();
        assert_eq!(code(&error).as_deref(), Some("23503"));

        // The ordinary helper keeps the compatible strength: no wait for the child.
        let (mut holder, _) = scoped(&foreground).await;
        let rows = locks::lock_source_series(&mut holder, &["t", "gone"])
            .await
            .unwrap();
        assert_eq!(
            (rows.locked, rows.missing),
            (vec!["t".to_owned()], vec!["gone".to_owned()])
        );
        let (mut child, _) = scoped(&foreground).await;
        tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query("INSERT INTO chapter VALUES('c2','t')").execute(&mut *child),
        )
        .await
        .expect("NO KEY UPDATE on a source row must not block a child insert")
        .unwrap();
        child.rollback().await.unwrap();
        holder.rollback().await.unwrap();

        // A source moved after discovery is the same typed restart, and missing
        // rows are reported, never treated as locked.
        let points = locks::pause_points::PausePoints::default();
        let armed = points.arm("source-discovered");
        let (mut tx, _) = scoped(&foreground).await;
        let rival = foreground.clone();
        let mover = tokio::spawn(async move {
            armed.reached().await;
            sqlx::query("UPDATE phase4_delete.source_series SET work_id='b' WHERE id='t'")
                .execute(&rival)
                .await
                .unwrap();
            armed.release();
        });
        let error = locks::SOURCE_DISCOVERY_PAUSE
            .scope(
                points,
                locks::lock_source_series_for_delete(&mut tx, &["t"]),
            )
            .await
            .unwrap_err();
        mover.await.unwrap();
        assert!(error.downcast_ref::<locks::RestartTransaction>().is_some());
        tx.rollback().await.unwrap();
        let (mut tx, _) = scoped(&foreground).await;
        let rows = locks::lock_source_series_for_delete(&mut tx, &["gone", "t"])
            .await
            .unwrap();
        assert_eq!(
            (rows.locked, rows.missing),
            (vec!["t".to_owned()], vec!["gone".to_owned()])
        );
        tx.rollback().await.unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn page_block_rule_read_failure_is_a_counted_read_error_and_fails_closed() {
        use axum::{body::Body, extract::FromRef, http::Request, routing::get, Router};
        use tower::ServiceExt;
        let pool = configured(DatabaseLimits {
            read_connections: 1,
            ..Default::default()
        })
        .await;
        let cover_dir = crate::cover_store::test_support::TestDir::new();
        let covers = crate::cover_store::test_support::store(&cover_dir);
        #[derive(Clone)]
        struct PageState(Arc<crate::graphql::AppState>, crate::CoverDb);
        impl FromRef<PageState> for Arc<crate::graphql::AppState> {
            fn from_ref(state: &PageState) -> Self {
                state.0.clone()
            }
        }
        impl FromRef<PageState> for crate::CoverDb {
            fn from_ref(state: &PageState) -> Self {
                state.1.clone()
            }
        }
        let app = Router::new()
            .route("/api/v1/manga/{*rest}", get(crate::serve_suwayomi_image))
            .with_state(PageState(
                crate::postgres_contention::app_state(&pool, &covers),
                crate::CoverDb(covers.clone()),
            ));
        let page = || {
            Request::builder()
                .uri("/api/v1/manga/7/chapter/1/page/0")
                .body(Body::empty())
                .unwrap()
        };
        // The only reader is taken: the site block rules cannot be read.
        let reader = pool.read().acquire().await.unwrap();
        let (response, seen) = test_counters::scope(app.clone().oneshot(page())).await;
        drop(reader);
        let response = response.unwrap();
        // Not served, through the standard database response (busy, retryable).
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "1");
        // Counted once, as a read: the write gate does not move.
        assert_eq!(
            seen,
            counted(
                DatabaseCounters {
                    pool_timeouts: 1,
                    ..Default::default()
                },
                0,
                0,
                1
            )
        );
        // Readable rules, none blocking: the handler goes on to the engine (which
        // is unreachable here) and no database error is counted.
        let (response, seen) = test_counters::scope(app.oneshot(page())).await;
        assert_eq!(
            response.unwrap().status(),
            axum::http::StatusCode::BAD_GATEWAY
        );
        assert_eq!(seen, Counters::default());
        pool.close().await;
    }

    #[tokio::test]
    async fn deferred_failure_is_observed_once_at_worker_boundary() {
        let pool = configured(DatabaseLimits::default()).await;
        let captured = pool.clone();
        // The job fails in the worker task. `defer` hands the worker this scope's
        // sink, so the worker-boundary observation is seen here and nowhere else.
        let ((), seen) = test_counters::scope(async {
            assert!(pool.defer(async move {
                let mut tx = captured.begin().await?;
                sqlx::query("SET LOCAL statement_timeout='20ms'")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("SELECT pg_sleep(0.1)")
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(())
            }));
            pool.flush_deferred().await;
        })
        .await;
        assert_eq!(
            seen,
            counted(
                DatabaseCounters {
                    statement_timeouts: 1,
                    ..Default::default()
                },
                0,
                1,
                0
            )
        );
        // The sink is the one current when the job is queued: a job queued outside
        // a scope reports to no test sink, even when a scope waits for it.
        let captured = pool.clone();
        assert!(pool.defer(async move {
            let mut tx = captured.begin().await?;
            sqlx::query("SET LOCAL statement_timeout='20ms'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SELECT pg_sleep(0.1)")
                .execute(&mut *tx)
                .await?;
            Ok(())
        }));
        let ((), unscoped) = test_counters::scope(pool.flush_deferred()).await;
        assert_eq!(unscoped, Counters::default());
        pool.close().await;
    }

    #[tokio::test]
    async fn avatar_handler_lock_timeout_is_observed_once() {
        use axum::{body::Body, http::Request, routing::post, Router};
        use tower::ServiceExt;
        let mut limits = DatabaseLimits::default();
        limits.foreground.lock_ms = 50;
        let pool = configured(limits).await;
        let user = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,username,email,password_hash,created_at) VALUES($1,'avatar_counter','avatar_counter@example.invalid','unused',now())").bind(user).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions(token,user_id,created_at,expires_at) VALUES($1,$2,now(),now()+interval '1 hour')").bind(crate::auth::hash_token("counter-token")).bind(user).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO user_avatars(user_id,webp,version,updated_at) VALUES($1,$2,1,now())",
        )
        .bind(user)
        .bind(vec![1u8])
        .execute(&pool)
        .await
        .unwrap();
        let foreground = pool.foreground();
        let mut held = foreground.begin().await.unwrap();
        sqlx::query("SELECT user_id FROM user_avatars WHERE user_id=$1 FOR UPDATE")
            .bind(user)
            .execute(&mut *held)
            .await
            .unwrap();
        #[derive(Clone)]
        struct UploadState(Database, Arc<crate::RateLimiter>);
        impl axum::extract::FromRef<UploadState> for Database {
            fn from_ref(state: &UploadState) -> Self {
                state.0.clone()
            }
        }
        impl axum::extract::FromRef<UploadState> for Arc<crate::RateLimiter> {
            fn from_ref(state: &UploadState) -> Self {
                state.1.clone()
            }
        }
        let app = Router::new()
            .route("/avatar", post(crate::upload_avatar))
            .with_state(UploadState(
                foreground,
                Arc::new(crate::RateLimiter::new(100, 60)),
            ));
        let mut image_bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(16, 16)
            .write_to(&mut image_bytes, image::ImageFormat::Png)
            .unwrap();
        let mut body = b"--counter\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n".to_vec();
        body.extend(image_bytes.into_inner());
        body.extend(b"\r\n--counter--\r\n");
        let (response, seen) = test_counters::scope(
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/avatar")
                    .header("authorization", "Bearer counter-token")
                    .header("content-type", "multipart/form-data; boundary=counter")
                    .body(Body::from(body))
                    .unwrap(),
            ),
        )
        .await;
        assert_eq!(
            response.unwrap().status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            seen,
            counted(
                DatabaseCounters {
                    lock_timeouts: 1,
                    ..Default::default()
                },
                1,
                1,
                0
            )
        );
        held.rollback().await.unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn foreground_commits_while_background_transaction_is_open() {
        let pool = configured(DatabaseLimits::default()).await;
        example(&pool).await;
        let mut background = pool.begin().await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=10 WHERE id=1")
            .execute(&mut *background)
            .await
            .unwrap();
        let foreground = pool.foreground();
        let mut user = foreground.begin().await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=20 WHERE id=2")
            .execute(&mut *user)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), user.commit())
            .await
            .unwrap()
            .unwrap();
        let visible: i64 = sqlx::query_scalar("SELECT value FROM phase4_example WHERE id=2")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(visible, 20);
        background.rollback().await.unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn request_scope_and_deferred_jobs_route_to_distinct_lanes() {
        let pool = configured(DatabaseLimits::default()).await;
        let foreground = pool.foreground();
        let held = pool.begin().await.unwrap();
        let points = locks::pause_points::PausePoints::default();
        let armed = points.arm("deferred-start");
        let (send, receive) = tokio::sync::oneshot::channel();
        Database::foreground_scope(async {
            sqlx::query("SELECT 1").execute(&pool).await.unwrap();
            let captured = foreground.clone();
            assert!(pool.defer(async move {
                points.hit("deferred-start").await;
                let lane = captured.write().options().get_max_connections();
                let result = sqlx::query("SELECT 1").execute(&captured).await;
                send.send((lane, result)).expect("test receiver alive");
                Ok(())
            }));
        })
        .await;
        armed.reached().await;
        armed.release();
        held.rollback().await.unwrap();
        pool.flush_deferred().await;
        let (lane, result) = receive.await.unwrap();
        assert_eq!(lane, 1);
        result.unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn foreground_row_writer_waits_then_proceeds_and_lock_timeout_is_counted() {
        let pool = configured(DatabaseLimits::default()).await;
        example(&pool).await;
        let foreground = pool.foreground();
        let mut first = foreground.begin().await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=1 WHERE id=1")
            .execute(&mut *first)
            .await
            .unwrap();
        let mut second = foreground.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *second)
            .await
            .unwrap();
        let waiter = tokio::spawn(async move {
            sqlx::query("UPDATE phase4_example SET value=value+1 WHERE id=1")
                .execute(&mut *second)
                .await
                .unwrap();
            second.commit().await.unwrap();
        });
        wait_for(&pool, pid, true).await;
        assert!(!waiter.is_finished());
        first.commit().await.unwrap();
        waiter.await.unwrap();
        let mut first = foreground.begin().await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=3 WHERE id=1")
            .execute(&mut *first)
            .await
            .unwrap();
        let mut second = foreground.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout='50ms'")
            .execute(&mut *second)
            .await
            .unwrap();
        let error = sqlx::query("UPDATE phase4_example SET value=4 WHERE id=1")
            .execute(&mut *second)
            .await
            .unwrap_err();
        assert_eq!(code(&error).as_deref(), Some("55P03"));
        let ((), seen) = test_counters::scope_sync(|| crate::db::record_database_error(&error));
        assert_eq!(
            seen,
            counted(
                DatabaseCounters {
                    lock_timeouts: 1,
                    ..Default::default()
                },
                1,
                1,
                0
            )
        );
        second.rollback().await.unwrap();
        first.rollback().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT value FROM phase4_example WHERE id=1")
                .fetch_one(&pool)
                .await
                .unwrap(),
            2
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn foreground_statement_timeout_does_not_apply_to_background() {
        let mut limits = DatabaseLimits::default();
        limits.foreground.statement_ms = 50;
        let pool = configured(limits).await;
        let foreground = pool.foreground();
        let error = sqlx::query("SELECT pg_sleep(0.15)")
            .execute(&foreground)
            .await
            .unwrap_err();
        assert_eq!(code(&error).as_deref(), Some("57014"));
        let (_, seen) =
            test_counters::scope_sync(|| crate::database_error_response(error, "statement failed"));
        // A statement timeout is counted, and it is not a lock error.
        assert_eq!(
            seen,
            counted(
                DatabaseCounters {
                    statement_timeouts: 1,
                    ..Default::default()
                },
                0,
                1,
                0
            )
        );
        sqlx::query("SELECT pg_sleep(0.15)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn idle_foreground_transaction_terminates_and_releases_row_lock() {
        let mut limits = DatabaseLimits::default();
        limits.foreground.idle_transaction_ms = 100;
        let pool = configured(limits).await;
        example(&pool).await;
        let foreground = pool.foreground();
        let mut abandoned = foreground.begin().await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=1 WHERE id=1")
            .execute(&mut *abandoned)
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *abandoned)
            .await
            .unwrap();
        wait_for(&pool, pid, false).await;
        let rolled_back: i64 = sqlx::query_scalar("SELECT value FROM phase4_example WHERE id=1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rolled_back, 0);
        sqlx::query("UPDATE phase4_example SET value=2 WHERE id=1")
            .execute(&foreground)
            .await
            .unwrap();
        let error = sqlx::query("SELECT 1")
            .execute(&mut *abandoned)
            .await
            .unwrap_err();
        assert!(
            code(&error).as_deref() == Some("25P03") || matches!(error, sqlx::Error::Io(_)),
            "{error:?}"
        );
        let ((), seen) = test_counters::scope_sync(|| crate::db::record_database_error(&error));
        // SQLx reports the termination as 25P03 or as a closed socket; each has
        // its own counter and neither is a lock error.
        let expected = if matches!(error, sqlx::Error::Io(_)) {
            DatabaseCounters {
                connection_lost: 1,
                ..Default::default()
            }
        } else {
            DatabaseCounters {
                idle_transaction_terminations: 1,
                ..Default::default()
            }
        };
        assert_eq!(seen, counted(expected, 0, 1, 0));
        drop(abandoned);
        let mut fresh = foreground.begin().await.unwrap();
        sqlx::query("SELECT 1").execute(&mut *fresh).await.unwrap();
        fresh.rollback().await.unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn retry_real_deadlock_restarts_a_fresh_transaction() {
        let pool = configured(DatabaseLimits::default()).await;
        example(&pool).await;
        let foreground = pool.foreground();
        let mut rival = foreground.begin().await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=value+1 WHERE id=2")
            .execute(&mut *rival)
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *rival)
            .await
            .unwrap();
        let points = locks::pause_points::PausePoints::default();
        let armed = points.arm("first-row");
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = attempts.clone();
        // The retry runs in a spawned task: scope it there and return what it saw.
        let writer = tokio::spawn(async move {
            let (result, seen) = test_counters::scope(crate::db::retry_transaction(|| async {
                let attempt = observed.fetch_add(1, Ordering::SeqCst);
                let mut tx = foreground.begin().await?;
                sqlx::query("SET LOCAL deadlock_timeout='50ms'")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE phase4_example SET value=value+1 WHERE id=1")
                    .execute(&mut *tx)
                    .await?;
                if attempt == 0 {
                    points.hit("first-row").await;
                }
                sqlx::query("UPDATE phase4_example SET value=value+1 WHERE id=2")
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(())
            }))
            .await;
            result.unwrap();
            seen
        });
        armed.reached().await;
        let rival = tokio::spawn(async move {
            sqlx::query("UPDATE phase4_example SET value=value+1 WHERE id=1")
                .execute(&mut *rival)
                .await
                .unwrap();
            rival.commit().await.unwrap();
        });
        wait_for(&pool, pid, true).await;
        armed.release();
        let seen = writer.await.unwrap();
        rival.await.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        // One deadlock on the first attempt (a lock error), one re-execution.
        assert_eq!(
            seen,
            counted(
                DatabaseCounters {
                    deadlocks: 1,
                    retries: 1,
                    ..Default::default()
                },
                1,
                1,
                0
            )
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT SUM(value)::bigint FROM phase4_example")
                .fetch_one(&pool)
                .await
                .unwrap(),
            4
        );
        pool.close().await;
    }

    #[tokio::test]
    async fn retry_serialization_exhausts_five_attempts_and_never_retries_lock_timeout() {
        let pool = configured(DatabaseLimits::default()).await;
        example(&pool).await;
        let foreground = pool.foreground();
        let attempts = AtomicUsize::new(0);
        let (result, seen): (anyhow::Result<()>, _) =
            test_counters::scope(crate::db::retry_transaction(|| async {
                attempts.fetch_add(1, Ordering::SeqCst);
                let mut tx = foreground
                    .begin_with("BEGIN ISOLATION LEVEL SERIALIZABLE")
                    .await?;
                sqlx::query_scalar::<_, i64>("SELECT value FROM phase4_example WHERE id=1")
                    .fetch_one(&mut *tx)
                    .await?;
                sqlx::query("UPDATE phase4_example SET value=value+1 WHERE id=1")
                    .execute(&foreground)
                    .await?;
                sqlx::query("UPDATE phase4_example SET value=value+1 WHERE id=1")
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(())
            }))
            .await;
        let error = result.unwrap_err();
        assert_eq!(
            code(error.downcast_ref::<sqlx::Error>().unwrap()).as_deref(),
            Some("40001")
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 5);
        // Five failed attempts, four re-executions, one exhaustion. A recovered
        // or exhausted 40001 is not itself a lock error; the exhaustion is.
        assert_eq!(
            seen,
            counted(
                DatabaseCounters {
                    serialization_failures: 5,
                    retries: 4,
                    retries_exhausted: 1,
                    ..Default::default()
                },
                1,
                5,
                0
            )
        );
        // The response boundary must not count the marked error again.
        let (_, recount) =
            test_counters::scope_sync(|| crate::database_error_response(error, "retry failed"));
        assert_eq!(recount, Counters::default());
        let mut held = foreground.begin().await.unwrap();
        sqlx::query("UPDATE phase4_example SET value=0 WHERE id=1")
            .execute(&mut *held)
            .await
            .unwrap();
        attempts.store(0, Ordering::SeqCst);
        let (result, seen): (anyhow::Result<()>, _) =
            test_counters::scope(crate::db::retry_transaction(|| async {
                attempts.fetch_add(1, Ordering::SeqCst);
                let mut tx = foreground.begin().await?;
                sqlx::query("SET LOCAL lock_timeout='50ms'")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE phase4_example SET value=0 WHERE id=1")
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(())
            }))
            .await;
        assert_eq!(
            code(result.unwrap_err().downcast_ref::<sqlx::Error>().unwrap()).as_deref(),
            Some("55P03")
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        // Counted once at the attempt, never retried.
        assert_eq!(
            seen,
            counted(
                DatabaseCounters {
                    lock_timeouts: 1,
                    ..Default::default()
                },
                1,
                1,
                0
            )
        );
        held.rollback().await.unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn lock_works_opposite_order_is_sorted_and_reports_missing_rows() {
        let pool = configured(DatabaseLimits::default()).await;
        sqlx::query("INSERT INTO work(id,created_at,updated_at) VALUES('w_a',now(),now()),('w_b',now(),now())").execute(&pool).await.unwrap();
        let foreground = pool.foreground();
        let mut first = foreground.begin().await.unwrap();
        let result = locks::lock_works(&mut first, &["w_b", "w_a", "w_missing", "w_a"])
            .await
            .unwrap();
        assert_eq!(result.locked, ["w_a", "w_b"]);
        assert_eq!(result.missing, ["w_missing"]);
        let mut second = foreground.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *second)
            .await
            .unwrap();
        let writer = tokio::spawn(async move {
            let result = locks::lock_works(&mut second, &["w_a", "w_b"])
                .await
                .unwrap();
            assert!(result.missing.is_empty());
            second.commit().await.unwrap();
        });
        wait_for(&pool, pid, true).await;
        first.commit().await.unwrap();
        writer.await.unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn all_advisory_classes_exclude_only_the_same_key() {
        use locks::{AdvisoryKey, KeyClass};
        let pool = configured(DatabaseLimits::default()).await;
        let foreground = pool.foreground();
        for class in [
            KeyClass::Membership,
            KeyClass::ExternalIdentity,
            KeyClass::SourceIdentity,
            KeyClass::LibraryProgress,
            KeyClass::BlockRule,
            KeyClass::ReviewPair,
            KeyClass::Username,
            KeyClass::Email,
            KeyClass::SourcePolicy,
            KeyClass::Series,
            KeyClass::Thread,
            KeyClass::Job,
            KeyClass::InstanceSetting,
            KeyClass::MatchDomain,
            KeyClass::Cover,
        ] {
            let key = AdvisoryKey::new(class, &["same"]);
            let mut first = foreground.begin().await.unwrap();
            locks::advisory(&mut first, &[key]).await.unwrap();
            let mut other = foreground.begin().await.unwrap();

            let different_class = if class == KeyClass::Email {
                KeyClass::Username
            } else {
                KeyClass::Email
            };
            locks::advisory(
                &mut other,
                &[
                    AdvisoryKey::new(class, &["different"]),
                    AdvisoryKey::new(different_class, &["same"]),
                ],
            )
            .await
            .unwrap();
            other.rollback().await.unwrap();
            let mut second = foreground.begin().await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *second)
                .await
                .unwrap();
            let writer = tokio::spawn(async move {
                locks::advisory(&mut second, &[key]).await.unwrap();
                second.commit().await.unwrap();
            });
            wait_for(&pool, pid, true).await;
            first.rollback().await.unwrap();
            writer.await.unwrap();
        }
        pool.close().await;
    }
}
