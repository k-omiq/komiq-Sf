//! The kept SQLite file. It is the read-only template of the output: this
//! module opens it with read-only handles only and never changes it.
use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use sqlx::{Connection, Row, SqliteConnection};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use super::tables::{copied, TARGETS};
use crate::pg_import::{plan::quote, source::SQLITE};

// The read-only backup reads through a `-wal` and changes neither file.
const MAKE_SAFE: &str = "stop every server that uses this file; if a -wal or -journal with content remains, make a clean copy with `sqlite3 -readonly <kept file> \".backup <copy>\"` and pass the copy as --template";

pub(super) fn sibling(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", path.display()))
}

/// A `-wal` or `-journal` file with content holds changes that are not in the
/// main file: a copy of the main file alone would not be the database. An
/// empty one holds nothing (SQLite leaves an empty `-wal` beside a WAL-mode
/// file that a read-only connection opened).
///
/// `path` has no symbolic link in it (`export::resolve`): SQLite names these
/// files after the real file, and so must this check.
pub(super) fn refuse_live_sidecars(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-journal"] {
        let sidecar = sibling(path, suffix);
        if let Ok(metadata) = std::fs::metadata(&sidecar) {
            ensure!(
                metadata.len() == 0,
                "the template has a live {} ({} bytes): a server has it open or stopped uncleanly; {MAKE_SAFE}",
                sidecar.display(),
                metadata.len()
            );
        }
    }
    Ok(())
}

pub(super) struct Template {
    pub conn: SqliteConnection,
    /// Highest applied SQLite migration: 121 or 122.
    pub target: i64,
    /// `sqlite_schema` as (type, name, tbl_name, sql), in creation order.
    pub schema: Vec<(String, String, String, Option<String>)>,
    // Held to the end: no server can start on the template's directory.
    _data_directory: Option<File>,
}

