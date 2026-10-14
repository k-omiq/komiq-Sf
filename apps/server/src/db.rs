use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub use crate::database::{Database, SqliteDatabase};

pub static MIGRATOR_PG: sqlx::migrate::Migrator = sqlx::migrate!("./migrations_pg");
use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

static WRITE_QUEUE_WAIT_MICROS: AtomicU64 = AtomicU64::new(0);
static WRITE_QUEUE_ACQUISITIONS: AtomicU64 = AtomicU64::new(0);
static LOCK_ERRORS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
include!("../bench/sqlite_migration.rs");

pub fn write_queue_wait_seconds() -> f64 {
    WRITE_QUEUE_WAIT_MICROS.load(Ordering::Relaxed) as f64 / 1_000_000.0
}

pub fn write_queue_acquisitions() -> u64 {
    WRITE_QUEUE_ACQUISITIONS.load(Ordering::Relaxed)
}

pub fn lock_error_count() -> u64 {
    LOCK_ERRORS.load(Ordering::Relaxed)
}

fn database_file(database_url: &str) -> Result<Option<PathBuf>> {
    let opts = SqliteConnectOptions::from_str(database_url)?;
    if opts.get_filename() == Path::new(":memory:")
        || opts
            .get_filename()
            .to_string_lossy()
            .starts_with("file:sqlx-in-memory-")
    {
        return Ok(None);
    }
    Ok(Some(opts.get_filename().to_owned()))
}

/// Keep this guard alive until the server exits. SQLite's sqlx migrator has no
/// inter-process lock, and a second server must not run on the same data directory.
pub struct DataDirectoryLock {
    _file: Option<File>,
}

impl DataDirectoryLock {
    pub fn acquire(database_url: &str) -> Result<Self> {
        if database_url.starts_with("postgres:") || database_url.starts_with("postgresql:") {
            return Ok(Self { _file: None });
        }
        let Some(database) = database_file(database_url)? else {
            return Ok(Self { _file: None });
        };
        let directory = database.parent().unwrap_or_else(|| Path::new("."));
        let lock_path = directory.join(".komiq-server.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open data-directory lock {}", lock_path.display()))?;
        file.try_lock().with_context(|| {
            format!(
                "cannot lock {}; another server may be using this data directory",
                lock_path.display()
            )
        })?;
        Ok(Self { _file: Some(file) })
    }
}

/// Connect beside a running server without creating the file or changing schema.
/// Validate every embedded migration, including checksums, before permitting CLI work.
pub async fn connect_existing(database_url: &str) -> Result<Database> {
    let opts = sqlx::postgres::PgConnectOptions::from_str(database_url)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection argument [redacted]"))?;
    let mut limits = crate::config::DatabaseLimits::from_env();
    limits.read_connections = 2;
    connect_existing_with(opts, limits).await
}

/// `connect_existing` with caller-chosen options and pool limits: the same
/// no-create, no-migrate schema validation. Maintenance commands keep their two
/// readers; the overlap benchmark passes the production default limits.
pub async fn connect_existing_with(
    opts: sqlx::postgres::PgConnectOptions,
    limits: crate::config::DatabaseLimits,
) -> Result<Database> {
    let pool = Database::connect_with_limits(opts, limits).await?;
    if let Err(error) = validate_schema(&pool).await {
        pool.close().await;
        return Err(error);
    }
    Ok(pool)
}

/// Instrument-only existing-schema connection retaining labelled sessions. This
/// changes SQLx recycling only, never session limits, pool sizes or schema policy.
#[cfg(test)]
pub(crate) async fn connect_existing_for_benchmark_with(
    opts: sqlx::postgres::PgConnectOptions,
    limits: crate::config::DatabaseLimits,
) -> Result<Database> {
    let pool = Database::connect_for_benchmark_with_limits(opts, limits).await?;
    if let Err(error) = validate_schema(&pool).await {
        pool.close().await;
        return Err(error);
    }
    Ok(pool)
}

pub async fn connect_existing_sqlite(database_url: &str) -> Result<SqliteDatabase> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(15));
    let pool = SqliteDatabase::connect_with(opts, 2).await?;
    let schema = validate_schema_sqlite(&pool).await;
    if let Err(error) = schema {
        pool.close().await;
        return Err(error);
    }
    Ok(pool)
}

async fn validate_schema(pool: &Database) -> Result<()> {
    let applied: Vec<(i64, Vec<u8>, bool)> =
        sqlx::query_as("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .context(
                "schema is behind or uninitialized; start the server to apply migrations first",
            )?;
    let migrations = &MIGRATOR_PG;
    for migration in migrations
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
    {
        let Some((_, checksum, success)) = applied
            .iter()
            .find(|(version, _, _)| *version == migration.version)
        else {
            anyhow::bail!(
                "schema is behind: missing migration {}; start the server to migrate first",
                migration.version
            );
        };
        anyhow::ensure!(*success, "migration {} is unfinished", migration.version);
        anyhow::ensure!(
            checksum.as_slice() == migration.checksum.as_ref(),
            "migration {} checksum differs from this binary",
            migration.version
        );
    }
    for (version, _, _) in applied {
        anyhow::ensure!(
            migrations
                .iter()
                .any(|migration| migration.version == version),
            "database migration {version} is unknown to this binary"
        );
    }
    Ok(())
}

async fn validate_schema_sqlite(pool: &SqliteDatabase) -> Result<()> {
    let applied: Vec<(i64, Vec<u8>, bool)> =
        sqlx::query_as("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .context(
                "schema is behind or uninitialized; start the server to apply migrations first",
            )?;
    let migrations = sqlx::migrate!("./migrations");
    for migration in migrations
        .iter()
        .filter(|migration| !migration.migration_type.is_down_migration())
    {
        let Some((_, checksum, success)) = applied
            .iter()
            .find(|(version, _, _)| *version == migration.version)
        else {
            anyhow::bail!(
                "schema is behind: missing migration {}; start the server to migrate first",
                migration.version
            );
        };
        anyhow::ensure!(*success, "migration {} is unfinished", migration.version);
        anyhow::ensure!(
            checksum.as_slice() == migration.checksum.as_ref(),
            "migration {} checksum differs from this binary",
            migration.version
        );
    }
    for (version, _, _) in applied {
        anyhow::ensure!(
            migrations
                .iter()
                .any(|migration| migration.version == version),
            "database migration {version} is unknown to this binary"
        );
    }
    Ok(())
}

/// Compatibility classifier for PostgreSQL retryable errors and retained SQLite
/// busy errors. retry_transaction itself accepts only PostgreSQL SQLSTATEs.
pub fn is_locked_error(error: &anyhow::Error) -> bool {
    sqlx_cause(error).is_some_and(|error| database_retryable(error) || sqlite_locked(error))
}

fn sqlx_cause(error: &anyhow::Error) -> Option<&sqlx::Error> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<sqlx::Error>())
}

fn sqlite_locked(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .and_then(|code| code.parse::<u32>().ok())
        .is_some_and(|code| matches!(code & 0xff, 5 | 6))
}

fn database_retryable(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .is_some_and(|code| matches!(code.as_ref(), "40001" | "40P01"))
}

static LOCK_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static STATEMENT_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static DEADLOCKS: AtomicU64 = AtomicU64::new(0);
static SERIALIZATION_FAILURES: AtomicU64 = AtomicU64::new(0);
static IDLE_TRANSACTION_TERMINATIONS: AtomicU64 = AtomicU64::new(0);
static POOL_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static RETRIES: AtomicU64 = AtomicU64::new(0);
static RETRIES_EXHAUSTED: AtomicU64 = AtomicU64::new(0);
static CONNECTION_LOST: AtomicU64 = AtomicU64::new(0);
static TRANSACTION_RESTARTS: AtomicU64 = AtomicU64::new(0);

/// Marks an error observed at an inner operation boundary. Anyhow preserves
/// this typed context through added contexts; response/job boundaries skip it.
#[derive(Debug)]
pub struct Observed(String);
impl std::fmt::Display for Observed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Observed {}

/// Observe an operation at a TERMINAL boundary: the error is logged/swallowed or
/// turned into a final response here and goes no further. A borrowed error cannot
/// be marked, so calling this and then propagating the same error (or passing it
/// to `database_error_response`/`gql_err`) counts it twice. A boundary that
/// propagates must use `observed_database_error` and return that marked error
/// intact; converting to `String` loses both the marker and the classification.
pub fn observe_database_error(error: &anyhow::Error) {
    if error.downcast_ref::<Observed>().is_none() {
        if let Some(cause) = sqlx_cause(error) {
            record_database_error(cause);
        }
    }
}
pub fn observed_database_error(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<Observed>().is_some() || sqlx_cause(&error).is_none() {
        error
    } else {
        observe_database_error(&error);
        let message = error.to_string();
        error.context(Observed(message))
    }
}

/// Mark a known read-only operation before passing it to a general response
/// boundary. Its pool timeouts count without inflating write-lane lock errors;
/// its I/O losses are not labelled write-lane connection loss.
#[allow(dead_code)]
pub fn observed_read_database_error(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<Observed>().is_some() || sqlx_cause(&error).is_none() {
        error
    } else {
        if let Some(cause) = sqlx_cause(&error) {
            record_read_database_error(cause);
        }
        let message = error.to_string();
        error.context(Observed(message))
    }
}

/// Bound standalone catalogue snapshots, including those waiting for publication.
/// Retain this permit in RefreshSnapshot until its connection closes/drops.
#[allow(dead_code)]
pub async fn snapshot_session_permit(
) -> Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError> {
    static SLOTS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    SLOTS
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(8)))
        .clone()
        .acquire_owned()
        .await
}

