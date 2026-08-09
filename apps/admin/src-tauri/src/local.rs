//! The komiq server this app runs on this computer ("Run komiq on this computer").
//!
//! One supervision task does the whole start: the source engine first (the reader's
//! supervisor, see `mod suwayomi` in lib.rs), then the `komika-server` sidecar pointed at
//! it. Progress, the failure of a step, the server's address and its one-time setup code
//! are read by the start screen through `local_server_status`; both processes' output is
//! kept in memory for `local_server_log` (src/logbuf.rs).
//!
//! The server binds loopback unless the owner turned on "Allow other devices"
//! (`set_local_network`), which restarts it on all interfaces. Its port is remembered so
//! a reader bookmarked on a phone keeps working; when the port is taken the next free one
//! is used and remembered instead. The app's own data (database, covers) lives in the app
//! data dir. Nothing here ever writes the setup code anywhere but the status reply.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use crate::logbuf::{find_setup_code, LogBuf, MASK};

/// First port tried for a new install. Then the next few, then any free one.
pub const DEFAULT_PORT: u16 = 7580;
/// Ports tried after the preferred one before asking the OS for any free port.
const PORT_TRIES: u16 = 10;
/// Boot attempts when the chosen port turns out to be taken after all.
const PORT_ATTEMPTS: usize = 3;
/// How long the server may take to answer `/health` (first boot runs migrations).
pub const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for the setup-code line once the server says it needs setup.
const CODE_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(200);
/// Unexpected server exits tolerated per minute before the start is reported failed.
const MAX_RESTARTS_PER_MIN: usize = 3;
/// Origins the app's own window uses (WebKit/macOS, WebView2 http and https) and the
/// admin dev server, so the console can call the server it runs.
const APP_ORIGINS: &str =
  "tauri://localhost,http://tauri.localhost,https://tauri.localhost,http://localhost:5273";

/// The step the start is on. `Ready` once the server answers `/health`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
  Engine,
  Server,
  Ready,
}

impl Stage {
  fn as_str(self) -> &'static str {
    match self {
      Stage::Engine => "engine",
      Stage::Server => "server",
      Stage::Ready => "ready",
    }
  }
}

/// `local_server_status`, the `LocalServerStatus` of apps/admin/src/lib/desktop.ts.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Status {
  pub stage: &'static str,
  pub error: Option<String>,
  pub address: Option<String>,
  pub setup_code: Option<String>,
}

/// `local_network` / `set_local_network`, the `LocalNetwork` of desktop.ts.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Network {
  /// "Allow other devices" is on (the server listens on every interface).
  pub enabled: bool,
  pub port: Option<u16>,
  /// Reader addresses other devices can open, e.g. `http://192.168.1.20:7580`.
  pub addresses: Vec<String>,
}

/// Per-device choices, kept in `<app data>/desktop.json`.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
struct Prefs {
  port: Option<u16>,
  #[serde(default)]
  lan: bool,
}

/// Where the source engine is, as the server must be told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineLink {
  /// What the server calls, credential included (`http://user:pass@127.0.0.1:port`).
  pub url: String,
  /// The same origin without the credential (image URLs the server builds).
  pub public_url: String,
}

pub enum EngineState {
  Starting,
  Ready(EngineLink),
  Failed(String),
}

/// The engine as the start sequence sees it. The app's implementation wraps the reader's
/// supervisor (lib.rs); tests use a fake.
pub trait Engine: Send + Sync {
  /// Start it if it is not running yet. Idempotent.
  fn ensure_started(&self);
  fn state(&self) -> EngineState;
}

/// What a server start needs, independent of Tauri.
#[derive(Clone, Debug)]
pub struct Launch {
  pub bin: PathBuf,
  /// Folder with `reader/` and `admin/` (`KOMIKA_WEB_DIR`).
  pub web_dir: Option<PathBuf>,
  /// Database, covers, `desktop.json`.
  pub data_dir: PathBuf,
}

enum Cmd {
  /// Clear a reported failure and try the failed step again.
  Retry,
  /// Stop the server and start it again (new bind address).
  Restart,
  Stop,
}

