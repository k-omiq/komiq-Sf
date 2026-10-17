//! A server from before PostgreSQL kept its main data in `komika.sqlite3`
//! beside the covers file. This build cannot read or convert that file. A start
//! that finds it while PostgreSQL holds no data refuses with one clear message,
//! so an old install never comes up empty without the owner knowing why. A
//! PostgreSQL database that an earlier release's import left unfinished is
//! refused too: its rows are partial and its triggers may still be off.
use anyhow::Result;
use sqlx::{ConnectOptions, Connection, PgConnection};
use std::path::{Path, PathBuf};
use std::str::FromStr;

const DOCS: &str = "https://docs.komiq.cc/self-host/update-and-backup#old-sqlite-install";

/// The old main database: `komika.sqlite3` in the covers file's directory.
/// An in-memory covers database has no directory, so there is nothing to find.
fn legacy_path(covers: &str) -> Result<Option<PathBuf>> {
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(covers)
        .map_err(|_| anyhow::anyhow!("invalid covers SQLite URL [redacted]"))?;
    let file = options.get_filename();
    if covers.contains("mode=memory")
        || file == Path::new(":memory:")
        || file.to_string_lossy().starts_with("file:sqlx-in-memory-")
    {
        return Ok(None);
    }
    let parent = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(Some(std::path::absolute(parent.join("komika.sqlite3"))?))
}

struct Postgres {
    /// No table outside the system schemas.
    empty: bool,
    /// An earlier release's SQLite import began and never finished.
    unfinished_import: bool,
}

async fn inspect(url: &str) -> Result<Postgres> {
    let options = sqlx::postgres::PgConnectOptions::from_str(url)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection argument [redacted]"))?;
    let mut conn = PgConnection::connect_with(&options.disable_statement_logging())
        .await
        .map_err(|_| anyhow::anyhow!("PostgreSQL connection failed [redacted]"))?;
    let result = async {
        let tables: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r','p','f','m') AND n.nspname <> 'information_schema' \
             AND n.nspname !~ '^pg_')",
        )
        .fetch_one(&mut conn)
        .await?;
        let state: bool =
            sqlx::query_scalar("SELECT to_regclass('public.komika_import_state') IS NOT NULL")
                .fetch_one(&mut conn)
                .await?;
        let completed: Option<bool> = if state {
            sqlx::query_scalar("SELECT completed FROM public.komika_import_state WHERE singleton")
                .fetch_optional(&mut conn)
                .await?
        } else {
            Some(true)
        };
        anyhow::Ok(Postgres {
            empty: !tables,
            unfinished_import: completed != Some(true),
        })
    }
    .await;
    // The answer is read; a close that fails changes nothing.
    let _ = conn.close().await;
    result
}

/// Called with the data-directory lock held, before `db::init` creates anything.
pub async fn refuse_unconverted(database_url: &str, covers_url: &str) -> Result<()> {
    let legacy = legacy_path(covers_url)?.filter(|path| path.try_exists().unwrap_or(true));
    let postgres = inspect(database_url).await?;
    if postgres.unfinished_import {
        anyhow::bail!(
            "PostgreSQL holds a SQLite import that an earlier release began and never \
             finished. This version cannot finish or serve it. Restore a PostgreSQL backup, \
             or empty the database to start fresh. See {DOCS}"
        );
    }
    let Some(path) = legacy else {
        return Ok(());
    };
    if postgres.empty {
        anyhow::bail!(
            "found an old SQLite database at {path} and PostgreSQL holds no data. This \
             version stores data only in PostgreSQL and cannot read or convert that file. \
             To start fresh, stop the server and move {path} (and any {path}-wal, \
             {path}-shm or {path}-journal file) out of the data directory. See {DOCS}",
            path = path.display()
        );
    }
    tracing::warn!(
        path = %path.display(),
        "an old SQLite database is in the data directory; this server does not use it"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media_manifest_tests::TempDir;

    fn covers_url(dir: &Path) -> String {
        format!("sqlite://{}", dir.join("covers.sqlite3").display())
    }

    #[test]
    fn legacy_path_is_beside_covers_and_absent_in_memory() {
        let dir = TempDir::new();
        assert_eq!(
            legacy_path(&covers_url(dir.0.as_path())).unwrap(),
            Some(dir.0.as_path().join("komika.sqlite3"))
        );
        assert_eq!(legacy_path("sqlite::memory:").unwrap(), None);
        assert_eq!(
            legacy_path("sqlite:///data/covers.sqlite3?mode=memory").unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn no_old_file_starts() {
        let dir = TempDir::new();
        for db in [
            crate::test_postgres::empty_database().unwrap(),
            crate::test_postgres::clone_database().unwrap(),
        ] {
            refuse_unconverted(&db.url, &covers_url(dir.0.as_path()))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn unfinished_import_refuses_and_finished_import_starts() {
        let dir = TempDir::new();
        let db = crate::test_postgres::clone_database().unwrap();
        let pool = sqlx::PgPool::connect(&db.url).await.unwrap();
        sqlx::query("CREATE TABLE public.komika_import_state (singleton boolean PRIMARY KEY, completed boolean NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO public.komika_import_state VALUES (true, false)")
            .execute(&pool)
            .await
            .unwrap();
        let error = refuse_unconverted(&db.url, &covers_url(dir.0.as_path()))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("never finished"), "{error}");
        assert!(error.contains(DOCS), "{error}");
        sqlx::query("UPDATE public.komika_import_state SET completed = true")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        refuse_unconverted(&db.url, &covers_url(dir.0.as_path()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn old_file_and_empty_postgres_refuse_with_docs_link() {
        let db = crate::test_postgres::empty_database().unwrap();
        let dir = TempDir::new();
        let old = dir.0.as_path().join("komika.sqlite3");
        std::fs::write(&old, b"SQLite format 3\0").unwrap();
        let error = refuse_unconverted(&db.url, &covers_url(dir.0.as_path()))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(&old.display().to_string()), "{error}");
        assert!(error.contains("cannot read or convert"), "{error}");
        assert!(error.contains(DOCS), "{error}");
        // Nothing was converted, moved or created.
        assert_eq!(std::fs::read(&old).unwrap(), b"SQLite format 3\0");
        assert!(inspect(&db.url).await.unwrap().empty);
    }

    #[tokio::test]
    async fn old_file_beside_postgres_data_starts() {
        let db = crate::test_postgres::clone_database().unwrap();
        let dir = TempDir::new();
        std::fs::write(dir.0.as_path().join("komika.sqlite3"), b"SQLite format 3\0").unwrap();
        refuse_unconverted(&db.url, &covers_url(dir.0.as_path()))
            .await
            .unwrap();
    }
}