/// Session policy for standalone readiness, catalogue snapshot and import sessions.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum Lane {
    Readers,
    Foreground,
    Background,
}
pub async fn apply_session_limits(
    connection: &mut sqlx::PgConnection,
    lane: Lane,
) -> Result<(), sqlx::Error> {
    let limits = crate::config::DatabaseLimits::from_env();
    let limits = match lane {
        Lane::Readers => limits.readers,
        Lane::Foreground => limits.foreground,
        Lane::Background => limits.background,
    };
    crate::database::configure_session(connection, limits).await
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub struct DatabaseCounters {
    pub lock_timeouts: u64,
    pub statement_timeouts: u64,
    pub deadlocks: u64,
    pub serialization_failures: u64,
    pub idle_transaction_terminations: u64,
    pub pool_timeouts: u64,
    pub retries: u64,
    pub retries_exhausted: u64,
    pub connection_lost: u64,
    pub transaction_restarts: u64,
}

impl DatabaseCounters {
    #[cfg(test)]
    pub fn since(self, before: Self) -> Self {
        Self {
            connection_lost: self.connection_lost.saturating_sub(before.connection_lost),
            transaction_restarts: self
                .transaction_restarts
                .saturating_sub(before.transaction_restarts),
            lock_timeouts: self.lock_timeouts.saturating_sub(before.lock_timeouts),
            statement_timeouts: self
                .statement_timeouts
                .saturating_sub(before.statement_timeouts),
            deadlocks: self.deadlocks.saturating_sub(before.deadlocks),
            serialization_failures: self
                .serialization_failures
                .saturating_sub(before.serialization_failures),
            idle_transaction_terminations: self
                .idle_transaction_terminations
                .saturating_sub(before.idle_transaction_terminations),
            pool_timeouts: self.pool_timeouts.saturating_sub(before.pool_timeouts),
            retries: self.retries.saturating_sub(before.retries),
            retries_exhausted: self
                .retries_exhausted
                .saturating_sub(before.retries_exhausted),
        }
    }

    pub fn prometheus(self) -> String {
        let mut text = String::new();
        for (name, value) in [
            ("connection_lost", self.connection_lost),
            ("transaction_restarts", self.transaction_restarts),
            ("lock_timeouts", self.lock_timeouts),
            ("statement_timeouts", self.statement_timeouts),
            ("deadlocks", self.deadlocks),
            ("serialization_failures", self.serialization_failures),
            (
                "idle_transaction_terminations",
                self.idle_transaction_terminations,
            ),
            ("pool_acquire_timeouts", self.pool_timeouts),
            ("transaction_retries", self.retries),
            ("transaction_retries_exhausted", self.retries_exhausted),
        ] {
            use std::fmt::Write;
            let _ = writeln!(
                text,
                "# TYPE komika_postgres_{name}_total counter\nkomika_postgres_{name}_total {value}"
            );
        }
        text
    }
}

pub fn database_counters() -> DatabaseCounters {
    DatabaseCounters {
        connection_lost: CONNECTION_LOST.load(Ordering::Relaxed),
        transaction_restarts: TRANSACTION_RESTARTS.load(Ordering::Relaxed),
        lock_timeouts: LOCK_TIMEOUTS.load(Ordering::Relaxed),
        statement_timeouts: STATEMENT_TIMEOUTS.load(Ordering::Relaxed),
        deadlocks: DEADLOCKS.load(Ordering::Relaxed),
        serialization_failures: SERIALIZATION_FAILURES.load(Ordering::Relaxed),
        idle_transaction_terminations: IDLE_TRANSACTION_TERMINATIONS.load(Ordering::Relaxed),
        pool_timeouts: POOL_TIMEOUTS.load(Ordering::Relaxed),
        retries: RETRIES.load(Ordering::Relaxed),
        retries_exhausted: RETRIES_EXHAUSTED.load(Ordering::Relaxed),
    }
}

/// Record an error deliberately swallowed (log and continue). Propagated errors
/// belong at retry/response/job boundaries instead. No helpers/executors count.
pub fn record_database_error(error: &sqlx::Error) {
    record_database_error_in_lane(error, true);
}

#[allow(dead_code)]
pub fn record_read_database_error(error: &sqlx::Error) {
    record_database_error_in_lane(error, false);
}

/// Every counted event goes through here, so a test-owned sink (see
/// `test_counters`) sees exactly the events of its own task tree.
fn count(counter: &'static AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    test_counters::record(counter);
}

fn record_database_error_in_lane(error: &sqlx::Error, write_lane: bool) {
    let code = error.as_database_error().and_then(|e| e.code());
    let counter = match code.as_deref() {
        Some("55P03") => &LOCK_TIMEOUTS,
        Some("57014") => &STATEMENT_TIMEOUTS,
        Some("40P01") => &DEADLOCKS,
        Some("40001") => &SERIALIZATION_FAILURES,
        Some("25P03") => &IDLE_TRANSACTION_TERMINATIONS,
        _ if write_lane && matches!(error, sqlx::Error::Io(_)) => &CONNECTION_LOST,
        _ if matches!(error, sqlx::Error::PoolTimedOut) => &POOL_TIMEOUTS,
        _ => return,
    };
    count(counter);
    #[cfg(test)]
    test_counters::record_lane(write_lane);
    if write_lane
        && (matches!(code.as_deref(), Some("55P03" | "40P01"))
            || matches!(error, sqlx::Error::PoolTimedOut))
    {
        count(&LOCK_ERRORS);
    }
}

pub(crate) fn record_sqlite_error(error: &sqlx::Error) {
    if sqlite_locked(error) {
        count(&LOCK_ERRORS);
    }
}

/// Test-owned observation of the process-global counters. Other tests in the
/// same process move the globals at any time, so an exact-delta assertion on
/// them is a false failure waiting to happen. `scope` gives the future its own
/// sink through a Tokio task local: only events recorded while THAT future is
/// polled are seen. `tokio::spawn` does not inherit task locals, so a spawned
/// task is covered only when it is itself wrapped (`current` + `within`); the
/// Database deferred worker does this for jobs queued inside a scope.
#[cfg(test)]
pub mod test_counters {
    use super::DatabaseCounters;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};

    /// Deltas seen by one scope. `write_lane_events`/`read_lane_events` count
    /// classified SQLx errors by the lane they were recorded in; they exist only
    /// here (production has one pool-timeout counter for both lanes).
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct Counters {
        pub database: DatabaseCounters,
        pub lock_errors: u64,
        pub write_lane_events: u64,
        pub read_lane_events: u64,
    }

    #[derive(Clone, Debug, Default)]
    pub struct Sink(Arc<Mutex<Counters>>);

    tokio::task_local! {
        static SINK: Sink;
    }

    /// Run `work` with a fresh sink and return what it counted. A nested scope
    /// shadows the outer one: its events are not added to the outer sink.
    pub async fn scope<F: std::future::Future>(work: F) -> (F::Output, Counters) {
        let sink = Sink::default();
        let output = SINK.scope(sink.clone(), work).await;
        let seen = *sink.0.lock().unwrap();
        (output, seen)
    }

    /// `scope` for code that counts synchronously.
    pub fn scope_sync<T>(work: impl FnOnce() -> T) -> (T, Counters) {
        let sink = Sink::default();
        let output = SINK.sync_scope(sink.clone(), work);
        let seen = *sink.0.lock().unwrap();
        (output, seen)
    }

    /// The sink of the running scope, to hand to a task that outlives the poll.
    pub fn current() -> Option<Sink> {
        SINK.try_with(Clone::clone).ok()
    }

    /// Run `work` (typically inside a spawned task) under a captured sink.
    pub async fn within<F: std::future::Future>(sink: Option<Sink>, work: F) -> F::Output {
        match sink {
            Some(sink) => SINK.scope(sink, work).await,
            None => work.await,
        }
    }

    pub(super) fn record(counter: &'static AtomicU64) {
        let _ = SINK.try_with(|sink| {
            let mut seen = sink.0.lock().unwrap();
            let seen = &mut *seen;
            let fields: [(&'static AtomicU64, &mut u64); 11] = [
                (&super::LOCK_ERRORS, &mut seen.lock_errors),
                (&super::LOCK_TIMEOUTS, &mut seen.database.lock_timeouts),
                (
                    &super::STATEMENT_TIMEOUTS,
                    &mut seen.database.statement_timeouts,
                ),
                (&super::DEADLOCKS, &mut seen.database.deadlocks),
                (
                    &super::SERIALIZATION_FAILURES,
                    &mut seen.database.serialization_failures,
                ),
                (
                    &super::IDLE_TRANSACTION_TERMINATIONS,
                    &mut seen.database.idle_transaction_terminations,
                ),
                (&super::POOL_TIMEOUTS, &mut seen.database.pool_timeouts),
                (&super::RETRIES, &mut seen.database.retries),
                (
                    &super::RETRIES_EXHAUSTED,
                    &mut seen.database.retries_exhausted,
                ),
                (&super::CONNECTION_LOST, &mut seen.database.connection_lost),
                (
                    &super::TRANSACTION_RESTARTS,
                    &mut seen.database.transaction_restarts,
                ),
            ];
            let field = fields
                .into_iter()
                .find(|(global, _)| std::ptr::eq(*global, counter))
                .expect("every counted global has a sink field")
                .1;
            *field += 1;
        });
    }

    pub(super) fn record_lane(write_lane: bool) {
        let _ = SINK.try_with(|sink| {
            let mut seen = sink.0.lock().unwrap();
            if write_lane {
                seen.write_lane_events += 1;
            } else {
                seen.read_lane_events += 1;
            }
        });
    }
}

pub(crate) fn record_write_wait(wait: Duration) {
    WRITE_QUEUE_WAIT_MICROS.fetch_add(wait.as_micros() as u64, Ordering::Relaxed);
    WRITE_QUEUE_ACQUISITIONS.fetch_add(1, Ordering::Relaxed);
}

/// The typed restart marker, wherever it sits in the error: an anyhow context,
/// the error itself, or a `std::error::Error::source()` further down (for example
/// a helper that wrapped it in `sqlx::Error::Decode`). A `String` conversion
/// carries neither this marker nor a SQLSTATE and is never retried.
fn restart_requested(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::locks::RestartTransaction>()
        .is_some()
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<crate::locks::RestartTransaction>()
                .is_some()
        })
}

/// Attached, as typed anyhow context, to the error of a unit whose five attempts
/// are spent. The retryable cause (40001, 40P01 or RestartTransaction) stays in
/// the chain underneath, so without this marker an enclosing `retry_transaction`
/// would see a retryable error and run the whole inner retry again: up to 25
/// executions for two nested levels. An error that carries the marker is final.
#[derive(Debug)]
pub struct RetryExhausted;
impl std::fmt::Display for RetryExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("transaction retry bound exhausted after five attempts")
    }
}
impl std::error::Error for RetryExhausted {}

/// The exhaustion marker, wherever it sits: an anyhow context at any depth, the
/// error itself, or a `std::error::Error::source()` further down. Text is never
/// a marker.
fn retry_exhausted(error: &anyhow::Error) -> bool {
    error.downcast_ref::<RetryExhausted>().is_some()
        || error
            .chain()
            .any(|cause| cause.downcast_ref::<RetryExhausted>().is_some())
}

/// Retry a complete unit, BEGIN through COMMIT, with no external side effects or
/// earlier commits in the callback. Only 40001, 40P01 and typed RestartTransaction
/// (direct, as context, or through the source chain) are retried, with a fresh
/// transaction each time. Five attempts total, jittered 25/50/100/200ms backoff.
/// The bound holds when retries nest: an error that already carries
/// `RetryExhausted` is returned at once, without another attempt or count.
pub async fn retry_transaction<T, F, Fut>(attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    retry_transaction_with(attempt, tokio::time::sleep).await
}

/// `retry_transaction` with the wait between attempts supplied by the caller,
/// so a test can see every delay without measuring a clock.
async fn retry_transaction_with<T, F, Fut, S, Slept>(mut attempt: F, mut sleep: S) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
    S: FnMut(Duration) -> Slept,
    Slept: std::future::Future<Output = ()>,
{
    use rand_core::RngCore;
    for retry in 0..5u32 {
        let result = attempt().await.map_err(observed_database_error);
        match result {
            Ok(value) => return Ok(value),
            Err(error) => {
                // An inner retry already spent the bound on this error (and
                // counted the exhaustion): final here too.
                if retry_exhausted(&error) {
                    return Err(error);
                }
                let restart = restart_requested(&error);
                if !restart && !sqlx_cause(&error).is_some_and(database_retryable) {
                    return Err(error);
                }
                if retry == 4 {
                    count(&RETRIES_EXHAUSTED);
                    count(&LOCK_ERRORS);
                    return Err(error.context(RetryExhausted));
                }
                if restart {
                    count(&TRANSACTION_RESTARTS);
                }
                count(&RETRIES);
                let base = 25u64 << retry;
                let jitter = rand_core::OsRng.next_u64() % (base + 1);
                sleep(Duration::from_millis(base + jitter)).await;
            }
        }
    }
    unreachable!("last attempt returns")
}

/// Open the main PostgreSQL database and apply only the PostgreSQL history.
pub async fn init(database_url: &str) -> Result<Database> {
    let options = sqlx::postgres::PgConnectOptions::from_str(database_url)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection argument [redacted]"))?;
    let pool =
        Database::connect_with_limits(options, crate::config::DatabaseLimits::from_env()).await?;
    if let Err(error) = MIGRATOR_PG.run(pool.write()).await {
        pool.close().await;
        return Err(error.into());
    }
    Ok(pool)
}

