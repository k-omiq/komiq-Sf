//! Embedded Suwayomi-Server sidecar supervisor (native desktop plan §3.2/§3.3, item N1.1).
//!
//! This module owns the lifecycle of an embedded Suwayomi-Server JVM child process:
//! it brokers a loopback port, writes the authoritative `server.conf`, launches the
//! jar headless, gates on a GraphQL readiness probe, supervises crashes with capped
//! backoff, and exposes three `#[tauri::command]`s to the JS side.
//!
//! Security posture (deliberate, and different from `fetch_image`'s SSRF guard):
//! the engine binds **127.0.0.1 only**, and JS never fetches the loopback port
//! directly — it calls `suwayomi_gql`, which proxies over IPC. Loopback is therefore
//! *allowed* here (the opposite of the image path), so this module uses its own plain
//! `reqwest::Client`, NOT `image_client()`. Loopback is not an authorization boundary
//! on a shared desktop, so the engine additionally runs behind HTTP Basic auth with a
//! per-run credential (see [`engine_password`]); every request this module makes
//! carries it as a client default header.
//!
//! Nothing here runs unless the app spawns it; it does not touch `fetch_image` or any
//! existing behavior. The boot recipe follows `suwayomi/SPIKE-FINDINGS.md` exactly
//! (pin: Suwayomi-Server v2.3.2243).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};
use tokio::io::AsyncBufReadExt;

/// Cold-start budget for the readiness gate (spike: ~5 s warm; recipe allows ~30 s).
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How often to poll `aboutServer` while waiting for readiness.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Per-request timeout for the readiness poll and the `suwayomi_gql` proxy.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on an engine image body. Kept in lockstep with lib.rs's
/// `MAX_IMAGE_BYTES` (32 MiB) so `suwayomi_image` and `fetch_image` bound bytes
/// identically; `read_capped` lives in lib.rs and is private, so we mirror its
/// value + streaming logic here rather than reach across modules.
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024; // 32 MiB
/// Restart-storm cap: more than this many restarts inside 60 s trips a long backoff.
const MAX_RESTARTS_PER_MIN: usize = 5;
/// Grace period to let a signalled JVM reap before we force it again.
const GRACE: Duration = Duration::from_secs(3);
/// How often the lock holder rewrites its lockfile (the heartbeat that proves liveness).
const LOCK_HEARTBEAT: Duration = Duration::from_secs(15);
/// A lockfile whose heartbeat has been silent this long is treated as abandoned. Kept
/// several beats wide so a suspended/resumed machine cannot look dead to a racing launch.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(60);
/// How often a refused engine lock is retried before the engine can start.
const LOCK_RETRY: Duration = Duration::from_secs(5);
/// Basic-auth username for the embedded engine; the password is the per-run secret.
const ENGINE_AUTH_USER: &str = "komika";

/// Lifecycle state, surfaced to JS as a lowercase string via `suwayomi_status`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EngineState {
  Starting,
  Ready,
  Degraded,
  Stopped,
}

impl EngineState {
  fn as_str(self) -> &'static str {
    match self {
      EngineState::Starting => "starting",
      EngineState::Ready => "ready",
      EngineState::Degraded => "degraded",
      EngineState::Stopped => "stopped",
    }
  }
}

/// JSON shape returned by `suwayomi_status`.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Status {
  pub state: String,
  pub version: Option<String>,
  pub last_error: Option<String>,
}

/// Resolved launch inputs. Kept AppHandle-free so the boot path is unit/integration
/// testable without a running Tauri app.
#[derive(Clone, Debug)]
struct EngineConfig {
  java: PathBuf,
  jar: PathBuf,
  data_dir: PathBuf,
}

/// Mutable state behind a single lock. The child process itself lives inside the
/// supervision task (so it can be `select!`ed on `.wait()`); it is NOT stored here.
struct Inner {
  port: Option<u16>,
  state: EngineState,
  version: Option<String>,
  last_error: Option<String>,
}

/// Best-effort single-instance lockfile guard; removes the file on drop. The handle is
/// kept open so [`LockGuard::refresh`] can beat the heartbeat without reopening the path.
struct LockGuard {
  path: PathBuf,
  file: std::fs::File,
}

impl LockGuard {
  /// Rewrite the holder PID to push the lockfile's mtime forward. Drop only runs on a
  /// clean exit, so this advancing mtime is the *only* signal a later launch has that the
  /// holder is still alive — see `lock_is_abandoned`.
  fn refresh(&mut self) {
    use std::io::{Seek, SeekFrom, Write};
    let _ = self.file.seek(SeekFrom::Start(0));
    let _ = write!(self.file, "{}", std::process::id());
    let _ = self.file.flush();
  }
}

impl Drop for LockGuard {
  fn drop(&mut self) {
    let _ = std::fs::remove_file(&self.path);
  }
}

/// The engine's Basic-auth password, generated once per app run. The conf is rewritten
/// with it before every (re)launch and the supervisor's client sends it on every request,
/// so an in-run engine restart keeps working while a credential never outlives the process.
fn engine_password() -> &'static str {
  static PASSWORD: OnceLock<String> = OnceLock::new();
  PASSWORD.get_or_init(random_token)
}

/// 128 bits of process-local entropy as hex, without adding an RNG dependency for one
/// loopback credential: `RandomState`'s SipHash keys are seeded from the OS the first
/// time the process uses them and are not observable from another process.
fn random_token() -> String {
  use std::collections::hash_map::RandomState;
  use std::hash::{BuildHasher, Hasher};
  let nonce = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_nanos() as u64)
    .unwrap_or_default()
    ^ u64::from(std::process::id());
  let mut token = String::with_capacity(32);
  for round in 0..2u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(nonce ^ round);
    token.push_str(&format!("{:016x}", hasher.finish()));
  }
  token
}