/// Open the template inside one read transaction and prove it is a database
/// of the old build at migration 0121 or 0122.
///
/// A rollback-journal file: the transaction's shared lock stays to the end of
/// the run, so no other process can write the file while it is copied. A
/// WAL-mode file (the old build's mode): a shared lock stops no WAL writer,
/// and a plain read-only connection would create `-shm` and `-wal` files
/// beside the kept file. It is opened immutable instead: no lock, no file.
/// What protects the copy there is that a writer shows in the `-wal` or in
/// the file's hash, and both are checked again after the run.
pub(super) async fn open(path: &Path) -> Result<Template> {
    ensure!(path.is_file(), "the template must be an existing file");
    refuse_live_sidecars(path)?;
    let mut header = [0u8; 20];
    File::open(path)?
        .read_exact(&mut header)
        .context("the template is not a SQLite database")?;
    ensure!(
        &header[..16] == b"SQLite format 3\0",
        "the template is not a SQLite database"
    );
    let wal_mode = header[18] == 2 || header[19] == 2;
    // The old build holds this lock for as long as it runs (`DataDirectoryLock`).
    let lock_path = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".komiq-server.lock");
    let data_directory = match File::open(&lock_path) {
        Ok(file) => {
            file.try_lock().map_err(|_| {
                anyhow::anyhow!(
                    "a server holds {}: it is running on the template's directory; {MAKE_SAFE}",
                    lock_path.display()
                )
            })?;
            Some(file)
        }
        Err(_) => None,
    };
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        .immutable(wal_mode)
        .busy_timeout(Duration::from_secs(1))
        .pragma("query_only", "ON")
        .pragma("temp_store", "FILE")
        .pragma("cache_size", "-8192");
    let busy = |error: sqlx::Error| match &error {
        sqlx::Error::Database(db)
            if db
                .code()
                .and_then(|code| code.parse::<i64>().ok())
                .is_some_and(|code| matches!(code & 0xff, 5 | 6)) =>
        {
            anyhow::anyhow!("another process has locked the template; {MAKE_SAFE}")
        }
        _ => anyhow::Error::new(error),
    };
    let mut conn = SqliteConnection::connect_with(&options)
        .await
        .map_err(busy)?;
    sqlx::query("BEGIN")
        .execute(&mut conn)
        .await
        .map_err(busy)?;
    // The first read takes the shared lock (rollback-journal file).
    let schema: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY rowid")
            .fetch_all(&mut conn)
            .await
            .map_err(busy)?;
    let applied: Vec<(i64, bool, Vec<u8>)> =
        sqlx::query_as("SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut conn)
            .await
            .context("the template has no SQLx migration history")?;
    let target = applied.last().map_or(0, |(version, _, _)| *version);
    ensure!(
        TARGETS.contains(&target),
        "the template is at SQLite migration {target}; the export writes a file at migration 121 or 122 only"
    );
    let mut expected = SQLITE
        .iter()
        .filter(|m| !m.migration_type.is_down_migration() && m.version <= target)
        .collect::<Vec<_>>();
    expected.sort_by_key(|m| m.version);
    ensure!(
        applied.len() == expected.len(),
        "the template's migration history is not the history of the SQLite build"
    );
    for ((version, success, checksum), migration) in applied.iter().zip(expected) {
        ensure!(
            *success
                && *version == migration.version
                && checksum.as_slice() == &*migration.checksum,
            "the template's migration history differs from the SQLite build at {version}"
        );
    }
    for table in copied(target) {
        let actual = sqlx::query(&format!("PRAGMA table_xinfo({})", quote(&table.name)))
            .fetch_all(&mut conn)
            .await?;
        ensure!(
            actual.len() == table.columns.len(),
            "{} column count differs from the import plan",
            table.name
        );
        for (row, c) in actual.iter().zip(&table.columns) {
            ensure!(
                row.try_get::<String, _>("name")? == c.name
                    && row.try_get::<String, _>("type")? == c.source
                    && (row.try_get::<i64, _>("hidden")? != 0) == c.generated,
                "{}.{} differs from the import plan",
                table.name,
                c.name
            );
        }
    }
    Ok(Template {
        conn,
        target,
        schema,
        _data_directory: data_directory,
    })
}

/// SHA-256 of a file, read in pieces.
pub(super) fn sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            return Ok(hex::encode(hash.finalize()));
        }
        hash.update(&buffer[..n]);
    }
}

/// Copy the template's bytes into a new file and return their SHA-256 and
/// count. `create_new` refuses a name that exists, so the copy never lands on
/// a file this run did not make.
pub(super) fn copy(template: &Path, partial: &Path) -> Result<(String, u64)> {
    let mut source = File::open(template)?;
    let mut header = [0u8; 16];
    source.read_exact(&mut header)?;
    ensure!(
        &header == b"SQLite format 3\0",
        "the template is not a SQLite database"
    );
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // The file holds password hashes and session tokens: the owner alone reads it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut target = options
        .open(partial)
        .with_context(|| format!("create {}", partial.display()))?;
    let copied = (|| {
        let mut hash = Sha256::new();
        hash.update(header);
        target.write_all(&header)?;
        let mut bytes = header.len() as u64;
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            target.write_all(&buffer[..n])?;
            bytes += n as u64;
        }
        target.sync_all()?;
        // A writer that started after the first check would show here.
        refuse_live_sidecars(template)?;
        if bytes != std::fs::metadata(template)?.len() {
            bail!("the template changed size while it was copied; {MAKE_SAFE}");
        }
        Ok((hex::encode(hash.finalize()), bytes))
    })();
    if copied.is_err() {
        // This run created the file a moment ago; nothing else is removed.
        let _ = std::fs::remove_file(partial);
    }
    copied
}

pub(super) async fn close(template: Template) -> Result<()> {
    let mut conn = template.conn;
    sqlx::query("ROLLBACK").execute(&mut conn).await?;
    conn.close().await?;
    Ok(())
}
