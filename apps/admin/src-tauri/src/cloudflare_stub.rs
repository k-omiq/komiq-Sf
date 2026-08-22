//! Stand-in for the reader's on-device Cloudflare shim (`apps/reader/src-tauri/src/
//! cloudflare.rs`), which the shared engine supervisor calls by path
//! (`crate::cloudflare::…`). The desktop app does not run the shim yet: the engine starts
//! without a FlareSolverr endpoint, exactly as the reader's engine does when its shim
//! fails to start, and Cloudflare-protected sources fail until it is wired in.

use std::sync::Arc;

/// Never constructed here; it only gives `start` the reader's return type.
pub struct CfShim;

impl CfShim {
  pub fn url(&self) -> String {
    String::new()
  }
}

/// The reader starts its loopback shim here. No shim: the supervisor skips the wiring.
pub fn start(_app: &tauri::AppHandle) -> Option<Arc<CfShim>> {
  None
}

/// Only reached when `start` returned a shim, which it never does.
pub async fn apply_settings(_client: reqwest::Client, _port: u16, _shim_url: String) {}