/// Open (creating if needed) the SEPARATE cover-cache DB and ensure its schema.
///
/// This DB holds only `work_cover_blob` (large, re-derivable-from-MangaDex cover
/// thumbnails). It is deliberately NOT the main DB: only the main PostgreSQL DB is
/// backed up, so keeping covers here excludes them from backup while they're
/// still served from our own origin. Schema is a single table, created inline (no
/// migration directory) — there is no FK to `work` because that table lives in the
/// other DB; an orphaned blob (its work deleted) is harmless and re-derivable.
pub async fn init_covers(database_url: &str) -> Result<SqliteDatabase> {
    match open_covers(database_url).await {
        Ok(pool) => Ok(pool),
        Err(error) if covers_file_failure(&error) => {
            let Some(path) = database_file(database_url)? else {
                return Err(error);
            };
            if !path.try_exists()? {
                return Err(error);
            }
            let quarantined = quarantine_covers(&path)?;
            tracing::warn!(error = %error, file = %quarantined.display(),
                "cover cache unavailable; preserved old files and recreating derived covers");
            open_covers(database_url).await
        }
        Err(error) => Err(error),
    }
}

fn covers_file_failure(error: &anyhow::Error) -> bool {
    error.chain().any(|error| {
        error
            .downcast_ref::<sqlx::Error>()
            .and_then(sqlx::Error::as_database_error)
            .and_then(|error| error.code())
            .and_then(|code| code.parse::<i32>().ok())
            .is_some_and(|code| matches!(code & 0xff, 11 | 14 | 26))
    })
}

fn quarantine_covers(path: &Path) -> Result<PathBuf> {
    let mut destination = path.as_os_str().to_owned();
    destination.push(format!(".quarantined-{}", uuid::Uuid::new_v4()));
    let destination = PathBuf::from(destination);
    // All handles are closed before renaming. Preserve the WAL and shared-memory
    // siblings too; leaving them beside the new DB can replay stale cache pages.
    // Move sidecars before the database. If a sidecar cannot be moved, boot aborts
    // with the original main file still present rather than creating a new file
    // alongside an old WAL on the next attempt.
    for suffix in ["-wal", "-shm"] {
        let mut source = path.as_os_str().to_owned();
        source.push(suffix);
        let source = PathBuf::from(source);
        if source.try_exists()? {
            let mut target = destination.as_os_str().to_owned();
            target.push(suffix);
            std::fs::rename(source, target).context("preserve cover-cache SQLite sidecar")?;
        }
    }
    std::fs::rename(path, &destination).context("preserve unavailable cover cache")?;
    Ok(destination)
}

/// Maintenance commands must never recreate or upgrade a live cover cache.
pub async fn connect_existing_covers(database_url: &str) -> Result<SqliteDatabase> {
    let pool = SqliteDatabase::connect_with(
        SqliteConnectOptions::from_str(database_url)?.create_if_missing(false),
        2,
    )
    .await?;
    let version = sqlx::query_scalar::<_, i64>("PRAGMA user_version")
        .fetch_one(&pool)
        .await;
    if !matches!(version, Ok(1)) {
        pool.close().await;
        anyhow::bail!(
            "cover cache schema is behind or unsupported; start the server to upgrade it first"
        );
    }
    if let Err(error) = sqlx::query("SELECT work_id, version FROM work_cover_blob LIMIT 1")
        .fetch_optional(&pool)
        .await
    {
        pool.close().await;
        return Err(error.into());
    }
    Ok(pool)
}

async fn open_covers(database_url: &str) -> Result<SqliteDatabase> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        // Match the main pool's contention margin + per-connection cache tuning
        // (see `init`); the cover DB is point-lookup heavy but shares the writer
        // stalls during a cover-materialization burst.
        //
        // mmap_size stays at 256 MB here, deliberately unlike the main pool. This DB is
        // 20.5 GB of ~170 KB blobs read once and streamed to the client; mapping them
        // would push single-use blob pages through the page cache at the expense of
        // everything else. 256 MB comfortably holds the PK index (121k keys) and the
        // hot end of the table, which is all that benefits.
        .busy_timeout(Duration::from_secs(15))
        .pragma("mmap_size", "268435456")
        .pragma("cache_size", "-16000")
        .pragma("temp_store", "MEMORY");
    let pool = SqliteDatabase::connect_with(opts, 4).await?;
    let schema = async {
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await?;
        anyhow::ensure!(
            version <= 1,
            "cover cache schema version {version} is newer than this server supports"
        );
        if version == 0 {
            // Version zero is the legacy inline schema. Upgrade it atomically without
            // discarding cached blobs; future changes get their own version branch.
            let mut transaction = pool.begin().await?;
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS work_cover_blob (\
             work_id    TEXT PRIMARY KEY,\
             webp       BLOB NOT NULL,\
             version    INTEGER NOT NULL,\
             updated_at TEXT NOT NULL\
         )",
            )
            .execute(&mut *transaction)
            .await?;
            // Resized, origin-cached Suwayomi SOURCE covers, keyed by numeric Suwayomi manga
            // id. The discovery/browse/search feeds hand back the raw `/api/v1/manga/{id}/
            // thumbnail` proxy path (full-resolution, up to several MB), so `serve_suwayomi_image`
            // downscales each cover to a bounded WebP on first request and caches it here.
            // Same rationale as `work_cover_blob`: large, re-derivable, and deliberately
            // excluded from the main-database backup.
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS suwayomi_cover_blob (\
             manga_id   INTEGER PRIMARY KEY,\
             webp       BLOB NOT NULL,\
             updated_at TEXT NOT NULL\
         )",
            )
            .execute(&mut *transaction)
            .await?;
            // Card-sized derivatives of the two cover masters above, generated lazily when a
            // request asks for an allowlisted `?w=` (see `cover::VARIANT_WIDTHS`). `asset_key`
            // namespaces the families that share this table: `w_…` for a canonical work,
            // `s:<manga_id>` for a Suwayomi source cover. Purely derived and re-generable from
            // the master on the next request, so — like the masters — it is deliberately
            // excluded from the main-database backup, and dropping the table is always safe.
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS cover_variant (\
             asset_key  TEXT NOT NULL,\
             width      INTEGER NOT NULL,\
             webp       BLOB NOT NULL,\
             updated_at TEXT NOT NULL,\
             PRIMARY KEY (asset_key, width)\
         )",
            )
            .execute(&mut *transaction)
            .await?;
            // Durable cursors for one-shot maintenance passes over this DB. One row per pass,
            // holding the highest key it has already handled, so a restart resumes instead of
            // re-scanning 20 GB of blobs. Lives here (not the main DB) because both the data it
            // walks and the progress marker are cover-cache state: dropping this file loses the
            // cursor and the covers together, which is consistent, and neither needs backup.
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS cover_maintenance (\
             key        TEXT PRIMARY KEY,\
             cursor     TEXT NOT NULL,\
             done       INTEGER NOT NULL DEFAULT 0,\
             updated_at TEXT NOT NULL\
         )",
            )
            .execute(&mut *transaction)
            .await?;
            // Extension icons Keiyoushi published AFTER the vendored snapshot in
            // `assets/ext-icons/` was taken (refreshed by `scripts/fetch-ext-icons.mjs`).
            // The baked-in set is authoritative and answers essentially every request;
            // this table only backfills extensions added upstream since, so the icons
            // don't go stale between snapshot refreshes. Same rationale as the cover
            // tables for living here: re-derivable from Keiyoushi, so deliberately
            // excluded from the main-database backup.
            //
            // An EMPTY `png` is a tombstone, not an icon: ~41 extensions publish no icon
            // in either source, and without a recorded miss every request for one would
            // re-fetch from Keiyoushi. `serve_ext_icon` serves 404 for a tombstone and
            // only re-checks upstream once it ages past its TTL.
            sqlx::query(
                "CREATE TABLE IF NOT EXISTS extension_icon_blob (\
             pkg_name   TEXT PRIMARY KEY,\
             png        BLOB NOT NULL,\
             updated_at TEXT NOT NULL\
         )",
            )
            .execute(&mut *transaction)
            .await?;
            sqlx::query("PRAGMA user_version = 1")
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
        }
        // Touch every cache table's key index, without reading the 20 GB of blobs.
        // Open/schema failures and corruption found here take the same recovery path.
        for table in [
            "work_cover_blob",
            "suwayomi_cover_blob",
            "cover_variant",
            "cover_maintenance",
            "extension_icon_blob",
        ] {
            sqlx::query(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(&pool)
                .await?;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(error) = schema {
        pool.close().await;
        return Err(error);
    }
    Ok(pool)
}

/// Repair cover pointers that disagree with the cover cache.
///
/// Two disagreements exist, and both come from the blob and its pointer living in
/// different databases:
///
/// * the pointer names a blob that is missing (a partly lost or recreated cache). The
///   pointer is cleared and the cover is fetched again;
/// * the pointer names an OLDER version than the stored blob. A publication commits the
///   blob first and the pointer second (`cover::publish_work_cover`), so a PostgreSQL
///   failure between the two leaves blob version 2 behind pointer version 1. Nothing
///   else repairs that: the drainer only looks at NULL pointers. The pointer and the
///   card URLs are moved to the blob's version here.
///
/// One unguarded probe per batch of [`COVER_POINTER_BATCH`] pointers finds the suspects.
/// Only a batch that has one opens a transaction, and it is ONE transaction for the
/// whole batch: one sorted advisory call for the suspects' Cover keys, one sorted
/// `lock_works`, a second read of the blobs under those guards, and conditional writes
/// that name the pointer value the probe saw. A publication that overlaps therefore
/// either finishes first (and the second read sees its blob) or waits for the batch.
pub async fn reconcile_cover_pointers(pool: &Database, covers: &SqliteDatabase) -> Result<u64> {
    let mut cursor = String::new();
    let mut repaired = 0;
    loop {
        let pointers: Vec<(String, i64)> = sqlx::query_as(
            "SELECT id, cover_cached_version FROM work \
             WHERE id > $1 AND cover_cached_version IS NOT NULL ORDER BY id LIMIT $2",
        )
        .bind(&cursor)
        .bind(COVER_POINTER_BATCH)
        .fetch_all(pool)
        .await?;
        let Some((last, _)) = pointers.last() else {
            break;
        };
        cursor.clone_from(last);
        let ids: Vec<&str> = pointers.iter().map(|(id, _)| id.as_str()).collect();
        let blobs = cover_blob_versions(covers, &ids).await?;
        // Sorted by id already (the keyset page), which is the order the guards need.
        let suspects: Vec<(String, i64)> = pointers
            .into_iter()
            .filter(|(id, version)| blobs.get(id) != Some(version))
            .collect();
        if suspects.is_empty() {
            continue;
        }
        repaired +=
            retry_transaction(|| repair_cover_pointer_batch(pool, covers, &suspects)).await?;
    }
    Ok(repaired)
}

/// Pointers read, and at most repaired, per transaction of [`reconcile_cover_pointers`].
const COVER_POINTER_BATCH: i64 = 256;

/// The stored blob version of each of `ids` that has a blob.
async fn cover_blob_versions(
    covers: &SqliteDatabase,
    ids: &[&str],
) -> Result<std::collections::HashMap<String, i64>> {
    let marks = crate::sql_params::placeholders(1, ids.len());
    let sql = format!("SELECT work_id, version FROM work_cover_blob WHERE work_id IN ({marks})");
    let mut query = sqlx::query_as::<_, (String, i64)>(&sql);
    for id in ids {
        query = query.bind(*id);
    }
    Ok(query.fetch_all(covers).await?.into_iter().collect())
}

/// One transaction of [`reconcile_cover_pointers`]: `suspects` is sorted by work id.
async fn repair_cover_pointer_batch(
    pool: &Database,
    covers: &SqliteDatabase,
    suspects: &[(String, i64)],
) -> Result<u64> {
    let ids: Vec<&str> = suspects.iter().map(|(id, _)| id.as_str()).collect();
    let mut transaction = pool.begin().await?;
    let _assets = crate::cover::lock_assets(&mut transaction, &ids).await?;
    crate::locks::lock_works(&mut transaction, &ids).await?;
    // The probe is stale by now: a publication may have committed a blob in between.
    // Decide again under the guards a publication holds.
    let blobs = cover_blob_versions(covers, &ids).await?;
    let mut work_ids = Vec::new();
    let mut seen = Vec::new();
    let mut current: Vec<Option<i64>> = Vec::new();
    let mut urls: Vec<Option<String>> = Vec::new();
    for (id, version) in suspects {
        let blob = blobs.get(id).copied();
        if blob == Some(*version) {
            continue;
        }
        work_ids.push(id.as_str());
        seen.push(*version);
        current.push(blob);
        urls.push(blob.map(|version| crate::cover::cover_path(id, version)));
    }
    if work_ids.is_empty() {
        transaction.rollback().await?;
        return Ok(0);
    }
    // Conditional on the pointer value the probe saw: a pointer that moved since is
    // somebody else's publication and is left alone.
    let changed: Vec<String> = sqlx::query_scalar(
        "UPDATE work w SET cover_cached_version = r.version \
           FROM UNNEST($1::text[], $2::bigint[], $3::bigint[]) AS r(id, seen, version) \
          WHERE w.id = r.id AND w.cover_cached_version = r.seen \
         RETURNING w.id",
    )
    .bind(&work_ids)
    .bind(&seen)
    .bind(&current)
    .fetch_all(&mut *transaction)
    .await?;
    // A pointer that moved to a newer blob takes its card URLs with it, as a
    // publication does. A cleared pointer keeps the cards as they are: the cover
    // route fetches the art again on the next request.
    let changed: std::collections::HashSet<&str> = changed.iter().map(String::as_str).collect();
    let (moved_ids, moved_urls): (Vec<&str>, Vec<&str>) = work_ids
        .iter()
        .zip(&urls)
        .filter(|(id, _)| changed.contains(**id))
        .filter_map(|(id, url)| Some((*id, url.as_deref()?)))
        .unzip();
    if !moved_ids.is_empty() {
        for table in ["feed_updates", "feed_series_updates", "browse_catalogue"] {
            sqlx::query(&format!(
                "UPDATE {table} t SET cover_url = r.url \
                   FROM UNNEST($1::text[], $2::text[]) AS r(id, url) \
                  WHERE t.work_id = r.id AND t.cover_url IS DISTINCT FROM r.url"
            ))
            .bind(&moved_ids)
            .bind(&moved_urls)
            .execute(&mut *transaction)
            .await?;
        }
    }
    transaction.commit().await?;
    Ok(changed.len() as u64)
}

/// Tables whose PostgreSQL statistics the planner depends on, and whose row
/// counts / key distributions move enough to go stale.
///
/// `work_fts` is deliberately absent: `ANALYZE` skips virtual tables, so naming it is a
/// silent no-op.
const ANALYZE_TABLES: &[&str] = &[
    "feed_updates",
    "source_series",
    "suwayomi_series",
    "series_scan_state",
    "merge_candidate",
    "notifications",
    // Browse's own table (migration 0069) and the updates feed's (0064). Both are
    // wholesale-rebuilt on every sync cycle, so their stats go stale by construction.
    //
    // `browse_catalogue` matters most: it is the largest table the planner has to choose
    // an index for (115,567 rows across eight indices), and 0069's query plans were
    // verified on an ANALYZEd copy. Without stat rows the planner falls back to its
    // ~1M-row default and can miss the intended index for a filter combination —
    // reintroducing exactly the temp-B-tree sort over the whole table that those eight
    // indices exist to prevent. That is the regression this list's own doc comment
    // describes ("leaving the newest tables with no stat rows at all"), repeated on the
    // newest table again.
    "browse_catalogue",
    "feed_series_updates",
];

/// How often stats are refreshed. Well under the rate at which these tables change
/// shape, and cheap enough that the interval is set by "don't be stale", not by cost.
///
/// Each table is analyzed separately with a fixed sample budget, releasing the
/// writer between tables instead of scanning millions of entries under one lock.
const ANALYZE_INTERVAL_SECS: u64 = 6 * 60 * 60;

/// Spawn the recurring statistics refresh.
///
/// Preserve the existing paced statistics refresh during the engine flip.
/// PostgreSQL refreshes table statistics with ANALYZE; it does not use SQLite
/// index-by-index analysis or PRAGMA analysis_limit.
///
pub fn spawn_analyze(pool: Database, shutdown: tokio::sync::watch::Receiver<bool>) {
    let pool = pool.background();
    tokio::spawn(crate::task::supervise(
        "analyze",
        Duration::from_secs(60),
        shutdown.clone(),
        move || analyze_loop(pool.clone(), shutdown.clone()),
    ));
}

async fn analyze_loop(pool: Database, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    // 300s behind boot: the last slot in the background-loop stagger (scanner at 0,
    // feed/FTS rebuild at 20s, mangadex at 60s, GC at 150s), so refreshing stats never
    // lands inside the boot write burst.
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(300),
        Duration::from_secs(ANALYZE_INTERVAL_SECS),
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(
        interval_secs = ANALYZE_INTERVAL_SECS,
        "query-statistics refresh started"
    );
    loop {
        tokio::select! {
            _ = ticker.tick() => refresh_statistics(&pool).await,
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("query-statistics refresh stopping");
                    break;
                }
            }
        }
    }
}