/// Render an `Authorization: Basic …` value. Base64 is inlined rather than pulled in as a
/// dependency: this is the only encoder the crate needs and the input shape is fixed.
fn basic_auth_value(user: &str, password: &str) -> String {
  const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let raw = format!("{user}:{password}");
  let bytes = raw.as_bytes();
  let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
  for chunk in bytes.chunks(3) {
    let triple = ((chunk[0] as u32) << 16)
      | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
      | (*chunk.get(2).unwrap_or(&0) as u32);
    encoded.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
    encoded.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
    // The trailing group is padded, not truncated: 1 byte → "xx==", 2 bytes → "xxx=".
    encoded.push(if chunk.len() > 1 {
      ALPHABET[((triple >> 6) & 0x3f) as usize] as char
    } else {
      '='
    });
    encoded.push(if chunk.len() > 2 {
      ALPHABET[(triple & 0x3f) as usize] as char
    } else {
      '='
    });
  }
  format!("Basic {encoded}")
}

/// Build the supervisor's HTTP client. The engine credential rides as a default header so
/// every caller of this client — the readiness poll, the `suwayomi_gql`/`suwayomi_image`
/// proxies, and `cloudflare::apply_settings` (handed a clone) — is authenticated without
/// the secret being threaded through each call site.
fn build_client() -> reqwest::Client {
  let mut auth =
    reqwest::header::HeaderValue::from_str(&basic_auth_value(ENGINE_AUTH_USER, engine_password()))
      .expect("basic auth header is ascii");
  auth.set_sensitive(true);
  let mut headers = reqwest::header::HeaderMap::new();
  headers.insert(reqwest::header::AUTHORIZATION, auth);
  reqwest::Client::builder()
    .timeout(HTTP_TIMEOUT)
    .default_headers(headers)
    // No redirect policy / SSRF host guard here on purpose: loopback is allowed.
    .build()
    .expect("build suwayomi http client")
}

/// Owns the embedded engine's shared state, HTTP client, and shutdown signal.
pub struct SuwayomiSupervisor {
  inner: Mutex<Inner>,
  client: reqwest::Client,
  /// Woken to interrupt boot/backoff/monitor and stop the supervision loop.
  notify: tokio::sync::Notify,
  /// Set once shutdown has been requested; the loop checks it and exits.
  stopping: AtomicBool,
  /// Held for the app's lifetime; dropping removes the lockfile.
  lock: Mutex<Option<LockGuard>>,
  /// Loopback URL of the on-device Cloudflare shim (N-CF), if it started. Set once at
  /// `start()`; the supervisor points the engine at it via `setSettings` on each `Ready`
  /// transition (the shim URL is only known post-broker, so this must run after ready).
  cf_shim_url: Mutex<Option<String>>,
}

impl SuwayomiSupervisor {
  pub fn new() -> Self {
    let client = build_client();
    SuwayomiSupervisor {
      inner: Mutex::new(Inner {
        port: None,
        state: EngineState::Starting,
        version: None,
        last_error: None,
      }),
      client,
      notify: tokio::sync::Notify::new(),
      stopping: AtomicBool::new(false),
      lock: Mutex::new(None),
      cf_shim_url: Mutex::new(None),
    }
  }

  /// Record the on-device Cloudflare shim URL so `setSettings` can point the engine at it
  /// once ready. Called at most once, before the supervision loop runs.
  pub fn set_cf_shim_url(&self, url: String) {
    *self.cf_shim_url.lock().unwrap_or_else(|e| e.into_inner()) = Some(url);
  }

  fn set_ready(&self, port: u16, version: String) {
    let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    g.port = Some(port);
    g.state = EngineState::Ready;
    g.version = Some(version);
    g.last_error = None;
  }

  /// Move to a non-ready state, clearing the port (base_url must be None off-ready).
  fn transition(&self, state: EngineState, err: Option<String>) {
    let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    g.state = state;
    if state != EngineState::Ready {
      g.port = None;
    }
    if let Some(e) = err {
      g.last_error = Some(e);
    }
  }

  pub fn status(&self) -> Status {
    let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    Status {
      state: g.state.as_str().to_string(),
      version: g.version.clone(),
      last_error: g.last_error.clone(),
    }
  }

  pub fn base_url(&self) -> Option<String> {
    let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    if g.state == EngineState::Ready {
      g.port.map(|p| format!("http://127.0.0.1:{p}"))
    } else {
      None
    }
  }

  /// Beat the single-instance lockfile's heartbeat, if we currently hold it.
  fn refresh_lock(&self) {
    if let Some(guard) = self.lock.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
      guard.refresh();
    }
  }

  /// The `(port, client)` needed to proxy a GraphQL call, or `None` if not ready.
  fn ready_endpoint(&self) -> Option<(u16, reqwest::Client)> {
    let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    if g.state == EngineState::Ready {
      g.port.map(|p| (p, self.client.clone()))
    } else {
      None
    }
  }

  /// Signal shutdown and wait (bounded) for the supervision loop to report `Stopped`.
  /// Safe to call from the Tauri run-loop thread (not a tokio worker).
  pub async fn stop_and_wait(&self) {
    self.stopping.store(true, Ordering::SeqCst);
    self.notify.notify_waiters();
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
      if self.inner.lock().unwrap_or_else(|e| e.into_inner()).state == EngineState::Stopped {
        break;
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Backstop: reflect Stopped even if no task was running (e.g. lock refused).
    self.transition(EngineState::Stopped, None);
  }
}

impl Default for SuwayomiSupervisor {
  fn default() -> Self {
    Self::new()
  }
}

/// Broker a free loopback port: bind :0, read the assignment, drop the listener.
fn broker_port() -> std::io::Result<u16> {
  let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
  let port = listener.local_addr()?.port();
  drop(listener);
  Ok(port)
}

