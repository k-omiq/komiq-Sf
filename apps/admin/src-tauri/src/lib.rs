//! komiq desktop app: the admin console (`ADMIN_TARGET=desktop` build) in a window, plus
//! the native side of its bridge (apps/admin/src/lib/desktop.ts): it runs the source
//! engine and the komiq server on this computer for "Run komiq on this computer".

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use tauri::{Manager, State};

mod firewall;
pub mod import;
pub mod local;
pub mod logbuf;
mod network;
pub mod postgres;

// The engine supervisor is the reader's own (apps/reader/src-tauri/src/suwayomi.rs),
// compiled into this crate rather than copied: same jar pin, JRE layout, server.conf,
// Basic-auth credential, lockfile and restart policy in both apps. It reaches two items
// of its host crate by path, `crate::cloudflare` (the reader's on-device Cloudflare shim,
// also shared, below) and `crate::image_fetch_semaphore` (below). Its Tauri commands are
// not registered here, hence `dead_code`; it keeps the reader's formatting, hence
// `rustfmt::skip`.
#[path = "../../../reader/src-tauri/src/suwayomi.rs"]
#[allow(dead_code)]
#[rustfmt::skip]
mod suwayomi;

// The reader's loopback FlareSolverr-v1 shim and WebView solver: the supervisor starts it
// with the engine and points the engine at it, so Cloudflare-protected sources solve
// their challenge in a hidden window of this app, exactly as in the reader.
#[path = "../../../reader/src-tauri/src/cloudflare.rs"]
#[rustfmt::skip]
mod cloudflare;

mod child;

/// Required by the shared supervisor's `suwayomi_image` command, which this app does not
/// register (the console never reads engine images directly).
#[allow(dead_code)]
pub(crate) fn image_fetch_semaphore() -> &'static tokio::sync::Semaphore {
  static SEM: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
  SEM.get_or_init(|| tokio::sync::Semaphore::new(6))
}

/// The reader's engine supervisor as the start sequence sees it.
struct BundledEngine {
  app: tauri::AppHandle,
  sup: Arc<suwayomi::SuwayomiSupervisor>,
  started: Arc<AtomicBool>,
}

/// Whether the engine was ever started this run (the exit hook only stops a started one:
/// `stop_and_wait` waits for a `stopped` that only a running supervision loop reports).
struct EngineStarted(Arc<AtomicBool>);

impl local::Engine for BundledEngine {
  fn ensure_started(&self) {
    // `suwayomi::start` spawns a supervision loop per call: claim it once per app run.
    // That loop restarts a crashed engine by itself. A start that launched no loop (the
    // bundled engine could not be resolved) gives the claim back, so Retry starts again.
    if self
      .started
      .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
      .is_ok()
      && !suwayomi::start(self.app.clone())
    {
      self.started.store(false, Ordering::SeqCst);
    }
  }

  fn stop(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
    Box::pin(async move {
      if self.started.load(Ordering::SeqCst) {
        self.sup.stop_and_wait().await;
      }
    })
  }

  fn state(&self) -> local::EngineState {
    let status = self.sup.status();
    match (status.state.as_str(), self.sup.base_url()) {
      ("ready", Some(base)) => local::EngineState::Ready(local::EngineLink {
        url: base.replacen(
          "http://",
          &format!(
            "http://{}:{}@",
            suwayomi::ENGINE_AUTH_USER,
            suwayomi::engine_password()
          ),
          1,
        ),
        public_url: base,
      }),
      ("degraded", _) | ("stopped", _) => local::EngineState::Failed(
        status
          .last_error
          .unwrap_or_else(|| "the engine stopped".into()),
      ),
      _ => local::EngineState::Starting,
    }
  }
}

/// `KOMIKA_DESKTOP_ENGINE=off` (development only): run the server without an engine.
/// It starts and serves the console; sources are unavailable.
struct NoEngine;

impl local::Engine for NoEngine {
  fn ensure_started(&self) {}

  fn state(&self) -> local::EngineState {
    local::EngineState::Ready(local::EngineLink {
      url: "http://127.0.0.1:1".into(),
      public_url: "http://127.0.0.1:1".into(),
    })
  }
}

/// The built reader and console the server serves: `KOMIKA_WEB_DIR` in development, else
/// the bundled `web/` resource (filled by scripts/prepare-bundle.sh).
fn web_dir(app: &tauri::App) -> Option<PathBuf> {
  if let Some(dir) = std::env::var_os("KOMIKA_WEB_DIR").filter(|d| !d.is_empty()) {
    return Some(PathBuf::from(dir));
  }
  let dir = app.path().resource_dir().ok()?.join("web");
  (dir.join("reader").is_dir() || dir.join("admin").is_dir()).then_some(dir)
}