/// Refresh one table at a time, retaining the Phase 3 single-writer lane.
async fn refresh_statistics(pool: &Database) {
    let started = std::time::Instant::now();
    let mut ok = 0usize;
    for table in ANALYZE_TABLES {
        let sql = format!("ANALYZE public.\"{}\"", table.replace('"', "\"\""));
        match sqlx::query(&sql).execute(pool).await {
            Ok(_) => ok += 1,
            Err(error) => {
                record_database_error(&error);
                tracing::warn!(table, %error, "analyze: table failed");
            }
        }
        tokio::task::yield_now().await;
    }
    tracing::info!(
        tables = ok,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "query statistics refreshed"
    );
}

#[cfg(test)]
mod boot_tests {
    use super::*;

    #[tokio::test]
    async fn main_pool_uses_postgres_native_types_and_utc_on_both_lanes() {
        let pool = Database::connect("sqlite::memory:").await.unwrap();
        for connection_pool in [pool.read(), pool.write()] {
            let timezone: String = sqlx::query_scalar("SHOW TIME ZONE")
                .fetch_one(connection_pool)
                .await
                .unwrap();
            assert_eq!(timezone, "UTC");
            let flag_type: String = sqlx::query_scalar("SELECT data_type FROM information_schema.columns WHERE table_schema='public' AND table_name='work' AND column_name='is_nsfw' ").fetch_one(connection_pool).await.unwrap();
            assert_eq!(flag_type, "boolean");
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn cover_schema_version_upgrades_legacy_data_and_rejects_future_versions() {
        let scratch = Scratch::new();
        let url = scratch.url("covers.sqlite3");
        let legacy = SqliteDatabase::connect_with(
            SqliteConnectOptions::from_str(&url)
                .unwrap()
                .create_if_missing(true),
            2,
        )
        .await
        .unwrap();
        sqlx::query("CREATE TABLE work_cover_blob(work_id TEXT PRIMARY KEY,webp BLOB NOT NULL,version INTEGER NOT NULL,updated_at TEXT NOT NULL); INSERT INTO work_cover_blob VALUES ('w_test',x'0102',7,'2026-01-01')")
            .execute(&legacy).await.unwrap();
        legacy.close().await;
        let upgraded = init_covers(&url).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("PRAGMA user_version")
                .fetch_one(&upgraded)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, Vec<u8>>(
                "SELECT webp FROM work_cover_blob WHERE work_id='w_test'"
            )
            .fetch_one(&upgraded)
            .await
            .unwrap(),
            vec![1, 2]
        );
        sqlx::query("PRAGMA user_version=99")
            .execute(&upgraded)
            .await
            .unwrap();
        upgraded.close().await;
        assert!(init_covers(&url)
            .await
            .unwrap_err()
            .to_string()
            .contains("newer"));
        assert_eq!(
            std::fs::read_dir(&scratch.0)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("quarantined"))
                .count(),
            0
        );
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("komiq-boot-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn url(&self, name: &str) -> String {
            format!("sqlite://{}", self.0.join(name).display())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn data_directory_lock_excludes_a_second_process_and_releases_on_drop() {
        let scratch = Scratch::new();
        let url = scratch.url("main.sqlite3");
        let guard = DataDirectoryLock::acquire(&url).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "db::boot_tests::lock_child", "--nocapture"])
            .env("KOMIQ_TEST_LOCK_DATABASE", &url)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "child could acquire the server's lifetime lock"
        );
        drop(guard);
        let _next_server = DataDirectoryLock::acquire(&url).unwrap();
    }

    #[test]
    fn lock_child() {
        if let Ok(url) = std::env::var("KOMIQ_TEST_LOCK_DATABASE") {
            assert!(DataDirectoryLock::acquire(&url).is_err());
        }
    }

    #[tokio::test]
    async fn maintenance_connection_rejects_a_missing_database_without_creating_it() {
        let scratch = Scratch::new();
        assert!(connect_existing(&scratch.url("missing.sqlite3"))
            .await
            .is_err());
        assert!(!scratch.0.join("missing.sqlite3").exists());
    }

    #[tokio::test]
    async fn maintenance_connection_refuses_behind_schema_without_migrating() {
        let server = Database::connect("sqlite::memory:").await.unwrap();
        let url = server.test_url();
        let latest = MIGRATOR_PG.iter().last().unwrap().version;
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
            .bind(latest)
            .execute(&server)
            .await
            .unwrap();
        let error = connect_existing(&url).await.unwrap_err();
        assert!(error.to_string().contains("schema is behind"));
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE version = $1")
                .bind(latest)
                .fetch_one(&server)
                .await
                .unwrap();
        assert_eq!(
            rows, 0,
            "maintenance command ran a migration beside the server"
        );
        server.close().await;
    }

    #[tokio::test]
    async fn maintenance_connection_accepts_current_schema_and_checks_checksums() {
        let server = Database::connect("sqlite::memory:").await.unwrap();
        let url = server.test_url();
        let maintenance = connect_existing(&url).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT 1::bigint")
                .fetch_one(&maintenance)
                .await
                .unwrap(),
            1
        );
        maintenance.close().await;
        sqlx::query("UPDATE _sqlx_migrations SET checksum = decode('00','hex') WHERE version = 1")
            .execute(&server)
            .await
            .unwrap();
        assert!(connect_existing(&url)
            .await
            .unwrap_err()
            .to_string()
            .contains("checksum differs"));
        server.close().await;
    }

    #[tokio::test]
    async fn corrupt_covers_are_preserved_and_recreated() {
        let scratch = Scratch::new();
        let path = scratch.0.join("covers.sqlite3");
        let original = b"not a SQLite database: preserve this cover cache";
        std::fs::write(&path, original).unwrap();
        let covers = init_covers(&scratch.url("covers.sqlite3")).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_cover_blob")
            .fetch_one(&covers)
            .await
            .unwrap();
        assert_eq!(count, 0);
        covers.close().await;
        let preserved: Vec<_> = std::fs::read_dir(&scratch.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".quarantined-")
            })
            .collect();
        assert_eq!(preserved.len(), 1);
        assert_eq!(std::fs::read(&preserved[0]).unwrap(), original);
    }

    #[test]
    fn quarantine_preserves_database_and_sidecars_together() {
        let scratch = Scratch::new();
        let path = scratch.0.join("covers.sqlite3");
        for (suffix, bytes) in [("", b"main".as_slice()), ("-wal", b"wal"), ("-shm", b"shm")] {
            std::fs::write(format!("{}{suffix}", path.display()), bytes).unwrap();
        }
        let destination = quarantine_covers(&path).unwrap();
        for (suffix, bytes) in [("", b"main".as_slice()), ("-wal", b"wal"), ("-shm", b"shm")] {
            assert_eq!(
                std::fs::read(format!("{}{suffix}", destination.display())).unwrap(),
                bytes
            );
            assert!(!Path::new(&format!("{}{suffix}", path.display())).exists());
        }
    }

    #[tokio::test]
    async fn maintenance_never_recreates_corrupt_covers() {
        let scratch = Scratch::new();
        let path = scratch.0.join("covers.sqlite3");
        std::fs::write(&path, b"corrupt cache").unwrap();
        assert!(connect_existing_covers(&scratch.url("covers.sqlite3"))
            .await
            .is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"corrupt cache");
    }

    #[tokio::test]
    async fn partly_missing_covers_clear_only_dangling_pointers_across_chunks() {
        let scratch = Scratch::new();
        let main = crate::database::TestPoolOptions::new()
            .max_connections(2)
            .connect_with(
                SqliteConnectOptions::from_str(&scratch.url("main.sqlite3"))
                    .unwrap()
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal),
            )
            .await
            .unwrap();
        let covers = init_covers(&scratch.url("covers.sqlite3")).await.unwrap();
        for n in 0..600 {
            sqlx::query("INSERT INTO work (id,cover_cached_version,created_at,updated_at) VALUES ($1, $2, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')")
                .bind(format!("w_{n:04}"))
                .bind(if n == 599 { None } else { Some(7_i64) })
                .execute(&main)
                .await
                .unwrap();
        }
        for id in ["w_0000", "w_0300", "w_0598"] {
            sqlx::query("INSERT INTO work_cover_blob VALUES ($1, X'01', 7, 'now')")
                .bind(id)
                .execute(&covers)
                .await
                .unwrap();
        }
        assert_eq!(reconcile_cover_pointers(&main, &covers).await.unwrap(), 596);
        let remaining: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM work WHERE cover_cached_version IS NOT NULL ORDER BY id",
        )
        .fetch_all(&main)
        .await
        .unwrap();
        assert_eq!(remaining, ["w_0000", "w_0300", "w_0598"]);
        assert_eq!(reconcile_cover_pointers(&main, &covers).await.unwrap(), 0);
        main.close().await;
        covers.close().await;
    }
}