struct State {
  stage: Stage,
  error: Option<String>,
  port: Option<u16>,
  /// Taken by the first status read after `Ready`; never stored anywhere else.
  setup_code: Option<String>,
  /// Bumped on every transition to `Ready`.
  ready_gen: u64,
  running: bool,
}

pub struct LocalServer {
  state: Mutex<State>,
  prefs: Mutex<Prefs>,
  prefs_path: Option<PathBuf>,
  launch: Launch,
  engine: Arc<dyn Engine>,
  pub engine_log: Arc<LogBuf>,
  pub server_log: Arc<LogBuf>,
  /// Filled by the output pump when the server prints its setup code.
  code_slot: Arc<Mutex<Option<String>>>,
  tx: Mutex<Option<mpsc::UnboundedSender<Cmd>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
  m.lock().unwrap_or_else(|e| e.into_inner())
}

impl LocalServer {
  /// `persist`: keep port and network choice in `<data_dir>/desktop.json`.
  pub fn new(
    launch: Launch,
    engine: Arc<dyn Engine>,
    engine_log: Arc<LogBuf>,
    persist: bool,
  ) -> Self {
    let prefs_path = persist.then(|| launch.data_dir.join("desktop.json"));
    let prefs = prefs_path
      .as_deref()
      .and_then(|p| std::fs::read(p).ok())
      .and_then(|bytes| serde_json::from_slice::<Prefs>(&bytes).ok())
      .unwrap_or_default();
    LocalServer {
      state: Mutex::new(State {
        stage: Stage::Engine,
        error: None,
        port: None,
        setup_code: None,
        ready_gen: 0,
        running: false,
      }),
      prefs: Mutex::new(prefs),
      prefs_path,
      launch,
      engine,
      engine_log,
      server_log: LogBuf::new(),
      code_slot: Arc::new(Mutex::new(None)),
      tx: Mutex::new(None),
    }
  }

  // ---- bridge ----------------------------------------------------------------

  /// `start_local_server`: begin the start, or retry a failed step. Returns at once.
  pub fn start(self: &Arc<Self>) {
    let mut tx = lock(&self.tx);
    if let Some(sender) = tx.as_ref() {
      if sender.send(Cmd::Retry).is_ok() {
        // Cleared here, not only when the task gets to it: the start screen polls right
        // after this returns and must not read the failure it is retrying.
        self.clear_error();
        return;
      }
    }
    let (sender, receiver) = mpsc::unbounded_channel();
    *tx = Some(sender);
    {
      let mut st = lock(&self.state);
      st.running = true;
      st.stage = Stage::Engine;
      st.error = None;
    }
    let this = self.clone();
    tauri::async_runtime::spawn(async move { this.run(receiver).await });
  }

  /// `local_server_status`. The setup code is handed out once, with the first `ready`.
  pub fn status(&self) -> Status {
    let mut st = lock(&self.state);
    let ready = st.stage == Stage::Ready;
    Status {
      stage: st.stage.as_str(),
      error: st.error.clone(),
      address: if ready {
        st.port.map(|p| format!("http://127.0.0.1:{p}"))
      } else {
        None
      },
      setup_code: if ready { st.setup_code.take() } else { None },
    }
  }

  /// `local_server_log`: both logs, engine first.
  pub fn log_text(&self) -> String {
    let section = |title: &str, buf: &LogBuf| {
      let text = buf.text();
      format!(
        "── {title} ──\n{}",
        if text.is_empty() {
          "(no output yet)".into()
        } else {
          text
        }
      )
    };
    format!(
      "{}\n\n{}",
      section("Source engine", &self.engine_log),
      section("Server", &self.server_log)
    )
  }

  /// `local_network`.
  pub fn network(&self) -> Network {
    let enabled = lock(&self.prefs).lan;
    let port = {
      let st = lock(&self.state);
      (st.stage == Stage::Ready).then_some(st.port).flatten()
    };
    let addresses = match (enabled, port) {
      (true, Some(port)) => crate::network::lan_ipv4()
        .into_iter()
        .map(|ip| format!("http://{ip}:{port}"))
        .collect(),
      _ => Vec::new(),
    };
    Network {
      enabled,
      port,
      addresses,
    }
  }

