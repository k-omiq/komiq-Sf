//! Mobile stubs for the embedded Suwayomi engine — Android and the iOS SIMULATOR.
//!
//! The desktop engine (`suwayomi.rs`) is a child-process JVM supervisor: it spawns a
//! `java` sidecar, brokers a loopback port, and runs a WebView Cloudflare shim
//! (`cloudflare.rs`). None of that applies on mobile — there is no `java` to spawn and
//! no child-process model on iOS — so on those targets the whole desktop implementation
//! is compiled out (`#[cfg(desktop)]`) and this module takes its place under the same
//! `suwayomi` module name (see `lib.rs`'s `#[path]` alias).
//!
//! REAL iOS devices no longer use this stub: they boot an in-process Zero JVM
//! (`suwayomi_ios.rs`, N4.2). The stub remains for Android (in-process JVM is a later
//! phase there) and for the iOS SIMULATOR, whose builds must keep compiling+linking
//! even though the vendored static JVM has no sim slice. These stubs keep the
//! four IPC commands REGISTERED (the JS side always invokes them by name), but they
//! report the engine as unavailable so the JS fallback ladder routes to the hosted
//! backend rather than hanging or crashing:
//!   * `suwayomi_status`   → `state: "degraded"` (mirrors the desktop "engine
//!                            unavailable" transition). JS `isReady()` returns false, so
//!                            `CompositeBackend.localReady()` is false and content stays
//!                            hosted.
//!   * `suwayomi_base_url` → `None` (no loopback engine to point at).
//!   * `suwayomi_gql` / `suwayomi_image` → a plain `Err(..)` string (never a panic); the
//!                            JS `.catch` paths fall back to hosted. In practice these are
//!                            unreachable while `isReady()` is false, but they must exist.
//!
//! Startup (`start`) is a NO-OP: no java spawn, no restart loop, no cloudflare shim, no
//! background threads. `SuwayomiSupervisor` is an empty placeholder kept only so `lib.rs`
//! can `manage` it and the shutdown hook can call `stop_and_wait` uniformly.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, State};

/// Mirror of the desktop `Status` shape so `suwayomi_status` returns identical JSON on
/// both platforms. JS reads `state`; anything other than `"ready"` is treated as not
/// usable (`isReady()` → false → composite falls back to hosted).
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Status {
  pub state: String,
  pub version: Option<String>,
  pub last_error: Option<String>,
}

/// Placeholder supervisor. Holds no state and owns no process — the mobile engine is a
/// later phase. Kept so `lib.rs` can `manage(Arc::new(..))` it and the exit hook can call
/// `stop_and_wait` on it, exactly as on desktop.
pub struct SuwayomiSupervisor;

impl SuwayomiSupervisor {
  pub fn new() -> Self {
    SuwayomiSupervisor
  }

  /// Nothing to reap on mobile — no child process was ever spawned.
  pub async fn stop_and_wait(&self) {}
}

impl Default for SuwayomiSupervisor {
  fn default() -> Self {
    Self::new()
  }
}

/// No-op engine startup. Mobile has no JVM sidecar to launch and no Cloudflare shim, so
/// this deliberately does nothing — no spawn, no restart loop, no background threads.
pub fn start(_app: AppHandle) {
  log::info!(target: "suwayomi", "embedded engine disabled on this platform; using hosted backend");
}

// ---- JS-facing commands (unavailable variants) -----------------------------

/// Always `None` on mobile: there is no loopback engine to address.
#[tauri::command]
pub fn suwayomi_base_url(_state: State<'_, Arc<SuwayomiSupervisor>>) -> Option<String> {
  None
}

/// Report a non-ready state (`degraded`) so JS `isReady()` returns false and the
/// composite backend serves content from the hosted backend. Matches the desktop
/// "engine unavailable" shape (`{ state, version: null, lastError }`).
#[tauri::command]
pub fn suwayomi_status(_state: State<'_, Arc<SuwayomiSupervisor>>) -> Status {
  Status {
    state: "degraded".to_string(),
    version: None,
    last_error: Some("embedded engine unavailable on this platform".to_string()),
  }
}

/// The engine is never ready on mobile, so this always errs (a clean string, no panic).
/// Unreachable while `isReady()` is false; present so the command stays registered.
#[tauri::command]
pub async fn suwayomi_gql(
  _state: State<'_, Arc<SuwayomiSupervisor>>,
  _query: String,
  _variables: Option<Value>,
) -> Result<Value, String> {
  Err("suwayomi engine not available on this platform".to_string())
}

/// Image counterpart to [`suwayomi_gql`]: always errs on mobile so JS falls back to the
/// hosted proxy. Unreachable while `isReady()` is false; present so it stays registered.
#[tauri::command]
pub async fn suwayomi_image(
  _state: State<'_, Arc<SuwayomiSupervisor>>,
  _path: String,
) -> Result<tauri::ipc::Response, String> {
  Err("suwayomi engine not available on this platform".to_string())
}