/// The retained SQLite source/cache tests use their own whole-unit busy retry.
/// Kept separate so PostgreSQL retry_transaction never accepts a SQLite code.
#[cfg(test)]
pub async fn retry_sqlite_transaction<T, F, Fut>(mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    use rand_core::RngCore;
    for retry in 0..5u32 {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) if retry < 4 && sqlx_cause(&error).is_some_and(sqlite_locked) => {
                let base = 25u64 << retry;
                let jitter = rand_core::OsRng.next_u64() % (base + 1);
                tokio::time::sleep(Duration::from_millis(base + jitter)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("last SQLite attempt returns")
}

#[cfg(test)]
mod retry_policy_tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_snapshot_retry_is_separate_from_postgres_retry() {
        let pool = SqliteDatabase::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE retry_example(value INTEGER); INSERT INTO retry_example VALUES(0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let external = SqliteDatabase::connect_with((*pool.connect_options()).clone(), 2)
            .await
            .unwrap();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let value = retry_sqlite_transaction(|| async {
            let attempt = attempts.fetch_add(1, Ordering::Relaxed);
            let mut tx = pool.begin_with("BEGIN").await?;
            let value: i64 = sqlx::query_scalar("SELECT value FROM retry_example")
                .fetch_one(&mut *tx)
                .await?;
            if attempt == 0 {
                sqlx::query("UPDATE retry_example SET value=1")
                    .execute(&external)
                    .await?;
            }
            sqlx::query("UPDATE retry_example SET value=2")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(value)
        })
        .await
        .unwrap();
        assert_eq!(value, 1);
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        external.close().await;
        pool.close().await;
    }

    // Exact counts below come from `test_counters::scope`: the process globals
    // are moved by every other test in this binary.
    #[tokio::test]
    async fn observed_retry_error_survives_context_without_response_recount() {
        let (error, seen) = test_counters::scope(async {
            let error = retry_transaction::<(), _, _>(|| async {
                Err(anyhow::Error::from(sqlx::Error::PoolTimedOut).context("acquiring writer"))
            })
            .await
            .unwrap_err()
            .context("request failed");
            observe_database_error(&error);
            error
        })
        .await;
        assert!(error.downcast_ref::<Observed>().is_some());
        assert!(sqlx_cause(&error).is_some_and(|e| matches!(e, sqlx::Error::PoolTimedOut)));
        assert_eq!(
            seen,
            test_counters::Counters {
                database: DatabaseCounters {
                    pool_timeouts: 1,
                    ..Default::default()
                },
                lock_errors: 1,
                write_lane_events: 1,
                read_lane_events: 0,
            }
        );
    }

    #[test]
    fn read_pool_failure_is_observed_without_write_gate_or_response_recount() {
        let (response, seen) = test_counters::scope_sync(|| {
            let error = observed_read_database_error(sqlx::Error::PoolTimedOut.into())
                .context("reader operation");
            crate::database_error_response(error, "read failed")
        });
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            seen,
            test_counters::Counters {
                database: DatabaseCounters {
                    pool_timeouts: 1,
                    ..Default::default()
                },
                lock_errors: 0,
                write_lane_events: 0,
                read_lane_events: 1,
            }
        );
    }

    #[test]
    fn write_io_loss_is_not_reported_as_idle_termination() {
        let ((), seen) = test_counters::scope_sync(|| {
            record_database_error(&sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed socket",
            )));
        });
        assert_eq!(
            seen.database,
            DatabaseCounters {
                connection_lost: 1,
                ..Default::default()
            }
        );
        assert_eq!(seen.lock_errors, 0);
    }

    #[test]
    fn read_io_loss_is_not_write_lane_connection_loss() {
        let ((), seen) = test_counters::scope_sync(|| {
            record_read_database_error(&sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed socket",
            )));
        });
        assert_eq!(seen, test_counters::Counters::default());
    }

    // The scope must not see other tasks (the reason exact global deltas were
    // false failures), must not see an unwrapped spawn, and must follow a sink
    // handed to a spawned task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_counters_ignore_other_tasks_and_follow_a_captured_sink() {
        let noise = tokio::spawn(async {
            for _ in 0..500 {
                record_database_error(&sqlx::Error::PoolTimedOut);
                tokio::task::yield_now().await;
            }
        });
        let global_before = database_counters().pool_timeouts;
        let ((), seen) = test_counters::scope(async {
            record_database_error(&sqlx::Error::PoolTimedOut);
            tokio::spawn(async { record_database_error(&sqlx::Error::PoolTimedOut) })
                .await
                .unwrap();
            let sink = test_counters::current();
            assert!(sink.is_some());
            tokio::spawn(test_counters::within(sink, async {
                record_read_database_error(&sqlx::Error::PoolTimedOut)
            }))
            .await
            .unwrap();
        })
        .await;
        noise.await.unwrap();
        assert!(test_counters::current().is_none());
        assert_eq!(
            seen,
            test_counters::Counters {
                database: DatabaseCounters {
                    pool_timeouts: 2,
                    ..Default::default()
                },
                lock_errors: 1,
                write_lane_events: 1,
                read_lane_events: 1,
            }
        );
        // The globals still receive every event: 500 noise + 3 from this test.
        assert!(database_counters().pool_timeouts >= global_before + 3);
    }

    #[derive(Debug)]
    struct HelperFailure(crate::locks::RestartTransaction);
    impl std::fmt::Display for HelperFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("helper failed")
        }
    }
    impl std::error::Error for HelperFailure {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[tokio::test]
    async fn restart_marker_is_recognised_through_the_source_chain_but_not_as_text() {
        for wrapped in [0u8, 1, 2] {
            let attempts = std::sync::atomic::AtomicUsize::new(0);
            let (result, seen) = test_counters::scope(retry_transaction(|| async {
                if attempts.fetch_add(1, Ordering::SeqCst) > 0 {
                    return Ok(7);
                }
                Err(match wrapped {
                    // A helper wrapped the marker in SQLx's own error type.
                    0 => anyhow::Error::from(sqlx::Error::Decode(Box::new(
                        crate::locks::RestartTransaction,
                    )))
                    .context("locking sources"),
                    // A custom error with the marker as its source.
                    1 => anyhow::Error::new(HelperFailure(crate::locks::RestartTransaction)),
                    // The documented direct form, under added context.
                    _ => anyhow::Error::from(crate::locks::RestartTransaction).context("guard"),
                })
            }))
            .await;
            assert_eq!(result.unwrap(), 7, "wrapping {wrapped}");
            assert_eq!(attempts.load(Ordering::SeqCst), 2, "wrapping {wrapped}");
            assert_eq!(
                seen,
                test_counters::Counters {
                    database: DatabaseCounters {
                        transaction_restarts: 1,
                        retries: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                "wrapping {wrapped}"
            );
        }
        // Text carries no marker: no retry, no restart count.
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let (result, seen) = test_counters::scope(retry_transaction::<(), _, _>(|| async {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!(crate::locks::RestartTransaction.to_string()))
        }))
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(seen, test_counters::Counters::default());
    }

    /// A database error with a chosen SQLSTATE, without a server.
    #[derive(Debug)]
    struct Sqlstate(&'static str);
    impl std::fmt::Display for Sqlstate {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "synthetic SQLSTATE {}", self.0)
        }
    }
    impl std::error::Error for Sqlstate {}
    impl sqlx::error::DatabaseError for Sqlstate {
        fn message(&self) -> &str {
            "synthetic database error"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(self.0.into())
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }
    fn sqlstate(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(Sqlstate(code)))
    }

    /// How an inner retry's final error reaches the enclosing retry.
    #[derive(Clone, Copy, Debug)]
    enum Between {
        Intact,
        MoreContext,
        SourceNode,
    }

    #[derive(Debug)]
    struct StepFailed(RetryExhausted);
    impl std::fmt::Display for StepFailed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("sync step failed")
        }
    }
    impl std::error::Error for StepFailed {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    // Third check, F3: the exhausted error kept its retryable cause, so an
    // enclosing retry ran the whole inner retry again (25 executions, five
    // exhaustion counts). The marker makes the first exhaustion final.
    #[tokio::test(start_paused = true)]
    async fn nested_retry_keeps_the_five_attempt_bound_and_counts_exhaustion_once() {
        for restart in [false, true] {
            for between in [Between::Intact, Between::MoreContext, Between::SourceNode] {
                let case = format!("restart={restart} between={between:?}");
                let units = std::sync::atomic::AtomicUsize::new(0);
                let outer = std::sync::atomic::AtomicUsize::new(0);
                let (result, seen) =
                    test_counters::scope(retry_transaction::<(), _, _>(|| async {
                        outer.fetch_add(1, Ordering::SeqCst);
                        let inner = retry_transaction::<(), _, _>(|| async {
                            units.fetch_add(1, Ordering::SeqCst);
                            Err(if restart {
                                anyhow::Error::from(crate::locks::RestartTransaction)
                            } else {
                                anyhow::Error::from(sqlstate("40001")).context("upsert work")
                            })
                        })
                        .await;
                        match between {
                            Between::Intact => inner,
                            Between::MoreContext => {
                                inner.map_err(|error| error.context("page").context("sync"))
                            }
                            // A helper that reports the exhaustion as its own error
                            // with the marker as `source()`.
                            Between::SourceNode => inner.map_err(|error| {
                                assert!(retry_exhausted(&error));
                                anyhow::Error::new(StepFailed(RetryExhausted)).context("sync")
                            }),
                        }
                    }))
                    .await;
                let error = result.unwrap_err();
                assert!(retry_exhausted(&error), "{case}");
                assert_eq!(units.load(Ordering::SeqCst), 5, "{case}");
                assert_eq!(outer.load(Ordering::SeqCst), 1, "{case}");
                let database = if restart {
                    DatabaseCounters {
                        transaction_restarts: 4,
                        retries: 4,
                        retries_exhausted: 1,
                        ..Default::default()
                    }
                } else {
                    DatabaseCounters {
                        serialization_failures: 5,
                        retries: 4,
                        retries_exhausted: 1,
                        ..Default::default()
                    }
                };
                assert_eq!(
                    seen,
                    test_counters::Counters {
                        database,
                        lock_errors: 1,
                        write_lane_events: if restart { 0 } else { 5 },
                        read_lane_events: 0,
                    },
                    "{case}"
                );
                // The retryable cause is still there for classification; only
                // the marker makes the error final.
                if matches!(between, Between::Intact | Between::MoreContext) {
                    assert_eq!(restart, restart_requested(&error), "{case}");
                    assert_eq!(
                        !restart,
                        sqlx_cause(&error).is_some_and(database_retryable),
                        "{case}"
                    );
                }
                if matches!(between, Between::Intact) {
                    assert!(error.to_string().contains("five attempts"), "{case}");
                }
            }
        }
        // Three levels deep: still five executions and one exhaustion.
        let units = std::sync::atomic::AtomicUsize::new(0);
        let (result, seen) = test_counters::scope(retry_transaction::<(), _, _>(|| async {
            retry_transaction::<(), _, _>(|| async {
                retry_transaction::<(), _, _>(|| async {
                    units.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::Error::from(sqlstate("40P01")))
                })
                .await
            })
            .await
        }))
        .await;
        assert!(result.is_err());
        assert_eq!(units.load(Ordering::SeqCst), 5);
        assert_eq!(seen.database.retries_exhausted, 1);
        assert_eq!(seen.database.retries, 4);
        // Text is not the marker: an error that only says so is treated by its
        // own class (here: no retryable cause, one attempt, nothing counted).
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let (result, seen) = test_counters::scope(retry_transaction::<(), _, _>(|| async {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err(anyhow::anyhow!(RetryExhausted.to_string()))
        }))
        .await;
        assert!(!retry_exhausted(&result.unwrap_err()));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(seen, test_counters::Counters::default());
    }

    // A retryable error that has NOT been through an exhausted retry is still
    // retried by the enclosing level (the marker must not be inferred).
    #[tokio::test(start_paused = true)]
    async fn an_error_without_the_exhaustion_marker_is_still_retried() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let (result, seen) = test_counters::scope(retry_transaction(|| async {
            if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                return Err(anyhow::Error::from(sqlstate("40001")).context("unit"));
            }
            Ok(3)
        }))
        .await;
        assert_eq!(result.unwrap(), 3);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(seen.database.retries, 2);
        assert_eq!(seen.database.retries_exhausted, 0);
        assert_eq!(seen.lock_errors, 0);
    }

    // The policy by class: lock timeout, statement timeout, idle termination,
    // constraint violation, connection loss and pool timeout get ONE attempt and
    // no wait; 40001 and 40P01 get five.
    #[tokio::test]
    async fn only_serialization_failure_deadlock_and_restart_are_retried() {
        type Class = (&'static str, fn() -> sqlx::Error, usize);
        let classes: [Class; 8] = [
            ("55P03", || sqlstate("55P03"), 1),
            ("57014", || sqlstate("57014"), 1),
            ("25P03", || sqlstate("25P03"), 1),
            ("23505", || sqlstate("23505"), 1),
            (
                "io",
                || {
                    sqlx::Error::Io(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "closed socket",
                    ))
                },
                1,
            ),
            ("pool", || sqlx::Error::PoolTimedOut, 1),
            ("40001", || sqlstate("40001"), 5),
            ("40P01", || sqlstate("40P01"), 5),
        ];
        for (name, make, expected) in classes {
            let attempts = std::sync::atomic::AtomicUsize::new(0);
            let waits = std::sync::atomic::AtomicUsize::new(0);
            let (result, seen) = test_counters::scope(retry_transaction_with(
                || async {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(anyhow::Error::from(make()).context("unit"))
                },
                |_| {
                    waits.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(())
                },
            ))
            .await;
            let error = result.unwrap_err();
            assert_eq!(attempts.load(Ordering::SeqCst), expected, "{name}");
            assert_eq!(waits.load(Ordering::SeqCst), expected - 1, "{name}");
            assert_eq!(seen.database.retries, expected as u64 - 1, "{name}");
            assert_eq!(seen.database.transaction_restarts, 0, "{name}");
            assert_eq!(
                seen.database.retries_exhausted,
                u64::from(expected == 5),
                "{name}"
            );
            assert_eq!(retry_exhausted(&error), expected == 5, "{name}");
        }
    }

    // Every delay the retry asks for, seen through the injected sleeper: one
    // between each pair of attempts, none after the last, each within
    // base..=2*base for base 25, 50, 100, 200 ms.
    #[tokio::test]
    async fn backoff_waits_between_attempts_within_the_jitter_bounds() {
        for round in 0..20 {
            let delays = std::sync::Mutex::new(Vec::new());
            let result = retry_transaction_with(
                || async { Err::<(), _>(anyhow::Error::from(sqlstate("40001"))) },
                |delay| {
                    delays.lock().unwrap().push(delay);
                    std::future::ready(())
                },
            )
            .await;
            assert!(result.is_err());
            let delays = delays.into_inner().unwrap();
            assert_eq!(delays.len(), 4, "round {round}: {delays:?}");
            for (retry, delay) in delays.iter().enumerate() {
                let base = Duration::from_millis(25 << retry);
                assert!(
                    *delay >= base && *delay <= base * 2,
                    "round {round}: delay {retry} is {delay:?}"
                );
            }
        }
    }

    // The public function really waits (Tokio's paused clock: the elapsed time
    // is exactly the sum of the sleeps, whatever the machine load).
    #[tokio::test(start_paused = true)]
    async fn public_retry_sleeps_between_attempts() {
        let started = tokio::time::Instant::now();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let value = retry_transaction(|| async {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(anyhow::Error::from(sqlstate("40P01")));
            }
            Ok(1)
        })
        .await
        .unwrap();
        assert_eq!(value, 1);
        let one = started.elapsed();
        assert!(
            one >= Duration::from_millis(25) && one <= Duration::from_millis(50),
            "{one:?}"
        );
        let started = tokio::time::Instant::now();
        let result =
            retry_transaction::<(), _, _>(|| async { Err(anyhow::Error::from(sqlstate("40001"))) })
                .await;
        assert!(result.is_err());
        let all = started.elapsed();
        assert!(
            all >= Duration::from_millis(375) && all <= Duration::from_millis(750),
            "{all:?}"
        );
    }

    // The counting contract for boundaries (design record, rule 4).
    #[test]
    fn terminal_observation_and_marked_propagation_each_count_once() {
        let write_once = test_counters::Counters {
            database: DatabaseCounters {
                pool_timeouts: 1,
                ..Default::default()
            },
            lock_errors: 1,
            write_lane_events: 1,
            read_lane_events: 0,
        };
        // Terminal swallow site: borrowed observation, then the error is dropped.
        let ((), seen) = test_counters::scope_sync(|| {
            let error = anyhow::Error::from(sqlx::Error::PoolTimedOut).context("job step");
            observe_database_error(&error);
        });
        assert_eq!(seen, write_once);
        // Propagating boundary: mark, return the marked error intact; every later
        // boundary (more context, borrowed observation, REST response) skips it.
        let (response, seen) = test_counters::scope_sync(|| {
            let error = observed_database_error(sqlx::Error::PoolTimedOut.into());
            let error = error.context("handler");
            observe_database_error(&error);
            crate::database_error_response(error, "busy")
        });
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(seen, write_once);
        // Observe-then-respond, the documented way: the response boundary is the
        // one observation.
        let (_, seen) = test_counters::scope_sync(|| {
            crate::database_error_response(anyhow::Error::from(sqlx::Error::PoolTimedOut), "busy")
        });
        assert_eq!(seen, write_once);
        // The forbidden order. A borrowed error cannot be marked, so observing it
        // and then handing it to another boundary counts twice. This pins the
        // limitation the rule is written around; if it ever counts once, relax
        // the rule in the design record.
        let (_, seen) = test_counters::scope_sync(|| {
            let error = anyhow::Error::from(sqlx::Error::PoolTimedOut);
            observe_database_error(&error);
            crate::database_error_response(error, "busy")
        });
        assert_eq!(seen.database.pool_timeouts, 2);
        // Text keeps neither marker nor class: nothing can be counted afterwards.
        let ((), seen) = test_counters::scope_sync(|| {
            let text = anyhow::Error::from(sqlx::Error::PoolTimedOut).to_string();
            observe_database_error(&anyhow::anyhow!(text));
        });
        assert_eq!(seen, test_counters::Counters::default());
    }

    #[tokio::test]
    async fn standalone_snapshot_budget_is_eight_and_releases_on_drop() {
        let mut held = Vec::new();
        for _ in 0..8 {
            held.push(snapshot_session_permit().await.unwrap());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(5), snapshot_session_permit())
                .await
                .is_err()
        );
        drop(held.pop());
        let ninth = tokio::time::timeout(Duration::from_secs(1), snapshot_session_permit())
            .await
            .unwrap()
            .unwrap();
        drop(ninth);
        drop(held);
    }

    #[test]
    fn prometheus_exposes_every_postgres_counter_class() {
        let metrics = DatabaseCounters::default().prometheus();
        for name in [
            "lock_timeouts",
            "statement_timeouts",
            "deadlocks",
            "serialization_failures",
            "idle_transaction_terminations",
            "connection_lost",
            "transaction_restarts",
            "pool_acquire_timeouts",
            "transaction_retries",
            "transaction_retries_exhausted",
        ] {
            assert!(metrics.contains(&format!("komika_postgres_{name}_total 0")));
        }
    }
}