  /// `set_local_network`: remember the choice and, when the server runs, restart it on
  /// the new address. Resolves once it answers again, or with the reason it did not.
  pub async fn set_network(self: &Arc<Self>, enabled: bool) -> Result<Network, String> {
    {
      let mut prefs = lock(&self.prefs);
      prefs.lan = enabled;
      self.save_prefs(&prefs);
    }
    let gen = {
      let st = lock(&self.state);
      if !st.running {
        return Ok(self.network());
      }
      st.ready_gen
    };
    if let Some(tx) = lock(&self.tx).as_ref() {
      let _ = tx.send(Cmd::Restart);
    }
    let deadline = Instant::now() + SERVER_READY_TIMEOUT + Duration::from_secs(15);
    loop {
      tokio::time::sleep(POLL).await;
      {
        let st = lock(&self.state);
        if st.ready_gen > gen && st.stage == Stage::Ready {
          break;
        }
        if st.stage != Stage::Ready {
          if let Some(err) = &st.error {
            return Err(err.clone());
          }
        }
        if !st.running {
          return Err("The server stopped.".into());
        }
      }
      if Instant::now() > deadline {
        return Err("The server did not come back in time.".into());
      }
    }
    Ok(self.network())
  }

  /// Stop the server (app exit). Waits for the process to be gone.
  pub async fn stop(&self) {
    let sent = lock(&self.tx)
      .take()
      .is_some_and(|tx| tx.send(Cmd::Stop).is_ok());
    if !sent {
      return;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while lock(&self.state).running && Instant::now() < deadline {
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
  }

  // ---- state -----------------------------------------------------------------

  fn set_stage(&self, stage: Stage) {
    let mut st = lock(&self.state);
    st.stage = stage;
    st.error = None;
    if stage != Stage::Ready {
      st.port = None;
    }
  }

  fn fail(&self, error: String) {
    self.server_log.push(&format!("[komiq] {error}"));
    lock(&self.state).error = Some(error);
  }

  fn clear_error(&self) {
    lock(&self.state).error = None;
  }

  fn set_ready(&self, port: u16, setup_code: Option<String>) {
    let mut st = lock(&self.state);
    st.stage = Stage::Ready;
    st.error = None;
    st.port = Some(port);
    st.setup_code = setup_code;
    st.ready_gen += 1;
  }

  fn save_prefs(&self, prefs: &Prefs) {
    if let Some(path) = &self.prefs_path {
      if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
      }
      if let Ok(json) = serde_json::to_vec_pretty(prefs) {
        let _ = std::fs::write(path, json);
      }
    }
  }

  // ---- the start sequence ------------------------------------------------------

  async fn run(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let mut exits: VecDeque<Instant> = VecDeque::new();
    'start: loop {
      // 1. The source engine.
      self.set_stage(Stage::Engine);
      self.engine.ensure_started();
      let link = loop {
        match self.engine.state() {
          EngineState::Ready(link) => break link,
          EngineState::Failed(e) => {
            let mut st = lock(&self.state);
            if st.error.is_none() {
              st.error = Some(format!("The source engine did not start: {e}"));
            }
          }
          EngineState::Starting => {}
        }
        tokio::select! {
          cmd = rx.recv() => match cmd {
            Some(Cmd::Retry) | Some(Cmd::Restart) => self.clear_error(),
            Some(Cmd::Stop) | None => return self.finish(None).await,
          },
          _ = tokio::time::sleep(POLL) => {}
        }
      };

      // 2. The server.
      self.set_stage(Stage::Server);
      let (mut child, port) = match self.boot(&link).await {
        Ok(ok) => ok,
        Err(e) => {
          self.fail(e);
          match wait_for_retry(&mut rx).await {
            true => continue 'start,
            false => return self.finish(None).await,
          }
        }
      };
      let code = self.wait_for_code(port).await;
      self.set_ready(port, code);
      self
        .server_log
        .push(&format!("[komiq] server ready on port {port}"));

      // 3. Keep it running.
      loop {
        tokio::select! {
          status = child.wait() => {
            let what = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
            self.server_log.push(&format!("[komiq] the server stopped unexpectedly ({what})"));
            let now = Instant::now();
            exits.push_back(now);
            while exits.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
              exits.pop_front();
            }
            if exits.len() > MAX_RESTARTS_PER_MIN {
              exits.clear();
              self.set_stage(Stage::Server);
              self.fail("The server keeps stopping. The log has the cause.".into());
              if !wait_for_retry(&mut rx).await {
                return self.finish(None).await;
              }
            } else {
              tokio::time::sleep(Duration::from_secs(1)).await;
            }
            continue 'start;
          }
          cmd = rx.recv() => match cmd {
            Some(Cmd::Retry) => {}
            Some(Cmd::Restart) => {
              self.server_log.push("[komiq] restarting the server");
              crate::suwayomi::stop_child(&mut child).await;
              continue 'start;
            }
            Some(Cmd::Stop) | None => return self.finish(Some(child)).await,
          },
          _ = tokio::time::sleep(Duration::from_secs(2)) => {
            // The engine restarted on a new port: the server must follow it.
            if let EngineState::Ready(now) = self.engine.state() {
              if now != link {
                self.server_log.push("[komiq] the source engine moved; restarting the server");
                crate::suwayomi::stop_child(&mut child).await;
                continue 'start;
              }
            }
          }
        }
      }
    }
  }

  async fn finish(&self, child: Option<tokio::process::Child>) {
    if let Some(mut child) = child {
      crate::suwayomi::stop_child(&mut child).await;
    }
    let mut st = lock(&self.state);
    st.running = false;
    st.stage = Stage::Engine;
    st.port = None;
  }

  /// Start the server on the remembered port (or the next free one), retrying on another
  /// port when the chosen one was taken in between.
  async fn boot(&self, link: &EngineLink) -> Result<(tokio::process::Child, u16), String> {
    let (lan, preferred) = {
      let prefs = lock(&self.prefs);
      (prefs.lan, prefs.port.unwrap_or(DEFAULT_PORT))
    };
    let bind = if lan {
      IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
      IpAddr::V4(Ipv4Addr::LOCALHOST)
    };
    let mut avoid: Vec<u16> = Vec::new();
    let mut last = String::new();
    for _ in 0..PORT_ATTEMPTS {
      let port = pick_port(bind, preferred, &avoid)?;
      let metrics_port =
        free_loopback_port().map_err(|e| format!("No free port for metrics: {e}"))?;
      *lock(&self.code_slot) = None;
      let attempt = Attempt {
        bind,
        port,
        metrics_port,
        engine: link.clone(),
      };
      match boot_server(
        &self.launch,
        &attempt,
        &self.server_log,
        &self.code_slot,
        SERVER_READY_TIMEOUT,
      )
      .await
      {
        Ok(child) => {
          let mut prefs = lock(&self.prefs);
          if prefs.port != Some(port) {
            prefs.port = Some(port);
            self.save_prefs(&prefs);
          }
          return Ok((child, port));
        }
        Err(BootError::PortInUse) => {
          avoid.push(port);
          last = format!("Port {port} is already in use.");
          self
            .server_log
            .push(&format!("[komiq] {last} Trying another port."));
        }
        Err(BootError::Failed(e)) => return Err(e),
      }
    }
    Err(format!("{last} No free port was found for the server."))
  }

  /// A fresh server prints its setup code before it listens, but the output pump may
  /// lag the health check by a moment. Wait for it only when the server needs setup.
  async fn wait_for_code(&self, port: u16) -> Option<String> {
    if let Some(code) = lock(&self.code_slot).take() {
      return Some(code);
    }
    if !needs_setup(port).await {
      return None;
    }
    let deadline = Instant::now() + CODE_WAIT;
    while Instant::now() < deadline {
      if let Some(code) = lock(&self.code_slot).take() {
        return Some(code);
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
  }
}

/// `true` to retry, `false` to stop. A restart request counts as a retry.
async fn wait_for_retry(rx: &mut mpsc::UnboundedReceiver<Cmd>) -> bool {
  matches!(rx.recv().await, Some(Cmd::Retry) | Some(Cmd::Restart))
}

// ---- one server process ----------------------------------------------------------

/// The inputs of one boot attempt.
#[derive(Clone, Debug)]
pub struct Attempt {
  pub bind: IpAddr,
  pub port: u16,
  pub metrics_port: u16,
  pub engine: EngineLink,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BootError {
  PortInUse,
  Failed(String),
}

/// The server's environment for one attempt. Everything else keeps the server's defaults.
pub fn server_env(launch: &Launch, attempt: &Attempt) -> Vec<(&'static str, String)> {
  let db = launch.data_dir.join("komika.sqlite3");
  let mut env = vec![
    ("BIND_ADDR", attempt.bind.to_string()),
    ("PORT", attempt.port.to_string()),
    ("KOMIKA_METRICS_PORT", attempt.metrics_port.to_string()),
    ("DATABASE_URL", format!("sqlite://{}", db.display())),
    ("SUWAYOMI_URL", attempt.engine.url.clone()),
    ("SUWAYOMI_PUBLIC_URL", attempt.engine.public_url.clone()),
    ("CORS_ORIGINS", APP_ORIGINS.to_string()),
    (
      "EXT_ICONS_DIR",
      launch.data_dir.join("ext-icons").display().to_string(),
    ),
    // Plain output: the log view shows it as text.
    ("NO_COLOR", "1".to_string()),
  ];
  if let Some(web) = &launch.web_dir {
    env.push(("KOMIKA_WEB_DIR", web.display().to_string()));
  }
  env
}

/// Spawn the server, copy its output into `log` (catching the setup code into
/// `code_slot`), and wait until it answers `/health`, it exits, or `timeout` passes.
pub async fn boot_server(
  launch: &Launch,
  attempt: &Attempt,
  log: &Arc<LogBuf>,
  code_slot: &Arc<Mutex<Option<String>>>,
  timeout: Duration,
) -> Result<tokio::process::Child, BootError> {
  std::fs::create_dir_all(&launch.data_dir)
    .map_err(|e| BootError::Failed(format!("Could not create the data folder: {e}")))?;
  log.push(&format!(
    "[komiq] starting the server on {}:{}",
    attempt.bind, attempt.port
  ));
  let mut cmd = tokio::process::Command::new(&launch.bin);
  cmd
    .envs(server_env(launch, attempt))
    .current_dir(&launch.data_dir)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
  #[cfg(windows)]
  {
    // CREATE_NO_WINDOW: the app is a GUI program; no console window for its server.
    cmd.creation_flags(0x0800_0000);
  }
  let mut child = cmd.spawn().map_err(|e| {
    BootError::Failed(format!(
      "Could not run the server ({}): {e}",
      launch.bin.display()
    ))
  })?;

  let mut pumps = Vec::new();
  if let Some(out) = child.stdout.take() {
    pumps.push(pump(out, log.clone(), code_slot.clone()));
  }
  if let Some(err) = child.stderr.take() {
    pumps.push(pump(err, log.clone(), code_slot.clone()));
  }

  let outcome = tokio::select! {
    biased;
    status = child.wait() => {
      // Let the pumps drain so the cause is in the log before reading it.
      for p in pumps {
        let _ = tokio::time::timeout(Duration::from_secs(1), p).await;
      }
      let status = status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
      if port_in_use(log) {
        Err(BootError::PortInUse)
      } else {
        Err(BootError::Failed(format!(
          "The server stopped while starting ({status}). The log has the cause."
        )))
      }
    }
    ok = wait_healthy(attempt.port, timeout) => {
      if ok {
        Ok(())
      } else {
        Err(BootError::Failed(format!(
          "The server did not answer within {} seconds.",
          timeout.as_secs()
        )))
      }
    }
  };
  match outcome {
    Ok(()) => Ok(child),
    Err(e) => {
      crate::suwayomi::stop_child(&mut child).await;
      Err(e)
    }
  }
}

fn pump<R>(
  reader: R,
  log: Arc<LogBuf>,
  code_slot: Arc<Mutex<Option<String>>>,
) -> tokio::task::JoinHandle<()>
where
  R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
  tokio::spawn(async move {
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
      match find_setup_code(&crate::logbuf::strip_ansi(&line)) {
        Some(code) => {
          log.push(&crate::logbuf::strip_ansi(&line).replace(&code, MASK));
          *lock(&code_slot) = Some(code);
        }
        None => log.push(&line),
      }
    }
  })
}

