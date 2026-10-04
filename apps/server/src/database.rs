//! SQLite access lanes: reads cannot occupy the single application writer.
#[cfg(test)]
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{FutureExt, TryStreamExt};
use sqlx::sqlite::{
    SqliteConnectOptions, SqlitePoolOptions, SqliteQueryResult, SqliteRow, SqliteTypeInfo,
};
use sqlx::{Describe, Either, Error, Execute, Executor, Sqlite, SqlitePool, Transaction};

#[derive(Clone, Debug)]
pub struct Database {
    readers: SqlitePool,
    writer: SqlitePool,
    deferred: Arc<DeferredWrites>,
    // Test-only ephemeral files let the normal test suite exercise WAL isolation too.
    _temporary: Option<Arc<TemporaryDatabase>>,
}

type DeferredWrite = BoxFuture<'static, anyhow::Result<()>>;

#[derive(Debug)]
struct DeferredWrites {
    sender: tokio::sync::mpsc::Sender<DeferredWrite>,
    pending: Arc<std::sync::atomic::AtomicUsize>,
    drained: Arc<tokio::sync::Notify>,
    accepting: std::sync::atomic::AtomicBool,
}

#[derive(Debug)]
struct TemporaryDatabase(std::path::PathBuf);

impl Drop for TemporaryDatabase {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.0.as_os_str().to_owned();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Database {
    pub async fn connect_with(
        mut options: SqliteConnectOptions,
        read_connections: u32,
    ) -> Result<Self, Error> {
        let temporary = if options.get_filename() == std::path::Path::new(":memory:")
            || options
                .get_filename()
                .to_string_lossy()
                .starts_with("file:sqlx-in-memory-")
        {
            let path =
                std::env::temp_dir().join(format!("komiq-test-{}.sqlite3", uuid::Uuid::new_v4()));
            options = options
                .filename(&path)
                .in_memory(false)
                .shared_cache(false)
                .create_if_missing(true);
            Some(Arc::new(TemporaryDatabase(path)))
        } else {
            None
        };
        options = options
            .pragma("query_only", "OFF")
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_millis(250));
        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options.clone())
            .await?;
        let readers = SqlitePoolOptions::new()
            .max_connections(read_connections.max(1))
            .acquire_timeout(Duration::from_secs(3))
            .connect_with(options.pragma("query_only", "ON"))
            .await;
        let readers = match readers {
            Ok(readers) => readers,
            Err(error) => {
                writer.close().await;
                return Err(error);
            }
        };
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<DeferredWrite>(64);
        let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let drained = Arc::new(tokio::sync::Notify::new());
        let worker_pending = pending.clone();
        let worker_drained = drained.clone();
        tokio::spawn(async move {
            while let Some(job) = receiver.recv().await {
                match std::panic::AssertUnwindSafe(job).catch_unwind().await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "deferred cache write failed; a later miss will retry")
                    }
                    Err(_) => {
                        tracing::error!("deferred cache write panicked; a later miss will retry")
                    }
                }
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
            deferred,
            _temporary: temporary,
        })
    }

    #[cfg(test)]
    pub async fn connect(url: &str) -> Result<Self, Error> {
        Self::connect_with(SqliteConnectOptions::from_str(url)?, 4).await
    }

    pub fn read(&self) -> &SqlitePool {
        &self.readers
    }
    /// Explicit lane for statements returning rows while changing data (RETURNING),
    /// schema maintenance, and helpers which need a raw SQLx pool.
    pub fn write(&self) -> &SqlitePool {
        &self.writer
    }
    pub fn connect_options(&self) -> Arc<SqliteConnectOptions> {
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
        if !self.deferred.accepting.load(Ordering::Acquire) {
            return false;
        }
        self.deferred.pending.fetch_add(1, Ordering::AcqRel);
        if !self.deferred.accepting.load(Ordering::Acquire)
            || self.deferred.sender.try_send(Box::pin(job)).is_err()
        {
            self.deferred.pending.fetch_sub(1, Ordering::AcqRel);
            self.deferred.drained.notify_waiters();
            return false;
        }
        true
    }
    pub async fn acquire(&self) -> Result<sqlx::pool::PoolConnection<Sqlite>, Error> {
        self.readers.acquire().await
    }
    pub async fn begin(&self) -> Result<Transaction<'static, Sqlite>, Error> {
        self.begin_with("BEGIN IMMEDIATE").await
    }
    pub async fn begin_with(
        &self,
        statement: &'static str,
    ) -> Result<Transaction<'static, Sqlite>, Error> {
        let started = Instant::now();
        let result = self.writer.begin_with(statement).await;
        crate::db::record_write_wait(started.elapsed());
        if let Err(error) = &result {
            crate::db::record_sqlite_error(error);
        }
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
        futures::join!(self.readers.close(), self.writer.close());
    }
}