/// Keeps the counting boundary enforceable. A database error that is turned into
/// text inside `map_err` loses its SQLSTATE and its `Observed` marker, so no later
/// boundary can count it. This scan reads the server sources at test time and
/// fails on that one narrow form; it does not try to find every swallowed error
/// (a `match`/`if let Err` that only logs is still a review matter).
#[cfg(test)]
mod observation_scan_tests {
    use std::path::{Path, PathBuf};

    /// The receiver of `.map_err(` is a database call when its statement contains
    /// one of these tokens: a direct SQLx query/transaction call, or the
    /// instance-setting writer (directly or through the Cloudflare `store` closure).
    const DATABASE_CALLS: &[&str] = &[
        "sqlx::query",
        "sqlx::raw_sql",
        "query_tuple(",
        ".fetch_one(",
        ".fetch_all(",
        ".fetch_optional(",
        ".execute(",
        ".begin(",
        ".begin_with(",
        ".commit(",
        "instance::store(",
        "store(&instance::",
    ];
    /// The argument produces text when it holds a string literal, `.to_string()`
    /// or `format!`.
    const TEXT: &[&str] = &["\"", ".to_string()", "format!("];
    /// It is fine when the same argument observes the error, hands it to an
    /// observing converter, or keeps it typed under added context.
    const OBSERVED: &[&str] = &[
        "record_",
        "observe",
        "gql_err",
        "gql_read_err",
        "database_error_response",
        ".context(",
        ".with_context(",
    ];