/// Render the authoritative HOCON `server.conf`. The persisted conf is authoritative
/// (JVM `-Dserver.*` sysprops do NOT reliably override Suwayomi's reactive config), so
/// this is rewritten before every launch. `kcefEnabled = false` is CRITICAL (else the
/// server downloads 226 MB of Chromium and SIGTRAPs headless).
///
/// The `authMode`/`authUsername`/`authPassword` trio is v2.3.2243's authentication
/// surface (the older `basicAuth*` keys are deprecated and migrate onto these), and
/// `BASIC_AUTH` is the enum constant name the config parser expects. It is not optional
/// hardening: the engine's GraphQL API can add extension repos and install extensions,
/// i.e. run arbitrary JVM code in-process, and a loopback bind keeps out *remote* callers
/// only — every other process on the machine can reach 127.0.0.1 just as easily as we can.
fn render_server_conf(port: u16) -> String {
  format!(
    "server.ip = \"127.0.0.1\"\n\
     server.port = {port}\n\
     server.webUIEnabled = false\n\
     server.initialOpenInBrowserEnabled = false\n\
     server.systemTrayEnabled = false\n\
     server.kcefEnabled = false\n\
     server.flareSolverrEnabled = false\n\
     server.authMode = \"BASIC_AUTH\"\n\
     server.authUsername = \"{user}\"\n\
     server.authPassword = \"{password}\"\n",
    user = ENGINE_AUTH_USER,
    password = engine_password()
  )
}

/// Write `server.conf` into the data dir (creating it), and best-effort remove any
/// previously downloaded kcef payload so a stale copy can't be re-used.
fn prepare_data_dir(data_dir: &Path, port: u16) -> Result<(), String> {
  std::fs::create_dir_all(data_dir).map_err(|e| format!("create data dir: {e}"))?;
  std::fs::write(data_dir.join("server.conf"), render_server_conf(port))
    .map_err(|e| format!("write server.conf: {e}"))?;
  let kcef = data_dir.join("bin").join("kcef");
  let _ = std::fs::remove_dir_all(&kcef);
  let _ = std::fs::remove_file(&kcef);
  Ok(())
}

/// Spawn the JVM per the proven launch line, piping stdout/stderr into the `suwayomi`
/// log target. Port is delivered via `server.conf`, not argv.
fn spawn_engine(cfg: &EngineConfig) -> Result<tokio::process::Child, String> {
  let mut cmd = tokio::process::Command::new(&cfg.java);
  cmd
    .arg("-Djava.awt.headless=true")
    .arg("-Xmx512m")
    .arg(format!(
      "-Dsuwayomi.tachidesk.config.server.rootDir={}",
      cfg.data_dir.display()
    ))
    .arg("-jar")
    .arg(&cfg.jar)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    // Backstop: if this Child is ever dropped (task cancelled / runtime torn down),
    // the OS kills the JVM so we never orphan it.
    .kill_on_drop(true);

  let mut child = cmd
    .spawn()
    .map_err(|e| format!("spawn java ({}) failed: {e}", cfg.java.display()))?;

  if let Some(out) = child.stdout.take() {
    tokio::spawn(async move {
      let mut lines = tokio::io::BufReader::new(out).lines();
      while let Ok(Some(line)) = lines.next_line().await {
        log::info!(target: "suwayomi", "{line}");
      }
    });
  }
  if let Some(err) = child.stderr.take() {
    tokio::spawn(async move {
      let mut lines = tokio::io::BufReader::new(err).lines();
      while let Ok(Some(line)) = lines.next_line().await {
        log::warn!(target: "suwayomi", "{line}");
      }
    });
  }
  Ok(child)
}

/// Poll `POST /api/graphql { aboutServer { version } }` until a version comes back or
/// the timeout elapses.
async fn poll_ready(
  client: &reqwest::Client,
  port: u16,
  timeout: Duration,
) -> Result<String, String> {
  let url = format!("http://127.0.0.1:{port}/api/graphql");
  let body = json!({ "query": "{ aboutServer { version } }" }).to_string();
  let deadline = Instant::now() + timeout;
  // Set once something answers 2xx on our port without being Suwayomi. The port broker is
  // inherently TOCTOU (we release the port before the JVM binds it), so this turns the
  // "a stranger owns the port" case from an opaque timeout into a named cause.
  let mut foreign_service = false;
  loop {
    if let Ok(resp) = client
      .post(&url)
      .header(reqwest::header::CONTENT_TYPE, "application/json")
      .body(body.clone())
      .send()
      .await
    {
      let ok = resp.status().is_success();
      let parsed = resp
        .bytes()
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
      if let Some(ver) = parsed
        .as_ref()
        .and_then(|val| val.pointer("/data/aboutServer/version"))
        .and_then(|v| v.as_str())
      {
        return Ok(ver.to_string());
      }
      // A 2xx whose body isn't even GraphQL-shaped is not a half-booted engine (which
      // would answer `{"data":…}`/`{"errors":…}` or refuse the connection outright).
      let graphql_shaped = parsed
        .as_ref()
        .is_some_and(|val| val.get("data").is_some() || val.get("errors").is_some());
      foreign_service |= ok && !graphql_shaped;
    }
    if Instant::now() >= deadline {
      return Err(if foreign_service {
        format!(
          "port {port} answered but is not the engine; another process took the brokered port"
        )
      } else {
        format!(
          "engine did not become ready within {}s (port {port})",
          timeout.as_secs()
        )
      });
    }
    tokio::time::sleep(POLL_INTERVAL).await;
  }
}