/// How each OS words a taken port (Linux, macOS, Windows), as the server reports it.
fn port_in_use(log: &LogBuf) -> bool {
  [
    "Address already in use",
    "address already in use",
    "os error 98)",
    "os error 48)",
    "os error 10048)",
  ]
  .iter()
  .any(|needle| log.contains(needle))
}

fn http() -> reqwest::Client {
  reqwest::Client::builder()
    .timeout(Duration::from_secs(3))
    .build()
    .unwrap_or_default()
}

/// Poll `GET /health` until it answers `ok` or `timeout` passes.
pub async fn wait_healthy(port: u16, timeout: Duration) -> bool {
  let client = http();
  let url = format!("http://127.0.0.1:{port}/health");
  let deadline = Instant::now() + timeout;
  loop {
    if let Ok(resp) = client.get(&url).send().await {
      if resp.status().is_success() && resp.text().await.is_ok_and(|b| b.trim() == "ok") {
        return true;
      }
    }
    if Instant::now() >= deadline {
      return false;
    }
    tokio::time::sleep(POLL).await;
  }
}

/// Whether the server still waits for its owner (`instance { setupRequired }`).
async fn needs_setup(port: u16) -> bool {
  let body = serde_json::json!({ "query": "{ instance { setupRequired } }" }).to_string();
  let Ok(resp) = http()
    .post(format!("http://127.0.0.1:{port}/graphql"))
    .header(reqwest::header::CONTENT_TYPE, "application/json")
    .body(body)
    .send()
    .await
  else {
    return false;
  };
  resp
    .bytes()
    .await
    .ok()
    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    .and_then(|v| {
      v.pointer("/data/instance/setupRequired")
        .and_then(|b| b.as_bool())
    })
    .unwrap_or(false)
}

