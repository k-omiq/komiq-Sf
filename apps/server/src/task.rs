//! Shared supervision for background loops.
//!
//! Several loops (scanner, source-sync, mangadex) already hand-rolled a
//! panic-restart supervisor; three others (cover drainer, comment-media GC, metadata
//! enrichment) did not, so a single panic in any of them ended that job silently for
//! the process lifetime. `supervise` is the one implementation they can all share.

use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;

/// Run a background loop under panic supervision.
///
/// `factory` builds a fresh loop future on each (re)start — it must re-clone whatever
/// state the loop owns, because a panicked task's captures are gone. A clean return
/// (the loop saw the shutdown signal) ends supervision; a panic logs and restarts
/// after `restart_delay`, unless shutdown was requested in the meantime.
///
/// The loop future is `tokio::spawn`ed so a panic unwinds into a `JoinError` we can
/// catch here, rather than tearing down this supervising task too.
pub async fn supervise<F, Fut>(
    name: &'static str,
    restart_delay: Duration,
    shutdown: watch::Receiver<bool>,
    factory: F,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        match tokio::spawn(factory()).await {
            Ok(()) => break, // clean shutdown
            Err(e) if e.is_panic() => {
                if *shutdown.borrow() {
                    break;
                }
                tracing::error!(
                    loop_name = name,
                    restart_secs = restart_delay.as_secs(),
                    "background loop panicked; restarting"
                );
                tokio::time::sleep(restart_delay).await;
                if *shutdown.borrow() {
                    break;
                }
            }
            Err(_) => break, // cancelled
        }
    }
}