/// Full core boot path: broker port, write conf, spawn, poll readiness. Readiness races
/// the child's own exit, so a JVM that dies immediately (bad jar, missing JRE module,
/// stolen port, OOM) is reported as an exit status in about a second instead of burning
/// the whole `ready_timeout` on a process that is already gone — which also lets the
/// caller's restart-storm accounting actually engage. Either failure kills the child
/// before returning (no orphan); the next attempt re-brokers a fresh port.
/// Returns the live child, the port, and the reported version.
async fn boot_engine(
  cfg: &EngineConfig,
  client: &reqwest::Client,
  ready_timeout: Duration,
) -> Result<(tokio::process::Child, u16, String), String> {
  let port = broker_port().map_err(|e| format!("port broker: {e}"))?;
  prepare_data_dir(&cfg.data_dir, port)?;
  log::info!(target: "suwayomi", "launching engine on 127.0.0.1:{port}");
  let mut child = spawn_engine(cfg)?;
  // `biased` so a dead child is reported as such even if a readiness poll happens to be
  // resolvable in the same wake — the exit status is always the more useful diagnostic.
  let outcome = tokio::select! {
    biased;
    status = child.wait() => Err(match status {
      Ok(st) => format!("engine exited during startup ({st}); see the suwayomi log for the JVM's own output"),
      Err(e) => format!("waiting on the engine process failed during startup: {e}"),
    }),
    ready = poll_ready(client, port, ready_timeout) => ready,
  };
  match outcome {
    Ok(version) => Ok((child, port, version)),
    Err(e) => {
      stop_child(&mut child).await;
      Err(e)
    }
  }
}

/// Ask the JVM to exit on its own terms. `Child::start_kill` is SIGKILL on unix, which
/// skips every shutdown hook; the engine registers hooks that stop Javalin and close the
/// embedded H2 store, so killing outright is what leaves the DB to recover (or refuse to
/// open) on the next launch. std exposes no signal API and the crate has no libc
/// dependency, hence `kill(1)`. Returns whether the signal was delivered.
#[cfg(unix)]
async fn request_graceful_exit(child: &tokio::process::Child) -> bool {
  let Some(pid) = child.id() else {
    return false; // already reaped
  };
  let mut cmd = tokio::process::Command::new("kill");
  cmd
    .arg("-TERM")
    .arg(pid.to_string())
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
  matches!(cmd.status().await, Ok(st) if st.success())
}

/// Windows has no console-less equivalent of SIGTERM for a child we did not create a job
/// object for, so the caller's `TerminateProcess` path stands unchanged.
#[cfg(not(unix))]
async fn request_graceful_exit(_child: &tokio::process::Child) -> bool {
  false
}

/// Terminate a live child: request a graceful exit first so the JVM's shutdown hooks run,
/// then fall back to a kill (spike-proven to leave no orphan), confirming within a grace
/// window and forcing again if it somehow lingers.
async fn stop_child(child: &mut tokio::process::Child) {
  if request_graceful_exit(child).await
    && tokio::time::timeout(GRACE, child.wait()).await.is_ok()
  {
    return;
  }
  let _ = child.start_kill();
  if tokio::time::timeout(GRACE, child.wait()).await.is_err() {
    let _ = child.start_kill();
    let _ = child.wait().await;
  }
}

/// Park until shutdown is requested, deterministically. `Notify::notify_waiters()` stores
/// no permit, so a stop signalled while the supervisor isn't parked in `notified()` is
/// lost. We defend against that by re-checking the `stopping` flag on a short timer: a
/// missed wake costs at most one 100 ms tick, never a hang. Combined with `stop_and_wait`
/// storing `stopping = true` BEFORE `notify_waiters()`, every select branch below observes
/// a stop even if the notify raced the select's registration — so a mid-boot stop is seen
/// promptly instead of running the full `READY_TIMEOUT` with a live child.
async fn wait_for_stop(sup: &SuwayomiSupervisor) {
  loop {
    if sup.stopping.load(Ordering::SeqCst) {
      return;
    }
    tokio::select! {
      _ = sup.notify.notified() => return,
      _ = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
  }
}

/// The supervision loop: (re)boot the engine, monitor for unexpected exit, restart with
/// capped backoff, and exit cleanly on shutdown. Runs on the Tauri async runtime.
async fn run_supervisor(sup: Arc<SuwayomiSupervisor>, cfg: EngineConfig) {
  let mut restarts: VecDeque<Instant> = VecDeque::new();
  while !sup.stopping.load(Ordering::SeqCst) {
    sup.transition(EngineState::Starting, None);

    // Prefer a shutdown signal over completing a boot. If stop fires mid-boot the boot
    // future is dropped, and kill_on_drop reaps any child it had spawned. `wait_for_stop`
    // observes the `stopping` flag even if the `notify_waiters` wake was lost, so a stop
    // during the up-to-`READY_TIMEOUT` boot is seen within ~100 ms rather than at timeout.
    let boot = tokio::select! {
      biased;
      _ = wait_for_stop(&sup) => break,
      r = boot_engine(&cfg, &sup.client, READY_TIMEOUT) => r,
    };

    match boot {
      Ok((mut child, port, version)) => {
        log::info!(target: "suwayomi", "engine ready ({version}) on port {port}");
        sup.set_ready(port, version);
        // Point the engine's Cloudflare interceptor at our on-device shim (N-CF). Best
        // effort and fire-and-forget: a failure only logs and CF sources fall back to
        // server-fetch — it must never stall or crash the supervision loop.
        if let Some(shim_url) = sup.cf_shim_url.lock().unwrap_or_else(|e| e.into_inner()).clone() {
          let client = sup.client.clone();
          tauri::async_runtime::spawn(crate::cloudflare::apply_settings(client, port, shim_url));
        }
        tokio::select! {
          biased;
          _ = wait_for_stop(&sup) => {
            stop_child(&mut child).await;
            break;
          }
          status = child.wait() => {
            log::warn!(target: "suwayomi", "engine exited unexpectedly: {status:?}");
            sup.transition(EngineState::Degraded, Some("engine exited unexpectedly".into()));
          }
        }
      }
      Err(e) => {
        log::error!(target: "suwayomi", "engine boot failed: {e}");
        sup.transition(EngineState::Degraded, Some(e));
      }
    }

    if sup.stopping.load(Ordering::SeqCst) {
      break;
    }

    // Restart accounting: count restarts inside the trailing 60 s window.
    let now = Instant::now();
    restarts.push_back(now);
    while matches!(restarts.front(), Some(&t) if now.duration_since(t) > Duration::from_secs(60)) {
      restarts.pop_front();
    }
    let storm = restarts.len() > MAX_RESTARTS_PER_MIN;
    let backoff = if storm {
      sup.transition(
        EngineState::Degraded,
        Some("restart budget exceeded; backing off".into()),
      );
      Duration::from_secs(60)
    } else {
      // 2s, 4s, 6s ... capped at 30s.
      Duration::from_secs((2 * restarts.len() as u64).min(30))
    };

    tokio::select! {
      biased;
      _ = wait_for_stop(&sup) => break,
      _ = tokio::time::sleep(backoff) => {}
    }
    if storm {
      restarts.clear();
    }
  }
  sup.transition(EngineState::Stopped, None);
  log::info!(target: "suwayomi", "supervisor stopped");
}

/// On Windows the JVM launcher is `java.exe`.
fn java_exe() -> &'static str {
  if cfg!(windows) {
    "java.exe"
  } else {
    "java"
  }
}

