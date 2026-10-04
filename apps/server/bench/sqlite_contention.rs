#[cfg(test)]
mod sqlite_writer_benchmark {
    use super::*;
    use sqlx::Connection;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    #[derive(Clone)]
    struct BenchLogs(Arc<std::sync::atomic::AtomicU64>);

    impl std::io::Write for BenchLogs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let line = String::from_utf8_lossy(bytes);
            if line.contains("locked") || line.contains("SQLITE_BUSY") {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn http_benchmark(
        pool: &Database,
        prefix: &str,
    ) -> (tokio::task::JoinHandle<()>, Vec<std::process::Child>) {
        use crate::graphql::*;
        let work: String = sqlx::query_scalar("SELECT b.work_id FROM browse_catalogue b JOIN work w ON w.id = b.work_id \
            WHERE b.reader_id LIKE 'w_%' AND b.is_nsfw = 0 AND COALESCE(w.is_nsfw_override,w.is_nsfw) = 0 \
            AND EXISTS (SELECT 1 FROM source_series ss WHERE ss.work_id = b.work_id AND ss.source_type='mangadex') LIMIT 1")
            .fetch_one(pool).await.unwrap();
        let token = "sqlite-local-benchmark-token";
        sqlx::query("INSERT OR IGNORE INTO users (id, username, email, password_hash, is_admin, created_at) \
            VALUES ('sqlite-local-benchmark','sqlite_local_benchmark','sqlite-local-benchmark@example.invalid','unused',0,'2026-10-08T00:00:00Z')")
            .execute(pool).await.unwrap();
        sqlx::query(
            "INSERT OR REPLACE INTO sessions(token,user_id,created_at,expires_at) VALUES (?, \
            'sqlite-local-benchmark','2026-10-08T00:00:00Z','2999-01-01T00:00:00Z')",
        )
        .bind(crate::auth::hash_token(token))
        .execute(pool)
        .await
        .unwrap();
        let state = Arc::new(AppState {
            pool: pool.clone(),
            cover_pool: pool.clone(),
            suwayomi: crate::suwayomi::SuwayomiClient::new("http://127.0.0.1:1".into(), None, None),
            mangadex: Arc::new(crate::mangadex::MangaDexClient::new(
                "sqlite-local-benchmark",
                5.0,
                40.0,
            )),
            admin_users: vec![],
            scan_health: std::sync::Mutex::new(ScanHealth::default()),
            auth_limiter: RateLimiter::new(100, 60),
            comment_limiter: comment_limiter(),
            password_work: Arc::new(tokio::sync::Semaphore::new(4)),
            federated_limiter: RateLimiter::new(100, 60),
            session_ttl_secs: 30 * 24 * 60 * 60,
            series_inflight: KeyedLocks::default(),
            chapters_inflight: KeyedLocks::default(),
            cover_crawl_running: Arc::new(AtomicBool::new(false)),
            catalogue_cover_phash: false,
            ext_icons_dir: std::path::PathBuf::new(),
            setup_code: crate::instance::SetupCode::default(),
        });
        state.mangadex.set_enabled(false);
        let schema = build_schema(state, false);
        let mutation = format!("mutation BenchmarkFavorite {{ setFavorite(seriesId: \"{work}\", favorite: true) {{ id }} }}");
        let request = async_graphql::Request::new(&mutation)
            .data(RequestAuth(Some(token.into())))
            .data(ClientIp(Some("127.0.0.1".into())))
            .data(RequestUserCache::default())
            .data(RequestNsfwPref::default())
            .data(RequestLibraryCache::default())
            .data(RequestBlocks::default());
        let check = schema.execute(request).await;
        assert!(
            check.errors.is_empty(),
            "benchmark mutation preflight: {:?}",
            check.errors
        );
        let app = axum::Router::new().route("/graphql", axum::routing::post(
            |axum::extract::State(schema): axum::extract::State<ApiSchema>,
             axum::Json(mut request): axum::Json<async_graphql::Request>| async move {
                let auth = request.query.contains("mutation").then(|| "sqlite-local-benchmark-token".to_string());
                request = request.data(RequestAuth(auth)).data(ClientIp(Some("127.0.0.1".into())))
                    .data(RequestUserCache::default()).data(RequestNsfwPref::default())
                    .data(RequestLibraryCache::default()).data(RequestBlocks::default());
                axum::Json(schema.execute(request).await)
             })).with_state(schema);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:18089")
            .await
            .unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let queries = format!("{prefix}-write-queries.json");
        std::fs::write(
            &queries,
            serde_json::to_vec(&serde_json::json!({"facets":mutation})).unwrap(),
        )
        .unwrap();
        let mut children = Vec::new();
        for (scenario, rate, kind) in [("mixed", "10", "read"), ("facets", "2", "write")] {
            let mut command = std::process::Command::new("node");
            command
                .arg("bench/loadtest.mjs")
                .args([
                    "--target",
                    "http://127.0.0.1:18089",
                    "--scenario",
                    scenario,
                    "--rate",
                    rate,
                    "--duration",
                    "20",
                    "--warmup",
                    "0",
                    "--max-p95",
                    "60000",
                    "--json",
                ])
                .arg(format!("{prefix}-{kind}.json"));
            if kind == "write" {
                command.arg("--queries").arg(&queries);
            }
            command
                .stdout(std::fs::File::create(format!("{prefix}-{kind}.log")).unwrap())
                .stderr(std::process::Stdio::inherit());
            children.push(command.spawn().unwrap());
        }
        println!("HTTP_BENCH prefix={prefix} work={work} read_rate=10 write_rate=2 duration=20s");
        (server, children)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires an explicitly supplied scratch copy of a real-size database"]
    async fn sqlite_writer_hold_benchmark() {
        let path = std::env::var("KOMIQ_BENCH_DB").expect("KOMIQ_BENCH_DB scratch database");
        assert!(
            path.starts_with("/tmp/komiq-sqlite-safety/"),
            "scratch databases only"
        );
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(15));
        let pool = Database::connect_with(options.clone(), 8).await.unwrap();
        // Migration time is startup work, excluded from request-storm measurements.
        sqlx::migrate!("./migrations")
            .run(pool.write())
            .await
            .unwrap();
        let migrations: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT version, execution_time FROM _sqlx_migrations WHERE version >= 119 ORDER BY version",
        ).fetch_all(&pool).await.unwrap();
        for (version, nanos) in migrations {
            println!(
                "MIGRATION_BENCH version={version} elapsed_ms={:.3}",
                nanos as f64 / 1_000_000.0
            );
        }

        let lock_errors = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let log_counter = lock_errors.clone();
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || BenchLogs(log_counter.clone()))
                .finish(),
        );
        let http = if let Ok(prefix) = std::env::var("KOMIQ_HTTP_BENCH_PREFIX") {
            Some(http_benchmark(&pool, &prefix).await)
        } else {
            None
        };
        let stop = Arc::new(AtomicBool::new(false));
        let probe_stop = stop.clone();
        let mut conn = sqlx::SqliteConnection::connect_with(&options)
            .await
            .unwrap();
        sqlx::query("PRAGMA busy_timeout = 0")
            .execute(&mut conn)
            .await
            .unwrap();
        let probe = tokio::spawn(async move {
            let mut held_since = None;
            let mut maximum = Duration::ZERO;
            let mut busy_probes = 0u64;
            while !probe_stop.load(Ordering::Relaxed) {
                match sqlx::query("BEGIN IMMEDIATE").execute(&mut conn).await {
                    Ok(_) => {
                        sqlx::query("ROLLBACK").execute(&mut conn).await.unwrap();
                        if let Some(since) = held_since.take() {
                            maximum = maximum.max(Instant::now().duration_since(since));
                        }
                    }
                    Err(error) => {
                        let code = error.as_database_error().and_then(|e| e.code());
                        assert!(
                            matches!(code.as_deref(), Some("5" | "6" | "261" | "517")),
                            "{error}"
                        );
                        held_since.get_or_insert_with(Instant::now);
                        busy_probes += 1;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if let Some(since) = held_since {
                maximum = maximum.max(Instant::now().duration_since(since));
            }
            (maximum, busy_probes)
        });
        let start = Instant::now();
        let mut result = match std::env::var("KOMIQ_BENCH_JOB").as_deref() {
            Ok("browse") => refresh_browse_catalogue(&pool).await,
            Ok("fts") => refresh_work_fts(&pool).await,
            Ok("merged-feed") => refresh_feed_series_updates(&pool).await,
            _ => refresh_feed_updates(&pool).await,
        };
        while http.is_some() && start.elapsed() < Duration::from_secs(20) && result.is_ok() {
            result = refresh_feed_updates(&pool).await;
        }
        let elapsed = start.elapsed();
        stop.store(true, Ordering::Relaxed);
        let (maximum, busy_probes) = probe.await.unwrap();
        println!(
            "WRITER_BENCH elapsed_ms={} longest_hold_ms={} busy_probes={} result={result:?}",
            elapsed.as_millis(),
            maximum.as_millis(),
            busy_probes
        );
        result.unwrap();
        if let Some((server, children)) = http {
            for mut child in children {
                let status = tokio::task::spawn_blocking(move || child.wait().unwrap())
                    .await
                    .unwrap();
                assert!(status.success(), "loadtest failed: {status}");
            }
            server.abort();
        }
        println!(
            "HTTP_BENCH lock_error_events={}",
            lock_errors.load(Ordering::Relaxed)
        );
        pool.close().await;
    }
}