// ---- bridge commands (contract: apps/admin/src/lib/desktop.ts) ----------------------

#[tauri::command]
fn start_local_server(server: State<'_, Arc<local::LocalServer>>) {
  server.start();
}

#[tauri::command]
fn local_server_status(server: State<'_, Arc<local::LocalServer>>) -> local::Status {
  server.status()
}

#[tauri::command]
fn local_server_log(server: State<'_, Arc<local::LocalServer>>) -> String {
  server.log_text()
}

#[tauri::command]
fn local_network(server: State<'_, Arc<local::LocalServer>>) -> local::Network {
  server.network()
}

#[tauri::command]
async fn set_local_network(
  server: State<'_, Arc<local::LocalServer>>,
  enabled: bool,
) -> Result<local::Network, String> {
  let server = server.inner().clone();
  server.set_network(enabled).await
}

#[tauri::command]
fn firewall_info() -> firewall::Info {
  firewall::info()
}

#[tauri::command]
async fn open_firewall(server: State<'_, Arc<local::LocalServer>>) -> Result<String, String> {
  let port = server
    .firewall_port()
    .ok_or_else(|| "Turn on Allow other devices first.".to_string())?;
  firewall::open(port).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  let engine_log = logbuf::LogBuf::new();
  // The server's startup config dump includes its engine URL; never show the credential.
  engine_log.add_secret(suwayomi::engine_password());
  let logger: &'static logbuf::Logger = Box::leak(Box::new(logbuf::Logger {
    engine: engine_log.clone(),
  }));
  if log::set_logger(logger).is_ok() {
    log::set_max_level(log::LevelFilter::Info);
  }

  let app = tauri::Builder::default()
    // Links that open a new tab go to the system browser (see Cargo.toml).
    .plugin(tauri_plugin_opener::init())
    .setup(move |app| {
      let data_dir = app.path().app_data_dir()?;
      // Children die with the app, even a force-killed one (see child.rs).
      // The local supervisor acquires the app-data OS lock before child::init.
      suwayomi::set_child_hooks(suwayomi::ChildHooks {
        before_spawn: child::before_spawn,
        after_spawn: child::after_spawn,
      });
      let supervisor = Arc::new(suwayomi::SuwayomiSupervisor::new());
      let started = Arc::new(AtomicBool::new(false));
      app.manage(supervisor.clone());
      app.manage(EngineStarted(started.clone()));
      let engine: Arc<dyn local::Engine> = match std::env::var("KOMIKA_DESKTOP_ENGINE").as_deref() {
        Ok("off") => Arc::new(NoEngine),
        _ => Arc::new(BundledEngine {
          app: app.handle().clone(),
          sup: supervisor,
          started,
        }),
      };
      let launch = local::Launch {
        bin: local::server_bin(),
        web_dir: web_dir(app),
        data_dir: data_dir.join("server"),
        postgres_runtime: Some(postgres::resolve(&app.path().resource_dir()?)),
      };
      let server = local::LocalServer::new(launch, engine, engine_log.clone(), true);
      server.server_log.add_secret(suwayomi::engine_password());
      app.manage(Arc::new(server));
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      start_local_server,
      local_server_status,
      local_server_log,
      local_network,
      set_local_network,
      firewall_info,
      open_firewall
    ])
    .build(tauri::generate_context!())
    .expect("error while building tauri application");

  // Quit: stop dumps and server, then engine, then database. block_on is
  // safe here, the run callback is on the main thread. kill_on_drop on children is
  // the backstop; child.rs covers a force-kill, which never gets here.
  app.run(|handle, event| {
    if let tauri::RunEvent::Exit = event {
      let server = handle
        .try_state::<Arc<local::LocalServer>>()
        .map(|s| s.inner().clone());
      let started = handle
        .try_state::<EngineStarted>()
        .is_some_and(|s| s.0.load(Ordering::SeqCst));
      let engine = handle
        .try_state::<Arc<suwayomi::SuwayomiSupervisor>>()
        .filter(|_| started)
        .map(|s| s.inner().clone());
      tauri::async_runtime::block_on(async move {
        if let Some(server) = server {
          server.stop().await;
        }
        if let Some(engine) = engine {
          engine.stop_and_wait().await;
        }
        child::forget_all();
      });
    }
  });
}