/// Best-effort bundle sub-path for a bundled JRE. N1.2 finalizes the real resource
/// layout; until then this is only probed (absent in dev), so the exact slug is not
/// load-bearing.
fn target_slug() -> String {
  format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// Resolve the `java` binary: env override first (dev/test), then a bundled JRE.
fn resolve_java(app: &AppHandle) -> Result<PathBuf, String> {
  if let Ok(p) = std::env::var("KOMIKA_JAVA_BIN") {
    if !p.is_empty() {
      return Ok(PathBuf::from(p));
    }
  }
  if let Ok(res) = app.path().resource_dir() {
    let cand = res
      .join("jre")
      .join(target_slug())
      .join("bin")
      .join(java_exe());
    if cand.exists() {
      return Ok(cand);
    }
  }
  Err("no java runtime found: set KOMIKA_JAVA_BIN or bundle a JRE".into())
}

/// Resolve the Suwayomi-Server jar: env override first (dev/test), then bundled resource.
fn resolve_jar(app: &AppHandle) -> Result<PathBuf, String> {
  if let Ok(p) = std::env::var("KOMIKA_SUWAYOMI_JAR") {
    if !p.is_empty() {
      return Ok(PathBuf::from(p));
    }
  }
  if let Ok(res) = app.path().resource_dir() {
    let cand = res.join("suwayomi").join("Suwayomi-Server.jar");
    if cand.exists() {
      return Ok(cand);
    }
  }
  Err("no Suwayomi-Server.jar found: set KOMIKA_SUWAYOMI_JAR or bundle the jar".into())
}

fn resolve_config(app: &AppHandle) -> Result<EngineConfig, String> {
  let data_dir = app
    .path()
    .app_data_dir()
    .map_err(|e| format!("resolve app_data_dir: {e}"))?
    .join("suwayomi");
  let java = resolve_java(app)?;
  let jar = resolve_jar(app)?;
  Ok(EngineConfig {
    java,
    jar,
    data_dir,
  })
}

/// Best-effort single-instance lock: a lockfile in the data dir keeps a second app
/// instance from double-spawning the engine against one data dir (two engines on one H2
/// store corrupt it). `LockGuard::drop` only runs on a clean exit, so liveness is proved
/// by the holder's heartbeat (`run_lock_heartbeat` refreshes the file every
/// `LOCK_HEARTBEAT`): a file whose mtime has stood still for `LOCK_STALE_AFTER` belongs
/// to a process that was killed or lost power, and we take it over. Callers retry rather
/// than treat a refusal as fatal, so neither a crash nor a live second instance can leave
/// the engine permanently disabled.
fn acquire_lock(data_dir: &Path) -> Result<LockGuard, String> {
  std::fs::create_dir_all(data_dir).map_err(|e| format!("create data dir: {e}"))?;
  let path = data_dir.join("komika-suwayomi.lock");
  match create_lock_file(&path) {
    Ok(guard) => Ok(guard),
    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
      if !lock_is_abandoned(&path) {
        return Err("another Komika instance holds the engine lock".into());
      }
      log::warn!(target: "suwayomi", "taking over stale engine lock ({})", path.display());
      let _ = std::fs::remove_file(&path);
      // If a second launch is racing us for the same corpse, exactly one `create_new`
      // wins; the loser reports the lock as held and retries on its own timer.
      create_lock_file(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => {
          "another Komika instance holds the engine lock".to_string()
        }
        _ => format!("acquire engine lock: {e}"),
      })
    }
    Err(e) => Err(format!("acquire engine lock: {e}")),
  }
}

/// Create the lockfile exclusively and stamp it with our PID (diagnostics, plus the write
/// that seeds the heartbeat mtime). The handle stays open inside the returned guard.
fn create_lock_file(path: &Path) -> std::io::Result<LockGuard> {
  let file = std::fs::OpenOptions::new()
    .write(true)
    .create_new(true)
    .open(path)?;
  let mut guard = LockGuard {
    path: path.to_path_buf(),
    file,
  };
  guard.refresh();
  Ok(guard)
}

/// Whether the lockfile's heartbeat has been silent longer than `LOCK_STALE_AFTER`.
/// Unreadable or future-dated metadata answers `false`: never steal a lock we cannot
/// reason about — the cost of waiting is a retry, the cost of a wrong takeover is two
/// engines on one database.
fn lock_is_abandoned(path: &Path) -> bool {
  match std::fs::metadata(path).and_then(|m| m.modified()) {
    Ok(modified) => modified
      .elapsed()
      .map(|age| age > LOCK_STALE_AFTER)
      .unwrap_or(false),
    Err(_) => false,
  }
}

