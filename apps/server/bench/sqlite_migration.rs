#[cfg(test)]
mod sqlite_migration_benchmark {
    use sqlx::Connection;
    use std::time::Instant;

    #[tokio::test]
    #[ignore = "requires an explicitly supplied scratch backup; run with --release"]
    async fn sqlite_release_migration_benchmark() {
        let path = std::path::PathBuf::from(
            std::env::var("KOMIQ_MIGRATION_BENCH_DB")
                .expect("KOMIQ_MIGRATION_BENCH_DB scratch copy"),
        );
        assert!(
            path.canonicalize().unwrap().starts_with(
                std::path::Path::new("/tmp/komiq-sqlite-safety")
                    .canonicalize()
                    .unwrap()
            ),
            "scratch databases only"
        );
        let bytes = std::fs::metadata(&path).unwrap().len();
        let mut before = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .read_only(true),
        )
        .await
        .unwrap();
        let already_applied: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version >= 120)",
        )
        .fetch_one(&mut before)
        .await
        .unwrap();
        assert!(
            !already_applied,
            "supply a fresh backup before migration 0120"
        );
        before.close().await.unwrap();

        // Use the actual production connection options and embedded SQLx
        // migrator, including its enclosing transaction and mmap/temp settings.
        let started = Instant::now();
        let pool = super::init_sqlite(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        let startup_seconds = started.elapsed().as_secs_f64();
        let migrations: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT version,execution_time FROM _sqlx_migrations WHERE version >= 119 ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        for (version, nanos) in migrations {
            println!(
                "MIGRATION_BENCH version={version} elapsed_ms={:.3}",
                nanos as f64 / 1_000_000.0
            );
        }
        let violations = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(violations.is_empty());
        let source_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM source_series")
            .fetch_one(&pool)
            .await
            .unwrap();
        let chapter_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chapter")
            .fetch_one(&pool)
            .await
            .unwrap();
        println!(
            "MIGRATION_STARTUP_BENCH debug_assertions={} bytes={bytes} elapsed_seconds={startup_seconds:.6} source_rows={source_rows} chapter_rows={chapter_rows} foreign_key_violations={}",
            cfg!(debug_assertions),
            violations.len()
        );
        pool.close().await;
    }
}

#[cfg(test)]
mod sqlite_migration_rollback {
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use sqlx::migrate::{Migrate, Migration};
    use sqlx::{Connection, Row, SqliteConnection};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("komiq-sqlx-migration-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn quoted_identifier(identifier: &str) -> String {
        format!("\"{}\"", identifier.replace('"', "\"\""))
    }

    async fn quoted_rows(conn: &mut SqliteConnection, table: &str) -> Vec<Vec<String>> {
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info($1) ORDER BY cid")
                .bind(table)
                .fetch_all(&mut *conn)
                .await
                .unwrap();
        assert!(!columns.is_empty(), "missing snapshot table {table}");
        let expressions = columns
            .iter()
            .map(|column| format!("quote({})", quoted_identifier(column)))
            .collect::<Vec<_>>()
            .join(",");
        let statement = format!("SELECT {expressions} FROM {}", quoted_identifier(table));
        let rows = sqlx::query(&statement).fetch_all(&mut *conn).await.unwrap();
        let mut values: Vec<Vec<String>> = rows
            .into_iter()
            .map(|row| {
                (0..columns.len())
                    .map(|index| row.try_get::<String, _>(index).unwrap())
                    .collect()
            })
            .collect();
        values.sort();
        values
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Snapshot {
        schema: Vec<Vec<String>>,
        temporary_schema: Vec<Vec<String>>,
        tables: BTreeMap<String, Vec<Vec<String>>>,
        versions: (i64, i64, i64),
    }

    async fn snapshot(conn: &mut SqliteConnection) -> Snapshot {
        let schema = quoted_rows(conn, "sqlite_schema").await;
        let temporary_schema = quoted_rows(conn, "sqlite_temp_schema").await;
        let names: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
                .fetch_all(&mut *conn)
                .await
                .unwrap();
        let mut tables = BTreeMap::new();
        for name in names {
            // Include migration bookkeeping, FTS backing tables and statistics,
            // not just the parent/children that 0120 intentionally reconstructs.
            let rows = quoted_rows(conn, &name).await;
            tables.insert(name, rows);
        }
        let user = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        let schema_version = sqlx::query_scalar("PRAGMA schema_version")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        let application = sqlx::query_scalar("PRAGMA application_id")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        Snapshot {
            schema,
            temporary_schema,
            tables,
            versions: (user, schema_version, application),
        }
    }

    const FIXTURE: &str = r#"
        INSERT INTO users(id,username,email,password_hash,created_at)
            VALUES('rollback_user','rollback_user','rollback@example.test','hash','2026-01-01');
        INSERT INTO user_avatars(user_id,webp,version,updated_at)
            VALUES('rollback_user',X'00017FFF',7,'2026-01-01');
        INSERT INTO work(id,primary_title,created_at,updated_at) VALUES
            ('rollback_old','Old','2026-01-01','2026-01-01'),
            ('rollback_keep','Keeper','2026-02-01','2026-02-01'),
            ('rollback_other','Other','2026-01-01','2026-01-01');
        INSERT INTO suwayomi_series(id,title,status,source_id,updated_at)
            VALUES(77,'Keeper','ONGOING','known','2026-01-01');
        INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) VALUES
            ('rollback_old_source','rollback_old','suwayomi','','77','2026-01-01'),
            ('rollback_keep_source','rollback_keep','suwayomi','known','77','2026-02-01'),
            ('rollback_md_source','rollback_keep','mangadex','mangadex','md-77','2026-02-01');
        INSERT INTO chapter(id,source_series_id,external_id,number,volume,chapter_key,created_at) VALUES
            ('rollback_old_shared','rollback_old_source','shared','1','2','100','2026-01-01'),
            ('rollback_keep_shared','rollback_keep_source','shared','1',NULL,'100','2026-02-01'),
            ('rollback_old_unique','rollback_old_source','unique','2','2','200','2026-01-01');
        INSERT INTO merge_candidate(id,source_series_id,candidate_work_id,score,method,status,created_at)
            VALUES('rollback_candidate','rollback_old_source','rollback_other',0.8,'title_exact','pending','2026-01-01');
        INSERT INTO release_event(work_id,chapter_key,first_seen_at,first_source_series_id,label,announce_at)
            VALUES('rollback_old','200',1000,'rollback_old_source','2',2000);
        INSERT INTO user_library(user_id,series_id,created_at)
            VALUES('rollback_user','rollback_old','2026-01-01');
        INSERT INTO canonical_progress(user_id,chapter_id,work_id,last_page_read,updated_at)
            VALUES('rollback_user','shared','rollback_old',15,'2026-01-01');
        PRAGMA user_version=17;
        PRAGMA application_id=1234;
    "#;