    /// (file under src/, text inside the flagged argument, reason). An entry that
    /// matches nothing fails the test, so a fixed site is removed from this list.
    /// `TODO(wave)`: an unobserved site in another owner's file, to be fixed there
    /// (observe before the conversion: `record_database_error(&e)` for SQLx,
    /// `observe_database_error(&e)` for anyhow) and then deleted here.
    const ALLOWED: &[(&str, &str, &str)] = &[
        (
            "cloudflare/mod.rs",
            "saving completed Cloudflare state failed",
            "accepted: the receiver is db::retry_transaction, which counts every attempt \
             and marks the error before this closure writes the user text",
        ),
        (
            "pg_import/startup.rs",
            "SQLite cannot write temporary files in the work directory",
            "accepted: a PRAGMA on the importer's private in-memory SQLite connection, \
             not a PostgreSQL error; no PostgreSQL counter applies",
        ),
    ];

    /// Same length as the input. Comments and the contents of string/char
    /// literals become spaces (newlines stay), so tokens and braces found later
    /// are real code and byte offsets still point into the original text.
    fn blank_comments_and_literals(source: &str) -> String {
        let bytes = source.as_bytes();
        let mut out = bytes.to_vec();
        let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
            for byte in &mut out[from..to.min(bytes.len())] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        };
        let identifier = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            if rest.starts_with(b"//") {
                let end = rest
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(bytes.len(), |n| i + n);
                blank(&mut out, i, end);
                i = end;
            } else if rest.starts_with(b"/*") {
                let (mut depth, mut end) = (1, i + 2);
                while end < bytes.len() && depth > 0 {
                    if bytes[end..].starts_with(b"/*") {
                        depth += 1;
                        end += 2;
                    } else if bytes[end..].starts_with(b"*/") {
                        depth -= 1;
                        end += 2;
                    } else {
                        end += 1;
                    }
                }
                blank(&mut out, i, end);
                i = end;
            } else if (rest[0] == b'r' || rest.starts_with(b"br"))
                && (i == 0 || !identifier(bytes[i - 1]))
                && {
                    let hashes = i + if rest[0] == b'b' { 2 } else { 1 };
                    let quote = hashes
                        + bytes[hashes..]
                            .iter()
                            .take_while(|byte| **byte == b'#')
                            .count();
                    bytes.get(quote) == Some(&b'"')
                }
            {
                let hashes = i + if rest[0] == b'b' { 2 } else { 1 };
                let count = bytes[hashes..]
                    .iter()
                    .take_while(|byte| **byte == b'#')
                    .count();
                let open = hashes + count;
                let mut close = vec![b'"'];
                close.extend(vec![b'#'; count]);
                let end = bytes[open + 1..]
                    .windows(close.len())
                    .position(|window| window == close.as_slice())
                    .map_or(bytes.len(), |n| open + 1 + n);
                blank(&mut out, open + 1, end);
                i = (end + close.len()).min(bytes.len());
            } else if rest[0] == b'"' {
                let mut end = i + 1;
                while end < bytes.len() && bytes[end] != b'"' {
                    end += if bytes[end] == b'\\' { 2 } else { 1 };
                }
                blank(&mut out, i + 1, end);
                i = (end + 1).min(bytes.len());
            } else if rest[0] == b'\'' {
                // A char literal, not a lifetime: an escape, or one character
                // followed by the closing quote.
                let end = if rest.get(1) == Some(&b'\\') {
                    // Skip the escaped character itself (it may be a quote).
                    rest.get(3..)
                        .and_then(|tail| tail.iter().position(|b| *b == b'\''))
                        .map(|n| i + 3 + n + 1)
                } else {
                    source[i + 1..]
                        .chars()
                        .next()
                        .map(|c| i + 1 + c.len_utf8())
                        .filter(|close| bytes.get(*close) == Some(&b'\''))
                        .map(|close| close + 1)
                };
                match end {
                    Some(end) => {
                        blank(&mut out, i + 1, end - 1);
                        i = end;
                    }
                    None => i += 1,
                }
            } else {
                i += 1;
            }
        }
        String::from_utf8(out).expect("only whole literal and comment bodies were blanked")
    }

    fn matching(code: &[u8], open: usize, opener: u8, closer: u8) -> usize {
        let mut depth = 0usize;
        for (offset, byte) in code[open..].iter().enumerate() {
            if *byte == opener {
                depth += 1;
            } else if *byte == closer {
                depth -= 1;
                if depth == 0 {
                    return open + offset;
                }
            }
        }
        code.len() - 1
    }

    /// Blank every item or block guarded by `#[cfg(test)]`.
    fn blank_test_code(code: &mut [u8]) {
        let attribute = b"#[cfg(test)]";
        let mut from = 0;
        while let Some(found) = code[from..]
            .windows(attribute.len())
            .position(|window| window == attribute)
        {
            let mut i = from + found + attribute.len();
            loop {
                while code.get(i).is_some_and(u8::is_ascii_whitespace) {
                    i += 1;
                }
                if code[i..].starts_with(b"#[") {
                    i = matching(code, i + 1, b'[', b']') + 1;
                } else {
                    break;
                }
            }
            while i < code.len() && code[i] != b'{' && code[i] != b';' {
                i += 1;
            }
            if code.get(i) == Some(&b'{') {
                let end = matching(code, i, b'{', b'}');
                for byte in &mut code[i..=end] {
                    if *byte != b'\n' {
                        *byte = b' ';
                    }
                }
                i = end;
            }
            from = i.min(code.len());
        }
    }

    /// Start of the statement that ends at `end`: back to a `;` or `,`, an unmatched
    /// opening bracket, or a closed block that the expression does not continue.
    fn statement_start(code: &[u8], end: usize) -> usize {
        let (mut i, mut depth) = (end, 0usize);
        while i > 0 {
            match code[i - 1] {
                b')' | b']' => depth += 1,
                b'}' => {
                    if depth == 0 {
                        let next = code[i..end]
                            .iter()
                            .find(|byte| !byte.is_ascii_whitespace())
                            .copied();
                        // `}` ends the previous statement unless a method call or
                        // `?` continues it (`match x { .. }.map_err`, `async {}.await`).
                        if !matches!(next, None | Some(b'.' | b'?')) {
                            break;
                        }
                    }
                    depth += 1;
                }
                b'(' | b'[' | b'{' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                // A comma outside brackets separates match arms, fields or
                // elements: the receiver starts after it.
                b';' | b',' if depth == 0 => break,
                _ => {}
            }
            i -= 1;
        }
        i
    }

    /// (line, original argument text) of every flagged `.map_err(` in one file.
    fn unobserved_text_conversions(source: &str) -> Vec<(usize, String)> {
        let mut code = blank_comments_and_literals(source).into_bytes();
        blank_test_code(&mut code);
        let needle = b".map_err(";
        let mut flagged = Vec::new();
        let mut from = 0;
        while let Some(found) = code[from..]
            .windows(needle.len())
            .position(|window| window == needle)
        {
            let call = from + found;
            let open = call + needle.len() - 1;
            let close = matching(&code, open, b'(', b')');
            from = open + 1;
            let argument = String::from_utf8_lossy(&code[open + 1..close]);
            let receiver = String::from_utf8_lossy(&code[statement_start(&code, call)..call]);
            if DATABASE_CALLS.iter().any(|token| receiver.contains(token))
                && TEXT.iter().any(|token| argument.contains(token))
                && !OBSERVED.iter().any(|token| argument.contains(token))
            {
                let line = source[..call].matches('\n').count() + 1;
                let original = source[open + 1..close]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                flagged.push((line, original));
            }
        }
        flagged
    }

    fn production_sources(directory: &Path, found: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            // Test-only modules: `*tests.rs`, `test_*.rs` and `tests/` directories.
            if stem.ends_with("tests") || stem.starts_with("test_") {
                continue;
            }
            if path.is_dir() {
                production_sources(&path, found);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }

    #[test]
    fn scanner_flags_only_unobserved_database_text_conversions() {
        let lines = |source: &str| {
            unobserved_text_conversions(source)
                .into_iter()
                .map(|(line, _)| line)
                .collect::<Vec<_>>()
        };
        // The Cloudflare form the second check found, and its two plain cousins.
        assert_eq!(
            lines(
                "async fn save(pool: &Database) -> Result<(), String> {\n\
                 instance::store(pool, &KEY, &raw).await.map_err(|e| {\n\
                 tracing::error!(error = %e, \"saving failed\");\n\
                 \"The server could not save.\".to_string()\n})\n}\n\
                 async fn a(p: &Database) -> Result<(), String> {\n\
                 sqlx::query(\"UPDATE t SET v=1\").execute(p).await.map_err(|e| e.to_string())?;\n\
                 let n = sqlx::query_scalar(\"SELECT 1\").fetch_one(p).await\n\
                 .map_err(|e| format!(\"failed: {e}\"))?;\nOk(())\n}\n"
            ),
            [2, 8, 10]
        );
        // Observed in the same closure, an observing converter, or kept typed.
        assert!(lines(
            "async fn a(p: &Database) -> Result<(), String> {\n\
             sqlx::query(\"x\").execute(p).await.map_err(|e| {\n\
             crate::db::record_database_error(&e);\n\"busy\".to_string()\n})?;\n\
             sqlx::query(\"x\").execute(p).await.map_err(gql_err)?;\n\
             sqlx::query(\"x\").execute(p).await.map_err(|e| {\n\
             if code(&e) == Some(\"23505\") { Error::new(\"taken\") } else { gql_err(e) }\n})?;\n\
             sqlx::query(\"x\").execute(p).await\n\
             .map_err(|e| anyhow::Error::from(e).context(\"saving\"))?;\n\
             sqlx::query(\"x\").execute(p).await.map_err(Into::into)\n}\n"
        )
        .is_empty());
        // Not a database receiver, even right after a block that used SQLx; and
        // tokens inside comments, literals and test code do not count.
        assert!(lines(
            "async fn a(p: &Database, s: &S) -> Result<String, String> {\n\
             if s.fresh { sqlx::query(\"x\").execute(p).await.ok(); }\n\
             let raw = serde_json::to_string(s).map_err(|e| e.to_string())?;\n\
             // sqlx::query(\"x\").execute(p).await.map_err(|e| e.to_string())\n\
             let text = \"sqlx::query(x).execute(p).await.map_err(|e| e.to_string())\";\n\
             let lifetime: &'static str = text; let quote = '\"'; let brace = '{';\n\
             Ok(raw)\n}\n\
             #[cfg(test)]\nmod tests {\n    async fn t(p: &Database) {\n\
             sqlx::query(\"x\").execute(p).await.map_err(|e| e.to_string()).unwrap();\n    }\n}\n"
        )
        .is_empty());
        // A statement continued after a block still belongs to its receiver.
        assert_eq!(
            lines(
                "async fn a(p: &Database) -> Result<(), String> {\n\
                 async { sqlx::query(\"x\").execute(p).await }\n.await\n\
                 .map_err(|_| \"Could not save.\".to_string())?;\nOk(())\n}\n"
            ),
            [4]
        );
    }

    #[test]
    fn database_errors_are_not_turned_into_text_unobserved() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        production_sources(&root, &mut files);
        assert!(
            files.len() > 20,
            "source scan found too few files under {root:?}"
        );
        let mut used = vec![false; ALLOWED.len()];
        let mut unexpected = Vec::new();
        for path in files {
            let file = path
                .strip_prefix(&root)
                .unwrap()
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let source = std::fs::read_to_string(&path).unwrap();
            for (line, argument) in unobserved_text_conversions(&source) {
                let allowed = ALLOWED
                    .iter()
                    .position(|(allowed, text, _)| *allowed == file && argument.contains(text));
                match allowed {
                    Some(entry) => used[entry] = true,
                    None => unexpected.push(format!("src/{file}:{line}: .map_err({argument})")),
                }
            }
        }
        assert!(
            unexpected.is_empty(),
            "a database error is turned into text without being observed. Call \
             db::record_database_error(&e) (SQLx) or db::observe_database_error(&e) (anyhow) in \
             the same closure, or return the typed error through gql_err/gql_read_err/\
             database_error_response:\n{}",
            unexpected.join("\n")
        );
        let stale: Vec<_> = ALLOWED
            .iter()
            .zip(&used)
            .filter(|(_, used)| !**used)
            .map(|((file, text, _), _)| format!("src/{file}: {text:?}"))
            .collect();
        assert!(
            stale.is_empty(),
            "these allowlist entries match no site any more; delete them from \
             db::observation_scan_tests::ALLOWED:\n{}",
            stale.join("\n")
        );
    }
}

