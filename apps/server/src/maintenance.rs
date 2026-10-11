//! Automatic pending-review merges and installed-extension updates.

use std::{
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use crate::db::Database;
use tokio::sync::watch;

use crate::{graphql::AppState, suwayomi::SuwayomiClient};

/// Rows kept in `extension_update_log`. An update pass writes one row per extension it
/// touched, and extensions update a few times a month, so this is months of history.
const UPDATE_LOG_KEEP: i64 = 500;

/// What the extension updater last did — the admin answer to "is auto-update running?".
///
/// In memory on purpose: the first pass runs 45s after boot, so a restart costs under a
/// minute of "not run yet", and a pass that never ran since boot is exactly what the
/// panel should say. The per-extension history is in `extension_update_log`.
#[derive(Debug, Clone, Default)]
pub struct UpdateStatus {
    /// Seconds between automatic passes; 0 means automatic updates are off.
    pub interval_secs: u64,
    /// A pass is in flight right now.
    pub running: bool,
    /// When the last pass FINISHED, whatever its outcome.
    pub last_run_at: Option<String>,
    /// Why the last pass failed as a whole (the store index could not be refreshed or
    /// listed). `None` when it ran, even if single extensions failed.
    pub last_error: Option<String>,
    /// Extensions the last pass updated, and extensions it tried and could not.
    pub last_updated: u64,
    pub last_failed: u64,
}

static STATUS: LazyLock<Mutex<UpdateStatus>> = LazyLock::new(Default::default);
/// One pass at a time: the automatic loop and an admin's "update now" must not both
/// drive `updateExtension` for the same package.
static PASS: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);

fn status() -> std::sync::MutexGuard<'static, UpdateStatus> {
    STATUS.lock().unwrap_or_else(|p| p.into_inner())
}

/// The updater's current status.
pub fn update_status() -> UpdateStatus {
    status().clone()
}

/// One row of `extension_update_log`, newest first.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UpdateLogEntry {
    #[sqlx(try_from = "crate::db_types::DbString")]
    pub at: String,
    #[sqlx(try_from = "crate::db_types::DbString")]
    pub pkg_name: String,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub from_version: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub to_version: Option<String>,
    #[sqlx(try_from = "crate::db_types::DbString")]
    pub origin: String,
    #[sqlx(try_from = "crate::db_types::DbOptionalString")]
    pub error: Option<String>,
}

