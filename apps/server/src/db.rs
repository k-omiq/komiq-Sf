use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub use crate::database::Database;
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
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(15));
    let pool = Database::connect_with(opts, 2).await?;
    let schema = validate_schema(&pool).await;
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

/// True if `e` is a transient SQLite lock/BUSY error worth retrying, rather than a
/// real failure.
///
/// This covers TWO distinct conditions that both surface with "database is locked":
///   - plain `SQLITE_BUSY` (code 5) — another writer holds the lock; `busy_timeout`
///     normally absorbs this, and seeing it means the timeout was exhausted.
///   - `SQLITE_BUSY_SNAPSHOT` (code 517) — a DEFERRED transaction read first, then
///     tried to upgrade to a write after another writer had already committed.
///     `busy_timeout` does not and structurally CANNOT retry this one: the
///     transaction's read snapshot is stale, so the only correct recovery is to
///     abort and re-run the whole transaction. Callers must retry, not just wait.
///
pub fn is_locked_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<sqlx::Error>())
        .any(sqlite_locked)
}

fn sqlite_locked(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .and_then(|code| code.parse::<u32>().ok())
        .is_some_and(|code| matches!(code & 0xff, 5 | 6))
}

pub(crate) fn record_sqlite_error(error: &sqlx::Error) {
    if sqlite_locked(error) {
        LOCK_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn record_write_wait(wait: Duration) {
    WRITE_QUEUE_WAIT_MICROS.fetch_add(wait.as_micros() as u64, Ordering::Relaxed);
    WRITE_QUEUE_ACQUISITIONS.fetch_add(1, Ordering::Relaxed);
}

/// Retry a complete database operation, including its BEGIN and all reads. The
/// callback must begin a fresh transaction on every attempt and must not perform
/// external side effects. Never retry just the failed statement in a transaction.
pub async fn retry_transaction<T, F, Fut>(mut attempt: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    use rand_core::RngCore;
    for retry in 0..5u32 {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) if retry < 4 && is_locked_error(&error) => {
                let base = 25u64 << retry;
                let jitter = rand_core::OsRng.next_u64() % (base + 1);
                tokio::time::sleep(Duration::from_millis(base + jitter)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("last retry returns")
}

/// Open (creating if needed) the SQLite database and run migrations.
///
/// WAL journaling is required for Litestream continuous backup (it ships the WAL
/// to object storage) and lets the background scan scheduler write without
/// blocking readers; `synchronous = NORMAL` is the WAL-safe durability level, and
/// a busy timeout absorbs the brief writer contention from scanner ticks.
pub async fn init(database_url: &str) -> Result<Database> {
    let opts = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        // Map projected growth up to 8 GiB. This reserves virtual address space;
        // only pages actually read become resident. .cargo/config.toml raises the
        // bundled SQLite ceiling to match. Litestream owns checkpoints.
        .pragma("mmap_size", "8589934592")
        .pragma("cache_size", "-16000")
        .pragma("temp_store", "MEMORY");
    let pool = Database::connect_with(opts, 8).await?;
    sqlx::migrate!("./migrations").run(pool.write()).await?;
    Ok(pool)
}

/// Open (creating if needed) the SEPARATE cover-cache DB and ensure its schema.
///
/// This DB holds only `work_cover_blob` (large, re-derivable-from-MangaDex cover
/// thumbnails). It is deliberately NOT the main DB: Litestream replicates only the
/// main DB to R2, so keeping covers here excludes them from backup while they're
/// still served from our own origin. Schema is a single table, created inline (no
/// migration directory) — there is no FK to `work` because that table lives in the
/// other DB; an orphaned blob (its work deleted) is harmless and re-derivable.
pub async fn init_covers(database_url: &str) -> Result<Database> {
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
pub async fn connect_existing_covers(database_url: &str) -> Result<Database> {
    let pool = Database::connect_with(
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

async fn open_covers(database_url: &str) -> Result<Database> {
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
    let pool = Database::connect_with(opts, 4).await?;
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
            // excluded from Litestream backup.
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
            // excluded from Litestream backup, and dropping the table is always safe.
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
            // excluded from Litestream backup.
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

/// Repair dangling pointers even when only part of the cover cache is missing.
/// Each write is bounded and conditional on the value we inspected, so another
/// materializer cannot have its freshly updated pointer cleared by this pass.
pub async fn reconcile_cover_pointers(pool: &Database, covers: &Database) -> Result<u64> {
    const BATCH: i64 = 256;
    let mut cursor = String::new();
    let mut cleared = 0;
    loop {
        let pointers: Vec<(String, i64)> = sqlx::query_as(
            "SELECT id, cover_cached_version FROM work \
             WHERE id > ? AND cover_cached_version IS NOT NULL ORDER BY id LIMIT ?",
        )
        .bind(&cursor)
        .bind(BATCH)
        .fetch_all(pool)
        .await?;
        let Some((last, _)) = pointers.last() else {
            break;
        };
        cursor.clone_from(last);
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
            "SELECT work_id FROM work_cover_blob WHERE work_id IN (",
        );
        let mut ids = query.separated(",");
        for (id, _) in &pointers {
            ids.push_bind(id);
        }
        ids.push_unseparated(")");
        let present: std::collections::HashSet<String> = query
            .build_query_scalar()
            .fetch_all(covers)
            .await?
            .into_iter()
            .collect();
        let missing: Vec<_> = pointers
            .iter()
            .filter(|(id, _)| !present.contains(id))
            .collect();
        if missing.is_empty() {
            continue;
        }
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        for (id, version) in missing {
            cleared += sqlx::query(
                "UPDATE work SET cover_cached_version = NULL WHERE id = ? AND cover_cached_version = ?",
            )
            .bind(id)
            .bind(version)
            .execute(&mut *transaction)
            .await?
            .rows_affected();
        }
        transaction.commit().await?;
    }
    Ok(cleared)
}

/// Tables whose `sqlite_stat1` rows the planner actually depends on, and whose row
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
/// Migration 0058 runs a targeted `ANALYZE` once, but nothing refreshed it afterwards —
/// which is how production ended up with `sqlite_stat1` written when `_sqlx_migrations`
/// held only 48 rows, leaving the newest tables with no stat rows at all and the planner
/// assuming ~1M rows for each of them.
///
pub fn spawn_analyze(pool: Database, shutdown: tokio::sync::watch::Receiver<bool>) {
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

/// Refresh one index at a time so even a cold statistics pass releases the writer.
async fn refresh_statistics(pool: &Database) {
    let started = std::time::Instant::now();
    let mut ok = 0usize;
    for table in ANALYZE_TABLES {
        let indices = sqlx::query_scalar::<_, String>(
            "SELECT name FROM sqlite_schema WHERE type = 'index' AND tbl_name = ? ORDER BY name",
        )
        .bind(table)
        .fetch_all(pool)
        .await;
        let indices = match indices {
            Ok(indices) => indices,
            Err(error) => {
                tracing::warn!(table, %error, "analyze: index inventory failed");
                continue;
            }
        };
        for index in indices {
            let result = async {
                let mut connection = pool.write().acquire().await?;
                sqlx::query("PRAGMA analysis_limit = 400")
                    .execute(&mut *connection)
                    .await?;
                // Names come from SQLite's schema, with identifier quoting retained.
                let quoted = index.replace('"', r#""""#);
                sqlx::query(&format!("ANALYZE \"{quoted}\""))
                    .execute(&mut *connection)
                    .await
            }
            .await;
            match result {
                Ok(_) => ok += 1,
                Err(error) => tracing::warn!(%index, %error, "analyze: index failed"),
            }
            tokio::task::yield_now().await;
        }
    }
    tracing::info!(
        indices = ok,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "analyze: query statistics refreshed"
    );
}

#[cfg(test)]
mod boot_tests {
    use super::*;

    #[tokio::test]
    async fn main_pool_maps_projected_eight_gib_on_reader_and_writer() {
        let scratch = Scratch::new();
        let pool = init(&scratch.url("main.sqlite3")).await.unwrap();
        for connection_pool in [pool.read(), pool.write()] {
            let effective: i64 = sqlx::query_scalar("PRAGMA mmap_size")
                .fetch_one(connection_pool)
                .await
                .unwrap();
            assert!(
                effective >= 8 * 1024 * 1024 * 1024,
                "SQLite capped mmap_size at {effective} bytes"
            );
        }
        pool.close().await;
    }

    #[tokio::test]
    async fn cover_schema_version_upgrades_legacy_data_and_rejects_future_versions() {
        let scratch = Scratch::new();
        let url = scratch.url("covers.sqlite3");
        let legacy = Database::connect_with(
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
        let scratch = Scratch::new();
        let url = scratch.url("main.sqlite3");
        let server = init(&url).await.unwrap();
        let latest = sqlx::migrate!("./migrations")
            .iter()
            .last()
            .unwrap()
            .version;
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = ?")
            .bind(latest)
            .execute(&server)
            .await
            .unwrap();
        let error = connect_existing(&url).await.unwrap_err();
        assert!(error.to_string().contains("schema is behind"));
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE version = ?")
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
        let scratch = Scratch::new();
        let url = scratch.url("main.sqlite3");
        let server = init(&url).await.unwrap();
        let maintenance = connect_existing(&url).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT 1")
                .fetch_one(&maintenance)
                .await
                .unwrap(),
            1
        );
        maintenance.close().await;
        sqlx::query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 1")
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
        sqlx::query("CREATE TABLE work (id TEXT PRIMARY KEY, cover_cached_version INTEGER)")
            .execute(&main)
            .await
            .unwrap();
        let covers = init_covers(&scratch.url("covers.sqlite3")).await.unwrap();
        for n in 0..600 {
            sqlx::query("INSERT INTO work VALUES (?, ?)")
                .bind(format!("w_{n:04}"))
                .bind(if n == 599 { None } else { Some(7_i64) })
                .execute(&main)
                .await
                .unwrap();
        }
        for id in ["w_0000", "w_0300", "w_0598"] {
            sqlx::query("INSERT INTO work_cover_blob VALUES (?, X'01', 7, 'now')")
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