#[cfg(test)]
mod rest_phase4_tests {
    use super::*;

    /// Boot must not queue behind (or open a transaction for) a work whose blob is
    /// present: only a pointer whose blob is missing takes the asset guard.
    #[tokio::test]
    async fn cover_pointer_repair_takes_the_asset_guard_only_for_missing_blobs() {
        let main = Database::connect("sqlite::memory:")
            .await
            .unwrap()
            .foreground();
        let covers = crate::sqlite_database::SqliteTestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE work_cover_blob(work_id TEXT PRIMARY KEY, webp BLOB NOT NULL,version INTEGER NOT NULL,updated_at TEXT NOT NULL)").execute(&covers).await.unwrap();
        sqlx::query("INSERT INTO work(id,cover_cached_version,created_at,updated_at) VALUES ('w_present',7,now(),now()),('w_missing',9,now(),now())").execute(&main).await.unwrap();
        sqlx::query(
            "INSERT INTO work_cover_blob VALUES ('w_present',x'00',7,'2026-01-01T00:00:00Z')",
        )
        .execute(&covers)
        .await
        .unwrap();
        // Another session is in the middle of a publication for the present asset.
        let mut busy = main.begin().await.unwrap();
        crate::locks::advisory(
            &mut busy,
            &[crate::locks::AdvisoryKey::new(
                crate::locks::KeyClass::Cover,
                &["w_present"],
            )],
        )
        .await
        .unwrap();
        let cleared = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            reconcile_cover_pointers(&main, &covers),
        )
        .await
        .expect("the repair waited on the guard of an asset whose blob is present")
        .unwrap();
        assert_eq!(cleared, 1);
        busy.rollback().await.unwrap();
        let pointers: Vec<(String, Option<i64>)> =
            sqlx::query_as("SELECT id, cover_cached_version FROM work ORDER BY id")
                .fetch_all(&main)
                .await
                .unwrap();
        assert_eq!(
            pointers,
            [
                ("w_missing".to_owned(), None),
                ("w_present".to_owned(), Some(7))
            ]
        );
    }

    #[tokio::test]
    async fn cover_pointer_repair_waits_for_the_asset_publication_guard() {
        for reject_pointer in [false, true] {
            let main = Database::connect("sqlite::memory:")
                .await
                .unwrap()
                .foreground();
            let covers = crate::sqlite_database::SqliteTestPoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .unwrap();
            for ddl in ["CREATE TABLE work_cover_blob(work_id TEXT PRIMARY KEY, webp BLOB NOT NULL,version INTEGER NOT NULL,updated_at TEXT NOT NULL)","CREATE TABLE cover_variant(asset_key TEXT NOT NULL,width INTEGER NOT NULL,webp BLOB NOT NULL,updated_at TEXT NOT NULL,PRIMARY KEY(asset_key,width))"] { sqlx::query(ddl).execute(&covers).await.unwrap(); }
            sqlx::query("INSERT INTO work(id,cover_cached_version,created_at,updated_at) VALUES ('w_repair_overlap',7,now(),now())").execute(&main).await.unwrap();
            if reject_pointer {
                sqlx::raw_sql("CREATE SEQUENCE rest_reject_once; CREATE FUNCTION rest_reject_cover_pointer() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.cover_cached_version IS DISTINCT FROM OLD.cover_cached_version AND NEW.cover_cached_version IS NOT NULL AND nextval('rest_reject_once') = 1 THEN RAISE EXCEPTION 'pointer publication deliberately rejected'; END IF; RETURN NEW; END $$; CREATE TRIGGER rest_reject_cover_pointer BEFORE UPDATE ON work FOR EACH ROW EXECUTE FUNCTION rest_reject_cover_pointer()")
                .execute(&main).await.unwrap();
            }
            let points = crate::locks::pause_points::PausePoints::default();
            let armed = points.arm("cover-locked");
            let writer = main.clone();
            let cache = covers.clone();
            let first = tokio::spawn(crate::cover::COVER_PUBLICATION_PAUSE.scope(
                points,
                async move {
                    crate::cover::put_work_cover(&writer, &cache, "w_repair_overlap", b"fresh")
                        .await
                },
            ));
            tokio::time::timeout(std::time::Duration::from_secs(30), armed.reached())
                .await
                .unwrap();
            let writer = main.clone();
            let cache = covers.clone();
            let second =
                tokio::spawn(async move { reconcile_cover_pointers(&writer, &cache).await });
            crate::cover::wait_for_writer_lock(&main).await;
            armed.release();
            let saved = first.await.unwrap();
            if reject_pointer {
                let error = saved.unwrap_err();
                assert_eq!(
                    error
                        .downcast_ref::<sqlx::Error>()
                        .and_then(sqlx::Error::as_database_error)
                        .and_then(|e| e.code())
                        .as_deref(),
                    Some("P0001")
                );
            } else {
                saved.unwrap();
            }

            // A sequence is not rolled back, so the trigger rejects the publication's
            // pointer write only: the blob is stored, the pointer still names the old
            // version. The repair that waited must move the pointer to the stored blob.
            assert_eq!(second.await.unwrap().unwrap(), u64::from(reject_pointer));
            let version: i64 = sqlx::query_scalar(
                "SELECT version FROM work_cover_blob WHERE work_id='w_repair_overlap'",
            )
            .fetch_one(&covers)
            .await
            .unwrap();
            let pointer: i64 = sqlx::query_scalar(
                "SELECT cover_cached_version FROM work WHERE id='w_repair_overlap'",
            )
            .fetch_one(&main)
            .await
            .unwrap();
            // Blob-first recovery must never clear an existing pointer after a failed
            // pointer write (a repair that read "missing" before waiting would), and
            // it must not leave the pointer on the replaced version either.
            assert_ne!(version, 7);
            assert_eq!(pointer, version);
        }
    }

    /// The state a failed pointer write leaves behind for a LATER run: the stored blob
    /// is newer than a non-NULL pointer. The drainer never looks at it (it selects
    /// NULL pointers), so the boot repair has to move the pointer and the card URLs.
    #[tokio::test]
    async fn a_stale_cover_pointer_is_moved_to_the_stored_blob_version() {
        let main = Database::connect("sqlite::memory:")
            .await
            .unwrap()
            .foreground();
        let covers = crate::sqlite_database::SqliteTestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE work_cover_blob(work_id TEXT PRIMARY KEY, webp BLOB NOT NULL,version INTEGER NOT NULL,updated_at TEXT NOT NULL)").execute(&covers).await.unwrap();
        sqlx::query("INSERT INTO work(id,cover_cached_version,created_at,updated_at) VALUES ('w_current',7,now(),now()),('w_gone',7,now(),now()),('w_stale',7,now(),now())").execute(&main).await.unwrap();
        sqlx::query(
            "INSERT INTO work_cover_blob VALUES ('w_current',x'00',7,'t'),('w_stale',x'01',9,'t')",
        )
        .execute(&covers)
        .await
        .unwrap();
        for work in ["w_current", "w_gone", "w_stale"] {
            let url = crate::cover::cover_path(work, 7);
            sqlx::query("INSERT INTO feed_series_updates (work_id, reader_id, title, released_at, cover_url) VALUES ($1, $1, 'Cover', now(), $2)")
                .bind(work).bind(&url).execute(&main).await.unwrap();
            sqlx::query("INSERT INTO browse_catalogue (work_id, reader_id, title, created_at, cover_url) VALUES ($1, $1, 'Cover', now(), $2)")
                .bind(work).bind(&url).execute(&main).await.unwrap();
        }

        assert_eq!(reconcile_cover_pointers(&main, &covers).await.unwrap(), 2);
        let state = || async {
            sqlx::query_as::<_, (String, Option<i64>, Option<String>, Option<String>)>(
                "SELECT w.id, w.cover_cached_version, f.cover_url, b.cover_url FROM work w \
                 JOIN feed_series_updates f ON f.work_id = w.id \
                 JOIN browse_catalogue b ON b.work_id = w.id ORDER BY w.id",
            )
            .fetch_all(&main)
            .await
            .unwrap()
        };
        let old = |work: &str| Some(crate::cover::cover_path(work, 7));
        let moved = Some(crate::cover::cover_path("w_stale", 9));
        let want = vec![
            (
                "w_current".to_owned(),
                Some(7),
                old("w_current"),
                old("w_current"),
            ),
            ("w_gone".to_owned(), None, old("w_gone"), old("w_gone")),
            ("w_stale".to_owned(), Some(9), moved.clone(), moved),
        ];
        assert_eq!(state().await, want);
        assert_eq!(reconcile_cover_pointers(&main, &covers).await.unwrap(), 0);
        assert_eq!(state().await, want);
    }

    /// A batch of suspects is one transaction: one advisory call, one `lock_works`.
    /// With a transaction per pointer, the first pointers would already be cleared
    /// while a later one of the same batch still waits for its work.
    #[tokio::test]
    async fn cover_pointer_repair_commits_a_batch_of_pointers_together() {
        let main = Database::connect("sqlite::memory:")
            .await
            .unwrap()
            .foreground();
        let covers = crate::sqlite_database::SqliteTestPoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE work_cover_blob(work_id TEXT PRIMARY KEY, webp BLOB NOT NULL,version INTEGER NOT NULL,updated_at TEXT NOT NULL)").execute(&covers).await.unwrap();
        sqlx::query("INSERT INTO work(id,cover_cached_version,created_at,updated_at) VALUES ('w_a',7,now(),now()),('w_b',7,now(),now()),('w_c',7,now(),now())").execute(&main).await.unwrap();
        let mut holder = main.begin().await.unwrap();
        crate::locks::lock_works(&mut holder, &["w_c"])
            .await
            .unwrap();
        let (writer, cache) = (main.clone(), covers.clone());
        let repair = tokio::spawn(async move { reconcile_cover_pointers(&writer, &cache).await });
        crate::cover::wait_for_writer_lock(&main).await;
        let pending: i64 =
            sqlx::query_scalar("SELECT count(*) FROM work WHERE cover_cached_version IS NOT NULL")
                .fetch_one(&main)
                .await
                .unwrap();
        assert_eq!(
            pending, 3,
            "no pointer of the batch is published before the batch commits"
        );
        holder.rollback().await.unwrap();
        assert_eq!(repair.await.unwrap().unwrap(), 3);
    }
}