/// Query methods that fetch rows default to the query-only lane. An accidental
/// INSERT/UPDATE/DELETE RETURNING fails instead of taking the writer unnoticed.
/// `execute` is the common, centralized path for statements without result rows.
impl<'c> Executor<'c> for &'c Database {
    type Database = Sqlite;

    fn execute<'e, 'q: 'e, E>(self, query: E) -> BoxFuture<'e, Result<SqliteQueryResult, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Sqlite>,
    {
        Box::pin(async move {
            let started = Instant::now();
            let acquired = self.writer.acquire().await;
            crate::db::record_write_wait(started.elapsed());
            let mut connection = acquired?;
            let result = (&mut *connection).execute(query).await;
            if let Err(error) = &result {
                crate::db::record_sqlite_error(error);
            }
            result
        })
    }

    fn execute_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<'e, Result<SqliteQueryResult, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Sqlite>,
    {
        Box::pin(
            self.writer
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
    ) -> BoxStream<'e, Result<Either<SqliteQueryResult, SqliteRow>, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Sqlite>,
    {
        self.readers.fetch_many(query)
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<Option<SqliteRow>, Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Sqlite>,
    {
        self.readers.fetch_optional(query)
    }

    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        parameters: &'e [SqliteTypeInfo],
    ) -> BoxFuture<'e, Result<sqlx::sqlite::SqliteStatement<'q>, Error>>
    where
        'c: 'e,
    {
        self.readers.prepare_with(sql, parameters)
    }

    fn describe<'e, 'q: 'e>(self, sql: &'q str) -> BoxFuture<'e, Result<Describe<Sqlite>, Error>>
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
        self.connect_with(SqliteConnectOptions::from_str(url)?)
            .await
    }
    pub async fn connect_with(self, options: SqliteConnectOptions) -> Result<Database, Error> {
        Database::connect_with(options, self.read_connections).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wal_readers_do_not_wait_for_writer_and_reject_writes() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE example (value INTEGER)")
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
        assert_eq!(count, 0, "read sees the last committed WAL snapshot");
        let error = sqlx::query("INSERT INTO example VALUES (2)")
            .execute(database.read())
            .await
            .unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("8")
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
    async fn deferred_cache_queue_is_bounded_and_drains_before_close() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE example (value INTEGER)")
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
        held.commit().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), database.close())
            .await
            .unwrap();
        assert!(!database.defer(async { Ok(()) }));
    }

    #[tokio::test]
    async fn retry_restarts_the_transaction_after_busy_snapshot() {
        let database = Database::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE example (value INTEGER); INSERT INTO example VALUES (0)")
            .execute(&database)
            .await
            .unwrap();
        let external = Database::connect_with((*database.connect_options()).clone(), 2)
            .await
            .unwrap();
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let observed = crate::db::retry_transaction(|| async {
            let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Deliberately reproduce the old deferred read-to-write upgrade.
            let mut transaction = database.begin_with("BEGIN").await?;
            let value: i64 = sqlx::query_scalar("SELECT value FROM example")
                .fetch_one(&mut *transaction)
                .await?;
            if attempt == 0 {
                sqlx::query("UPDATE example SET value = 1")
                    .execute(&external)
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
        external.close().await;
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