pub async fn recent_updates(pool: &Database, limit: i64) -> anyhow::Result<Vec<UpdateLogEntry>> {
    Ok(sqlx::query_as::<_, UpdateLogEntry>(
        "SELECT at, pkg_name, from_version, to_version, origin, error \
           FROM extension_update_log ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
tokio::task_local! { static UPDATE_LOG_PAUSE: crate::locks::pause_points::PausePoints; }

async fn log_attempts(
    pool: &Database,
    attempts: &[UpdateAttempt],
    origin: &str,
) -> anyhow::Result<()> {
    crate::db::retry_transaction(|| log_attempts_once(pool, attempts, origin)).await
}

async fn log_attempts_once(
    pool: &Database,
    attempts: &[UpdateAttempt],
    origin: &str,
) -> anyhow::Result<()> {
    if attempts.is_empty() {
        return Ok(());
    }
    let now = chrono::Utc::now();
    let mut tx = pool.begin().await?;
    for a in attempts {
        sqlx::query(
            "INSERT INTO extension_update_log (at, pkg_name, from_version, to_version, origin, error) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(now)
        .bind(&a.pkg_name)
        .bind(&a.from_version)
        .bind(&a.to_version)
        .bind(origin)
        .bind(&a.error)
        .execute(&mut *tx)
        .await?;
    }
    #[cfg(test)]
    if let Ok(points) = UPDATE_LOG_PAUSE.try_with(Clone::clone) {
        points.hit("update-log-appended").await;
    }
    sqlx::query(
        "DELETE FROM extension_update_log \
          WHERE id IN (SELECT id FROM extension_update_log ORDER BY at DESC, id DESC OFFSET $1)",
    )
    .bind(UPDATE_LOG_KEEP)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Run one update pass and record what it did: the in-memory status, one log row per
/// extension touched, and refreshed `source_extension` coordinates when anything changed.
/// `origin` is 'auto' or 'manual'. Returns immediately if a pass is already in flight.
pub async fn run_update_pass(state: &AppState, origin: &'static str) {
    let Ok(_pass) = PASS.try_lock() else {
        return;
    };
    status().running = true;
    let result = update_extensions(&state.suwayomi).await;
    let (updated, failed, error) = match &result {
        Ok(attempts) => {
            let failed = attempts.iter().filter(|a| a.error.is_some()).count() as u64;
            (attempts.len() as u64 - failed, failed, None)
        }
        Err(e) => (0, 0, Some(e.to_string())),
    };
    match &result {
        Ok(attempts) => {
            tracing::info!(updated, failed, origin, "extension update pass complete");
            if let Err(e) = log_attempts(&state.pool, attempts, origin).await {
                let e = crate::db::observed_database_error(e);
                tracing::warn!(error = %e, "extension update log write failed");
            }
            if updated > 0 {
                crate::scanner::record_source_extensions(state).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, origin, "extension update check failed; will retry"),
    }
    let mut s = status();
    s.running = false;
    s.last_run_at = Some(crate::db_types::format_date(chrono::Utc::now()));
    s.last_error = error;
    s.last_updated = updated;
    s.last_failed = failed;
}

pub fn spawn(state: Arc<AppState>, update_interval_secs: u64, shutdown: watch::Receiver<bool>) {
    status().interval_secs = update_interval_secs;
    let review_state = state.clone();
    let review_shutdown = shutdown.clone();
    tokio::spawn(async move {
        crate::task::supervise("pending-review-merges", Duration::from_secs(30), review_shutdown.clone(), move || {
            let state = review_state.clone();
            let mut shutdown = review_shutdown.clone();
            async move {
                if !wait(Duration::from_secs(30), &mut shutdown).await { return; }
                loop {
                    // A failed pass (the database stayed busy) is tried again soon, not an
                    // hour later: the queue it left behind is what admins see.
                    let pause = tokio::select! {
                        biased;
                        _ = shutdown.changed() => return,
                        result = crate::graphql::auto_merge_pending_reviews(&state.pool, Some(&state.cover_pool)) => {
                            match result {
                                Ok(merged) => {
                                    tracing::info!(merged, "waiting merge reviews rechecked");
                                    3600
                                }
                                Err(e) => {
                                    let e = crate::db::observed_database_error(e);
                                    tracing::warn!(error = %e, "waiting merge reviews failed; will retry");
                                    300
                                }
                            }
                        }
                    };
                    if !wait(Duration::from_secs(pause), &mut shutdown).await { return; }
                }
            }
        }).await;
    });
    if update_interval_secs == 0 {
        tracing::info!("automatic extension updates disabled");
        return;
    }
    tokio::spawn(async move {
        crate::task::supervise(
            "extension-auto-update",
            Duration::from_secs(30),
            shutdown.clone(),
            move || {
                let state = state.clone();
                let mut shutdown = shutdown.clone();
                async move {
                    if !wait(Duration::from_secs(45), &mut shutdown).await {
                        return;
                    }
                    loop {
                        tokio::select! {
                            biased;
                            _ = shutdown.changed() => return,
                            _ = run_update_pass(&state, "auto") => {}
                        }
                        if !wait(Duration::from_secs(update_interval_secs), &mut shutdown).await {
                            return;
                        }
                    }
                }
            },
        )
        .await;
    });
}

async fn wait(duration: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    tokio::select! {
        biased;
        _ = shutdown.changed() => false,
        _ = tokio::time::sleep(duration) => true,
    }
}

/// One extension an update pass touched. `error` is `None` when it is now up to date.
#[derive(Debug, Clone)]
pub struct UpdateAttempt {
    pub pkg_name: String,
    pub from_version: String,
    pub to_version: Option<String>,
    pub error: Option<String>,
}

/// Refresh before checking hasUpdate. One failure must not block other packages.
/// Returns every extension that had an update, with how its update went.
async fn update_extensions(client: &SuwayomiClient) -> anyhow::Result<Vec<UpdateAttempt>> {
    client.refresh_extensions().await?;
    let mut extensions = client.list_extensions().await?;
    extensions.sort_by(|a, b| a.pkg_name.cmp(&b.pkg_name));
    let mut attempts = Vec::new();
    for ext in extensions
        .into_iter()
        .filter(|e| e.is_installed && e.has_update)
    {
        let (to_version, error) = match client.update_extension(&ext.pkg_name).await {
            Ok(current) if current.is_installed && !current.has_update => {
                tracing::info!(pkg_name = %ext.pkg_name, from = %ext.version_name,
                    to = %current.version_name, "extension updated");
                (Some(current.version_name), None)
            }
            Ok(_) => {
                tracing::warn!(pkg_name = %ext.pkg_name, "extension still has an update; will retry");
                (None, Some("still has an update after updating".to_string()))
            }
            Err(e) => {
                let e = crate::db::observed_database_error(e);
                tracing::warn!(pkg_name = %ext.pkg_name, error = %e, "extension update failed; will retry");
                (None, Some(e.to_string()))
            }
        };
        attempts.push(UpdateAttempt {
            pkg_name: ext.pkg_name,
            from_version: ext.version_name,
            to_version,
            error,
        });
    }
    Ok(attempts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::sync::Mutex;

    #[tokio::test]
    async fn overlapping_extension_log_retention_keeps_ranked_rows_across_sequence_gaps() {
        let pool = crate::database::TestPoolOptions::new()
            .max_connections(8)
            .connect("sqlite::memory:")
            .await
            .unwrap()
            .foreground();
        sqlx::migrate!("./migrations_pg")
            .run(pool.write())
            .await
            .unwrap();
        sqlx::query("INSERT INTO extension_update_log (at, pkg_name, origin) SELECT '2020-01-01'::timestamptz, 'old-' || n, 'auto' FROM generate_series(1, 500) n").execute(&pool).await.unwrap();
        sqlx::query("SELECT setval(pg_get_serial_sequence('extension_update_log','id'),50000)")
            .execute(&pool)
            .await
            .unwrap();
        let points = crate::locks::pause_points::PausePoints::default();
        let armed = points.arm("update-log-appended");
        let a = pool.clone();
        let pause = points.clone();
        let first = tokio::spawn(UPDATE_LOG_PAUSE.scope(pause, async move {
            log_attempts(
                &a,
                &[UpdateAttempt {
                    pkg_name: "first".into(),
                    from_version: "1".into(),
                    to_version: Some("2".into()),
                    error: None,
                }],
                "manual",
            )
            .await
        }));
        tokio::time::timeout(Duration::from_secs(30), armed.reached())
            .await
            .unwrap();
        log_attempts(
            &pool,
            &[UpdateAttempt {
                pkg_name: "second".into(),
                from_version: "1".into(),
                to_version: Some("2".into()),
                error: None,
            }],
            "auto",
        )
        .await
        .unwrap();
        armed.release();
        first.await.unwrap().unwrap();
        let (all, fresh): (i64, i64) = sqlx::query_as("SELECT COUNT(*), COUNT(*) FILTER (WHERE pkg_name IN ('first','second')) FROM extension_update_log").fetch_one(&pool).await.unwrap();
        assert_eq!((all, fresh), (500, 2));
    }

    fn extension(pkg: &str, installed: bool, update: bool) -> Value {
        json!({"pkgName": pkg, "name": pkg, "lang": "en", "versionName": "1.2",
            "isInstalled": installed, "hasUpdate": update, "isNsfw": false})
    }

    #[tokio::test]
    async fn updates_installed_packages_after_refresh_and_continues_after_failure() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = calls.clone();
        let app = Router::new().route("/api/graphql", post(move |Json(body): Json<Value>| {
            let recorded = recorded.clone();
            async move {
                let query = body["query"].as_str().unwrap();
                if query.contains("fetchExtensions") {
                    recorded.lock().unwrap().push("refresh".into());
                    Json(json!({"data": {"fetchExtensions": {"extensions": []}}}))
                } else if query.contains("updateExtension") {
                    let pkg = body["variables"]["id"].as_str().unwrap();
                    assert_eq!(body["variables"]["patch"], json!({"update": true}));
                    recorded.lock().unwrap().push(pkg.into());
                    if pkg == "a-fails" {
                        Json(json!({"errors": [{"message": "download failed"}]}))
                    } else {
                        Json(json!({"data": {"updateExtension": {"extension": extension(pkg, true, false)}}}))
                    }
                } else {
                    recorded.lock().unwrap().push("list".into());
                    Json(json!({"data": {"extensions": {"nodes": [
                        extension("a-fails", true, true), extension("b-updates", true, true),
                        extension("fresh", true, false), extension("uninstalled", false, true)
                    ]}}}))
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = SuwayomiClient::new(url, None, None);
        let attempts = update_extensions(&client).await.unwrap();
        let outcome: Vec<_> = attempts
            .iter()
            .map(|a| {
                (
                    a.pkg_name.as_str(),
                    a.to_version.as_deref(),
                    a.error.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            outcome,
            [
                ("a-fails", None, Some("download failed")),
                ("b-updates", Some("1.2"), None)
            ]
        );
        assert_eq!(
            *calls.lock().unwrap(),
            ["refresh", "list", "a-fails", "b-updates"]
        );
        server.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn wait_stops_promptly_on_shutdown() {
        let (tx, mut rx) = watch::channel(false);
        tx.send(true).unwrap();
        assert!(!wait(Duration::from_secs(21600), &mut rx).await);
    }
}