    #[tokio::test]
    async fn sqlx_migration_0120_fault_rolls_back_without_caller_rollback() {
        let migrations = sqlx::migrate!("./migrations");
        let original = migrations.iter().find(|m| m.version == 120).unwrap();
        for marker in [
            "DROP TABLE source_series;",
            "DROP TABLE _ss_identity_release_history;",
        ] {
            let scratch = Scratch::new();
            let path = scratch.0.join("main.sqlite3");
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
            let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
            conn.ensure_migrations_table().await.unwrap();
            for migration in migrations.iter().filter(|m| m.version < 120) {
                conn.apply(migration).await.unwrap();
            }
            sqlx::raw_sql(FIXTURE).execute(&mut conn).await.unwrap();
            let before = snapshot(&mut conn).await;
            assert!(before.temporary_schema.is_empty());
            assert_eq!(original.sql.matches(marker).count(), 1);
            let injected_sql = original.sql.replacen(
                marker,
                &format!("{marker}\nSELECT * FROM deliberately_missing_0120_table;"),
                1,
            );
            let injected = Migration::new(
                original.version,
                original.description.clone(),
                original.migration_type,
                Cow::Owned(injected_sql),
                original.no_tx,
            );

            // Use SQLx's real migration implementation and transaction Drop.
            // The caller never begins, commits, or rolls back a transaction.
            let failure = conn.apply(&injected).await.unwrap_err();
            assert!(
                failure
                    .to_string()
                    .contains("deliberately_missing_0120_table"),
                "fault was not reached after {marker}: {failure}"
            );
            assert_eq!(snapshot(&mut conn).await, before, "after {marker}");
            let applied: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE version >= 120")
                    .fetch_one(&mut conn)
                    .await
                    .unwrap();
            assert_eq!(applied, 0);
            assert_eq!(
                sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
                    .fetch_one(&mut conn)
                    .await
                    .unwrap(),
                1
            );
            assert!(sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&mut conn)
                .await
                .unwrap()
                .is_empty());
            assert_eq!(
                sqlx::query_scalar::<_, String>("PRAGMA integrity_check")
                    .fetch_all(&mut conn)
                    .await
                    .unwrap(),
                ["ok"]
            );
            // Closing/reopening also proves the committed file is intact; the
            // complete snapshot includes the absence of a failed ledger entry.
            conn.close().await.unwrap();
            let mut reopened = SqliteConnection::connect_with(&options).await.unwrap();
            assert_eq!(snapshot(&mut reopened).await, before);
            reopened.apply(original).await.unwrap();
            assert_eq!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM _sqlx_migrations WHERE version=120 AND success=1",
                )
                .fetch_one(&mut reopened)
                .await
                .unwrap(),
                1
            );
            assert!(sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&mut reopened)
                .await
                .unwrap()
                .is_empty());
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM chapter")
                    .fetch_one(&mut reopened)
                    .await
                    .unwrap(),
                2
            );
            assert_eq!(
                sqlx::query_scalar::<_, String>(
                    "SELECT source_series_id FROM merge_candidate WHERE id='rollback_candidate'",
                )
                .fetch_one(&mut reopened)
                .await
                .unwrap(),
                "rollback_keep_source"
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT first_seen_at FROM release_event WHERE work_id='rollback_keep' AND chapter_key='200'",
                )
                .fetch_one(&mut reopened)
                .await
                .unwrap(),
                1000
            );
            reopened.close().await.unwrap();
        }
    }
}
