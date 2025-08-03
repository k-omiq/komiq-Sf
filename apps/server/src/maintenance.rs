//! Automatic pending-review merges and installed-extension updates.

use std::{sync::Arc, time::Duration};

use tokio::sync::watch;

use crate::{graphql::AppState, suwayomi::SuwayomiClient};

pub fn spawn(state: Arc<AppState>, update_interval_secs: u64, shutdown: watch::Receiver<bool>) {
    let review_state = state.clone();
    let review_shutdown = shutdown.clone();
    tokio::spawn(async move {
        crate::task::supervise("pending-review-merges", Duration::from_secs(30), review_shutdown.clone(), move || {
            let state = review_state.clone();
            let mut shutdown = review_shutdown.clone();
            async move {
                if !wait(Duration::from_secs(30), &mut shutdown).await { return; }
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown.changed() => return,
                        result = crate::graphql::auto_merge_pending_reviews(&state.pool, Some(&state.cover_pool)) => {
                            match result {
                                Ok(merged) => tracing::info!(merged, "waiting merge reviews rechecked"),
                                Err(e) => tracing::warn!(error = %e, "waiting merge reviews failed; will retry"),
                            }
                        }
                    }
                    if !wait(Duration::from_secs(3600), &mut shutdown).await { return; }
                }
            }
        }).await;
    });
    if update_interval_secs == 0 {
        tracing::info!("automatic extension updates disabled");
        return;
    }
    tokio::spawn(async move {
        crate::task::supervise("extension-auto-update", Duration::from_secs(30), shutdown.clone(), move || {
            let state = state.clone();
            let mut shutdown = shutdown.clone();
            async move {
                if !wait(Duration::from_secs(45), &mut shutdown).await { return; }
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown.changed() => return,
                        result = update_extensions(&state.suwayomi) => {
                            match result {
                                Ok(updated) => {
                                    tracing::info!(updated, "automatic extension update pass complete");
                                    if updated > 0 {
                                        crate::scanner::record_source_extensions(&state).await;
                                    }
                                }
                                Err(e) => tracing::warn!(error = %e, "extension update check failed; will retry"),
                            }
                        }
                    }
                    if !wait(Duration::from_secs(update_interval_secs), &mut shutdown).await { return; }
                }
            }
        }).await;
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

/// Refresh before checking hasUpdate. One failure must not block other packages.
async fn update_extensions(client: &SuwayomiClient) -> anyhow::Result<u64> {
    client.refresh_extensions().await?;
    let mut extensions = client.list_extensions().await?;
    extensions.sort_by(|a, b| a.pkg_name.cmp(&b.pkg_name));
    let mut updated = 0;
    for ext in extensions
        .into_iter()
        .filter(|e| e.is_installed && e.has_update)
    {
        match client.update_extension(&ext.pkg_name).await {
            Ok(current) if current.is_installed && !current.has_update => {
                updated += 1;
                tracing::info!(pkg_name = %ext.pkg_name, from = %ext.version_name,
                    to = %current.version_name, "extension automatically updated");
            }
            Ok(_) => {
                tracing::warn!(pkg_name = %ext.pkg_name, "extension still has an update; will retry")
            }
            Err(e) => {
                tracing::warn!(pkg_name = %ext.pkg_name, error = %e, "extension update failed; will retry")
            }
        }
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::sync::Mutex;

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
        assert_eq!(update_extensions(&client).await.unwrap(), 1);
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