/// Keep our lockfile's heartbeat advancing for as long as we hold it. Without this a
/// long-lived, perfectly healthy instance would look abandoned to the next launch.
async fn run_lock_heartbeat(sup: Arc<SuwayomiSupervisor>) {
  loop {
    tokio::select! {
      biased;
      _ = wait_for_stop(&sup) => return,
      _ = tokio::time::sleep(LOCK_HEARTBEAT) => sup.refresh_lock(),
    }
  }
}

/// Initialize and launch the supervisor. Non-fatal by design: a resolution failure logs,
/// sets `Degraded`, and returns — it never aborts app startup, and a held lock only defers
/// the launch (see `run_engine`). The supervisor is always started; the JS-side flag gates
/// *use*, and an unused engine is harmless (nothing calls it).
pub fn start(app: AppHandle) {
  let sup = match app.try_state::<Arc<SuwayomiSupervisor>>() {
    Some(s) => s.inner().clone(),
    None => {
      log::error!(target: "suwayomi", "supervisor state not managed; skipping start");
      return;
    }
  };

  let cfg = match resolve_config(&app) {
    Ok(c) => c,
    Err(e) => {
      log::warn!(target: "suwayomi", "engine unavailable: {e}");
      sup.transition(EngineState::Degraded, Some(e));
      return;
    }
  };

  tauri::async_runtime::spawn(run_engine(app, sup, cfg));
}

/// Take the single-instance lock, wire the Cloudflare shim, then run the supervision
/// loop. The lock is *waited for*, not demanded once: a stale lock from a killed
/// predecessor becomes takeable one `LOCK_STALE_AFTER` after its heartbeat stops, and a
/// genuine second instance may quit at any time — either way the engine heals itself
/// instead of staying Degraded for the rest of the session.
async fn run_engine(app: AppHandle, sup: Arc<SuwayomiSupervisor>, cfg: EngineConfig) {
  let mut announced = false;
  loop {
    if sup.stopping.load(Ordering::SeqCst) {
      sup.transition(EngineState::Stopped, None);
      return;
    }
    match acquire_lock(&cfg.data_dir) {
      Ok(guard) => {
        *sup.lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard);
        break;
      }
      Err(e) => {
        // Log/report once, then keep quiet: this can poll for the whole session.
        if !announced {
          log::warn!(target: "suwayomi", "engine start deferred: {e}");
          sup.transition(EngineState::Degraded, Some(e));
          announced = true;
        }
        tokio::select! {
          biased;
          _ = wait_for_stop(&sup) => {
            sup.transition(EngineState::Stopped, None);
            return;
          }
          _ = tokio::time::sleep(LOCK_RETRY) => {}
        }
      }
    }
  }

  tauri::async_runtime::spawn(run_lock_heartbeat(sup.clone()));

  // On-device Cloudflare shim (N-CF): start the loopback FlareSolverr-v1 listener and
  // record its URL so the supervisor wires it into the engine once ready. Best effort;
  // if it doesn't start, CF sources just fall back to server-fetch. The shim is inert
  // until the engine POSTs to it (which only happens when the engine hits a challenge).
  if let Some(shim) = crate::cloudflare::start(&app) {
    sup.set_cf_shim_url(shim.url());
    app.manage(shim);
  }

  run_supervisor(sup, cfg).await;
}

// ---- JS-facing commands ----------------------------------------------------

/// `http://127.0.0.1:<port>` when the engine is ready, else `None`.
#[tauri::command]
pub fn suwayomi_base_url(state: State<'_, Arc<SuwayomiSupervisor>>) -> Option<String> {
  state.base_url()
}

/// `{ state, version, lastError }`.
#[tauri::command]
pub fn suwayomi_status(state: State<'_, Arc<SuwayomiSupervisor>>) -> Status {
  state.status()
}

/// IPC-proxy transport: POST a GraphQL query to the loopback engine and return the
/// parsed JSON. This is the transport the JS `LocalSuwayomiBackend` calls by name — the
/// port stays hidden and CSP stays tight (`connect-src 'self' ipc:`).
#[tauri::command]
pub async fn suwayomi_gql(
  state: State<'_, Arc<SuwayomiSupervisor>>,
  query: String,
  variables: Option<Value>,
) -> Result<Value, String> {
  let (port, client) = state
    .ready_endpoint()
    .ok_or_else(|| "suwayomi engine not ready".to_string())?;
  let body = match variables {
    Some(vars) => json!({ "query": query, "variables": vars }),
    None => json!({ "query": query }),
  };
  let resp = client
    .post(format!("http://127.0.0.1:{port}/api/graphql"))
    .header(reqwest::header::CONTENT_TYPE, "application/json")
    .body(body.to_string())
    .send()
    .await
    .map_err(|e| e.to_string())?;
  let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
  serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string())
}

/// Guard for `suwayomi_image`'s `path`: our narrow, deliberate loopback exemption
/// (plan §13). The engine's image/proxy routes all live under `/api/`, so we require
/// `path` to be exactly that shape — this is the SSRF containment that keeps the
/// command from ever being turned into an arbitrary-URL fetch on 127.0.0.1. Reject
/// anything that isn't a rooted `/api/...` path, and reject any `..` traversal outright.
fn validate_image_path(path: &str) -> Result<(), String> {
  // Must be a rooted path beginning with `/api/`. This rejects empty strings,
  // absolute URLs (`http://…`), scheme-relative (`//evil`), backslashes, and any
  // other route (`/etc/passwd`, `/foo`). Byte-level `starts_with` is exact — a
  // leading `//` fails because the second char must be `a`.
  if !path.starts_with("/api/") {
    return Err(format!("suwayomi_image: path must start with /api/, got {path:?}"));
  }
  // Percent-decode ONCE (mirrors the engine's own HTTP-layer decode) before the
  // traversal checks, so encoded forms like `%2e%2e` / `%5c` can't slip past the
  // literal `..` / `\` guards below.
  let decoded = percent_decode_once(path);
  // Belt-and-braces: no path traversal, even though it can't escape `/api/` on the
  // engine — a `/api/../secret` (or `/api/%2e%2e/secret`) must never be forwarded.
  if decoded.contains("..") {
    return Err(format!("suwayomi_image: path must not contain '..', got {path:?}"));
  }
  // No backslashes (a Windows-y separator that could confuse downstream parsing).
  if decoded.contains('\\') {
    return Err(format!("suwayomi_image: path must not contain '\\', got {path:?}"));
  }
  Ok(())
}