// ---- ports -------------------------------------------------------------------------

fn port_free(bind: IpAddr, port: u16) -> bool {
  std::net::TcpListener::bind((bind, port)).is_ok()
}

/// `preferred`, else one of the next few, else any free port, skipping `avoid`.
pub fn pick_port(bind: IpAddr, preferred: u16, avoid: &[u16]) -> Result<u16, String> {
  for offset in 0..=PORT_TRIES {
    let Some(port) = preferred.checked_add(offset) else {
      break;
    };
    if !avoid.contains(&port) && port_free(bind, port) {
      return Ok(port);
    }
  }
  std::net::TcpListener::bind((bind, 0))
    .and_then(|l| l.local_addr())
    .map(|a| a.port())
    .map_err(|e| format!("No free port on {bind}: {e}"))
}

fn free_loopback_port() -> std::io::Result<u16> {
  let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
  listener.local_addr().map(|a| a.port())
}

/// The server binary next to the app's executable (Tauri puts `externalBin` sidecars
/// there), or `KOMIKA_SERVER_BIN` for development. A missing binary is reported by the
/// start itself ("Could not run the server …"), where the person can see it.
pub fn server_bin() -> PathBuf {
  if let Some(p) = std::env::var_os("KOMIKA_SERVER_BIN").filter(|p| !p.is_empty()) {
    return PathBuf::from(p);
  }
  let name = if cfg!(windows) {
    "komika-server.exe"
  } else {
    "komika-server"
  };
  let dir = std::env::current_exe()
    .ok()
    .and_then(|exe| exe.parent().map(Path::to_path_buf))
    .unwrap_or_default();
  dir.join(name)
}

#[cfg(test)]
mod tests;
