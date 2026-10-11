#[cfg(test)]
mod incremental_projection_benchmark {
    use super::*;
    use sqlx::Connection;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    async fn measured<F, T>(
        options: &sqlx::sqlite::SqliteConnectOptions,
        operation: F,
    ) -> (T, Duration, u64)
    where
        F: std::future::Future<Output = Result<T>>,
    {
        // Open before the operation; opening while its writer is busy can itself fail.
        let mut connection = sqlx::SqliteConnection::connect_with(options).await.unwrap();
        sqlx::query("PRAGMA busy_timeout = 0")
            .execute(&mut connection)
            .await
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let probe_stop = stop.clone();
        let probe = tokio::spawn(async move {
            let mut since = None;
            let mut longest = Duration::ZERO;
            let mut busy = 0;
            while !probe_stop.load(Ordering::Relaxed) {
                match sqlx::query("BEGIN IMMEDIATE")
                    .execute(&mut connection)
                    .await
                {
                    Ok(_) => {
                        sqlx::query("ROLLBACK")
                            .execute(&mut connection)
                            .await
                            .unwrap();
                        if let Some(start) = since.take() {
                            longest = longest.max(Instant::now().duration_since(start));
                        }
                    }
                    Err(error) => {
                        let error: anyhow::Error = error.into();
                        assert!(
                            crate::db::is_locked_error(&error),
                            "unexpected probe error: {error}"
                        );
                        since.get_or_insert_with(Instant::now);
                        busy += 1;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if let Some(start) = since {
                longest = longest.max(Instant::now().duration_since(start));
            }
            (longest, busy)
        });
        tokio::task::yield_now().await;
        let result = operation.await;
        stop.store(true, Ordering::Relaxed);
        let (longest, busy) = probe.await.unwrap();
        (result.unwrap(), longest, busy)
    }

    /// Run only on an expendable clone of the real-size database. Keeps its full FTS
    /// corpus while adding 128 source mappings and 8,192 chapters to the timed works.
    #[tokio::test]
    #[ignore = "requires KOMIQ_PROJECTION_BENCH_DB pointing at a real-size scratch copy"]
    async fn incremental_projection_writer_holds_on_real_size_catalogue() {
        let path = std::env::var("KOMIQ_PROJECTION_BENCH_DB").expect("scratch database required");
        let canonical = std::fs::canonicalize(&path).unwrap();
        assert!(
            canonical.starts_with("/tmp/komiq-sqlite-safety")
                || canonical.starts_with("/private/tmp/komiq-sqlite-safety")
        );
        assert_ne!(canonical.file_name().unwrap(), "base.sqlite3");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(15));
        let pool = Database::connect_with(options.clone(), 8).await.unwrap();
        sqlx::migrate!("./migrations")
            .run(pool.write())
            .await
            .unwrap();
        let corpus: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM work_fts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            corpus >= 100_000,
            "use the real-size scratch copy: {corpus} FTS rows"
        );
        let suffix = uuid::Uuid::new_v4().to_string();
        let source = create_work(
            &pool,
            &WorkInput {
                primary_title: Some(format!("Projection Source {suffix}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let target = upsert_work_from_mangadex(
            &pool,
            &format!("projection-target-{suffix}"),
            &WorkInput {
                primary_title: Some(format!("Projection Target {suffix}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let prefix = format!("projection-source-{suffix}-");
        let first_manga: i64 = sqlx::query_scalar(
            "SELECT MAX(COALESCE(MAX(id), 0), 9000000000) + 1024 FROM suwayomi_series",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let now = Utc::now().to_rfc3339();
        let mut setup = pool.begin().await.unwrap();
        sqlx::query(
            "WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<127) \
             INSERT INTO source_series(id,work_id,source_type,source_id,source_key,created_at) \
             SELECT $1 || i, $2, 'suwayomi', 'projection-benchmark', CAST($3+i AS TEXT), $4 FROM n",
        )
        .bind(&prefix)
        .bind(&source)
        .bind(first_manga)
        .bind(&now)
        .execute(&mut *setup)
        .await
        .unwrap();
        sqlx::query(
            "WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<127) \
             INSERT INTO suwayomi_series(id,title,status,source_id,lang,chapter_count,updated_at,created_at) \
             SELECT $1+i, 'Projection source ' || i, 'ONGOING', 'projection-benchmark', 'en', 64, $2, $2 FROM n",
        ).bind(first_manga).bind(&now).execute(&mut *setup).await.unwrap();
        sqlx::query(
            "WITH RECURSIVE s(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM s WHERE i<127), \
             n(ch) AS (VALUES(1) UNION ALL SELECT ch+1 FROM n WHERE ch<64) \
             INSERT INTO chapter(id,source_series_id,external_id,number,lang,title,readable_at,chapter_key,label,created_at) \
             SELECT $1 || i || '-' || ch, $1 || i, CAST(ch AS TEXT), CAST(ch AS TEXT), 'en', \
                    'Chapter ' || ch, '2026-01-01T00:00:00Z', CAST(ch*10 AS TEXT), CAST(ch AS TEXT), $2 \
               FROM s CROSS JOIN n",
        ).bind(&prefix).bind(&now).execute(&mut *setup).await.unwrap();
        upsert_browse_work(&mut setup, &source).await.unwrap();
        setup.commit().await.unwrap();

        let (merged, merge_hold, merge_probes) =
            measured(&options, merge_works(&pool, &source, &target)).await;
        assert_eq!(merged.moved_source_series, 128);
        let detached: Vec<String> = (0..64).map(|i| format!("{prefix}{i}")).collect();
        let (split, split_hold, split_probes) = measured(
            &options,
            split_source_series(&pool, &target, &detached, None),
        )
        .await;
        assert_eq!(split.moved_sources, 64);
        for work in [&target, &split.new_work_id] {
            let count: i64 = sqlx::query_scalar(
                "SELECT CAST(chapters AS INTEGER) FROM work_fts WHERE work_id = $1",
            )
            .bind(work)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(count, 4096);
        }
        println!("INCREMENTAL_PROJECTION_BENCH corpus={corpus} sources=128 chapters=8192 merge_hold_ms={} split_hold_ms={} merge_busy_probes={merge_probes} split_busy_probes={split_probes}", merge_hold.as_millis(), split_hold.as_millis());
        assert!(
            merge_hold < Duration::from_secs(1),
            "merge writer held for {merge_hold:?}"
        );
        assert!(
            split_hold < Duration::from_secs(1),
            "split writer held for {split_hold:?}"
        );
        pool.close().await;
    }
}