/// Single-pass, lossy percent-decode. One decode level matches what the engine's
/// HTTP router applies, so decoding here catches `%2e%2e`/`%5c` traversal without
/// over-decoding (`%252e` stays `%2e`, exactly as the engine would see it).
fn percent_decode_once(s: &str) -> String {
  let bytes = s.as_bytes();
  let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'%' && i + 2 < bytes.len() {
      let hi = (bytes[i + 1] as char).to_digit(16);
      let lo = (bytes[i + 2] as char).to_digit(16);
      if let (Some(hi), Some(lo)) = (hi, lo) {
        out.push((hi * 16 + lo) as u8);
        i += 3;
        continue;
      }
    }
    out.push(bytes[i]);
    i += 1;
  }
  String::from_utf8_lossy(&out).into_owned()
}

/// IPC-proxy transport for engine image bytes: stream a page/proxy image from the
/// LOOPBACK engine's own `/api/` routes over IPC and hand the raw bytes to JS. This is
/// the image counterpart to `suwayomi_gql` — the native `NativeImageProvider` passes a
/// relative Suwayomi path (e.g. `/api/v1/manga/3/chapter/1/page/0`) and gets back an
/// `ArrayBuffer`, so the engine (not the JS side) applies the extension's request
/// context, the port stays hidden, and CSP stays `connect-src 'self' ipc:`.
///
/// Loopback is INTENTIONAL here (the inverse of `fetch_image`, whose host guard blocks
/// 127.0.0.1). The SSRF containment is the `/api/` path guard in `validate_image_path`:
/// this command can only ever hit the engine's own routes, never an arbitrary URL. The
/// body is size-capped exactly like `fetch_image`'s `read_capped` (`MAX_IMAGE_BYTES`).
#[tauri::command]
pub async fn suwayomi_image(
  state: State<'_, Arc<SuwayomiSupervisor>>,
  path: String,
) -> Result<tauri::ipc::Response, String> {
  validate_image_path(&path)?;
  // Share the process-wide image concurrency cap with `fetch_image` so the two
  // paths together can't stack N × MAX_IMAGE_BYTES of native heap.
  let _permit = crate::image_fetch_semaphore()
    .acquire()
    .await
    .map_err(|_| "image fetch pool closed".to_string())?;
  let (port, client) = state
    .ready_endpoint()
    .ok_or_else(|| "suwayomi engine not ready".to_string())?;
  let resp = client
    .get(format!("http://127.0.0.1:{port}{path}"))
    .send()
    .await
    .map_err(|e| e.to_string())?;
  let status = resp.status();
  if !status.is_success() {
    return Err(format!("engine returned {status}"));
  }
  // Mirror lib.rs's `read_capped`: fail fast on an oversized Content-Length, then
  // enforce the cap again while streaming (a missing/understated header can't OOM us).
  if let Some(len) = resp.content_length() {
    if len > MAX_IMAGE_BYTES as u64 {
      return Err(format!("image too large: {len} bytes"));
    }
  }
  let mut buf: Vec<u8> = Vec::new();
  let mut resp = resp;
  while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
    if buf.len() + chunk.len() > MAX_IMAGE_BYTES {
      return Err("image exceeds size cap".to_string());
    }
    buf.extend_from_slice(&chunk);
  }
  Ok(tauri::ipc::Response::new(buf))
}

// ---- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn server_conf_has_required_lines_and_kcef_off() {
    let conf = render_server_conf(54321);
    assert!(conf.contains("server.ip = \"127.0.0.1\""));
    assert!(conf.contains("server.port = 54321"));
    assert!(conf.contains("server.webUIEnabled = false"));
    assert!(conf.contains("server.initialOpenInBrowserEnabled = false"));
    assert!(conf.contains("server.systemTrayEnabled = false"));
    // The two lines that keep the JVM from crashing / phoning out.
    assert!(conf.contains("server.kcefEnabled = false"));
    assert!(conf.contains("server.flareSolverrEnabled = false"));
    // Auth is mandatory: loopback keeps remote callers out, not local ones.
    assert!(conf.contains("server.authMode = \"BASIC_AUTH\""));
    assert!(conf.contains(&format!("server.authUsername = \"{ENGINE_AUTH_USER}\"")));
    assert!(conf.contains(&format!("server.authPassword = \"{}\"", engine_password())));
  }

  #[test]
  fn engine_password_is_stable_and_unguessable() {
    let password = engine_password();
    assert_eq!(password, engine_password(), "one credential per process");
    assert_eq!(password.len(), 32, "128 bits of hex");
    assert!(password.chars().all(|c| c.is_ascii_hexdigit()));
    // Two fresh draws must not collide (a constant token would defeat the whole point).
    assert_ne!(random_token(), random_token());
  }

  #[test]
  fn basic_auth_value_matches_rfc_vector() {
    // RFC 7617's own example, which also exercises both padding lengths.
    assert_eq!(
      basic_auth_value("Aladdin", "open sesame"),
      "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
    );
    assert_eq!(basic_auth_value("a", "b"), "Basic YTpi");
    assert_eq!(basic_auth_value("ab", "c"), "Basic YWI6Yw==");
  }

  #[test]
  fn image_path_guard_accepts_engine_api_routes() {
    // The real relative path shape the engine returns (GQL-SCHEMA-FINDINGS.md §C).
    assert!(validate_image_path("/api/v1/manga/3/chapter/1/page/0").is_ok());
    assert!(validate_image_path("/api/v1/manga/3/thumbnail").is_ok());
  }

  #[test]
  fn image_path_guard_rejects_non_api_and_traversal() {
    for bad in [
      "",                        // empty
      "/foo",                    // wrong route
      "/etc/passwd",             // absolute-looking non-api
      "http://evil",             // absolute URL
      "//evil",                  // scheme-relative host
      "/api/../secret",          // traversal out of /api/
      "/api/%2e%2e/secret",      // percent-encoded traversal
      "/api/%2E%2E/secret",      // percent-encoded traversal (upper hex)
      "../x",                    // relative traversal
      "/api/\\windows",          // backslash separator
      "/api/x%5cy",              // percent-encoded backslash
      "api/v1/manga",            // not rooted
    ] {
      assert!(
        validate_image_path(bad).is_err(),
        "{bad:?} must be rejected"
      );
    }
  }

  #[test]
  fn broker_port_returns_usable_port() {
    let port = broker_port().expect("broker a port");
    assert!(port > 0);
    // Freed by the broker, so we can bind it ourselves right after.
    let again = std::net::TcpListener::bind(("127.0.0.1", port));
    assert!(again.is_ok(), "brokered port should be immediately bindable");
  }

  #[test]
  fn state_transitions_drive_base_url_and_status() {
    let sup = SuwayomiSupervisor::new();
    assert_eq!(sup.status().state, "starting");
    assert!(sup.base_url().is_none());

    sup.set_ready(4567, "v2.3.2243".into());
    assert_eq!(sup.status().state, "ready");
    assert_eq!(sup.status().version.as_deref(), Some("v2.3.2243"));
    assert_eq!(sup.base_url().as_deref(), Some("http://127.0.0.1:4567"));

    sup.transition(EngineState::Degraded, Some("boom".into()));
    assert_eq!(sup.status().state, "degraded");
    assert!(sup.base_url().is_none(), "no base_url when not ready");
    assert_eq!(sup.status().last_error.as_deref(), Some("boom"));

    sup.transition(EngineState::Stopped, None);
    assert_eq!(sup.status().state, "stopped");
    // last_error is sticky across a None-error transition.
    assert_eq!(sup.status().last_error.as_deref(), Some("boom"));
  }

  #[test]
  fn lockfile_is_exclusive_then_released() {
    let dir = std::env::temp_dir().join(format!("komika-lock-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let guard = acquire_lock(&dir).expect("first lock acquires");
    assert!(acquire_lock(&dir).is_err(), "second lock is refused");
    drop(guard);
    assert!(acquire_lock(&dir).is_ok(), "lock re-acquirable after release");
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// The crash case: a lockfile left behind by a process that never ran `Drop` must not
  /// disable the engine forever.
  #[test]
  fn stale_lockfile_is_taken_over() {
    let dir = std::env::temp_dir().join(format!("komika-stale-lock-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let guard = acquire_lock(&dir).expect("first lock acquires");
    assert!(acquire_lock(&dir).is_err(), "a beating lock is respected");
    // Age the heartbeat past the abandonment window — what a SIGKILLed holder looks like.
    guard
      .file
      .set_modified(std::time::SystemTime::now() - LOCK_STALE_AFTER * 2)
      .expect("age the lockfile");
    let taken = acquire_lock(&dir).expect("an abandoned lock is taken over");
    drop(taken);
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn lock_heartbeat_clears_abandonment() {
    let dir = std::env::temp_dir().join(format!("komika-beat-lock-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut guard = acquire_lock(&dir).expect("lock acquires");
    let path = dir.join("komika-suwayomi.lock");
    guard
      .file
      .set_modified(std::time::SystemTime::now() - LOCK_STALE_AFTER * 2)
      .expect("age the lockfile");
    assert!(lock_is_abandoned(&path), "a silent lock reads as abandoned");
    guard.refresh();
    assert!(!lock_is_abandoned(&path), "a beat lock reads as live");
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// Live boot against a REAL Suwayomi-Server jar. Skipped unless BOTH
  /// `KOMIKA_JAVA_BIN` and `KOMIKA_SUWAYOMI_JAR` are set; kept `#[ignore]` so the
  /// normal `cargo test` stays fast and offline. Run with:
  ///   KOMIKA_JAVA_BIN=... KOMIKA_SUWAYOMI_JAR=... cargo test -- --ignored sidecar_boots
  #[tokio::test]
  #[ignore]
  async fn sidecar_boots_and_reports_about_server() {
    let java = match std::env::var("KOMIKA_JAVA_BIN") {
      Ok(v) if !v.is_empty() => v,
      _ => return, // skip: env not set
    };
    let jar = match std::env::var("KOMIKA_SUWAYOMI_JAR") {
      Ok(v) if !v.is_empty() => v,
      _ => return, // skip: env not set
    };

    let data_dir =
      std::env::temp_dir().join(format!("komika-suwayomi-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let cfg = EngineConfig {
      java: PathBuf::from(java),
      jar: PathBuf::from(jar),
      data_dir: data_dir.clone(),
    };
    // Must be the supervisor's own client: the conf we write enables Basic auth, so a
    // credential-less client would poll a 401 until the readiness gate gave up.
    let client = build_client();

    // Boot: broker port, write conf, spawn, poll aboutServer.
    let (mut child, port, version) = boot_engine(&cfg, &client, Duration::from_secs(60))
      .await
      .expect("engine should boot and report aboutServer");
    assert!(port > 0);
    assert!(!version.is_empty(), "aboutServer returned a version");

    // The conf we wrote must have kcef disabled (else the JVM would SIGTRAP).
    let conf = std::fs::read_to_string(data_dir.join("server.conf")).unwrap();
    assert!(conf.contains("server.kcefEnabled = false"));

    // Shut down and assert no orphan: the child must have exited.
    stop_child(&mut child).await;
    assert!(
      child.try_wait().unwrap().is_some(),
      "child JVM must be gone after stop_child (no orphan)"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
  }
}
