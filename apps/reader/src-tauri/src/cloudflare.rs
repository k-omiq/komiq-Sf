//! On-device Cloudflare bypass for the embedded Suwayomi engine (native desktop plan
//! §8b, item N-CF). NO Suwayomi fork.
//!
//! Stock Suwayomi v2.3.2243 already ships a `CloudflareInterceptor`: when
//! `flareSolverrEnabled=true`, on a `403`/`503` + `Server: cloudflare` response it POSTs
//! a **FlareSolverr-v1** request to `flareSolverrUrl`, then harvests `cf_clearance` into
//! its OkHttp cookie jar and unifies its User-Agent from the reply. So instead of
//! forking, we run a loopback Rust HTTP shim that *speaks the FlareSolverr-v1 protocol*
//! and solves the challenge in Tauri's own WebView, and point the engine at it via the
//! `setSettings` mutation (see `suwayomi.rs`).
//!
//! ## Trust model (deliberate)
//! The shim binds **127.0.0.1 only** and only the loopback engine calls it. But the URLs
//! it is asked to open WebViews for come from the (untrusted) extension via the engine —
//! extensions run untrusted scraper code (plan §13). This is inherent to any on-device
//! CF-solve design and is the same trust model Mihon/tachimanga use: the WebView renders
//! a third-party origin to obtain its `cf_clearance`. We do NOT execute or trust any
//! response content; we only read cookies + the (fixed) UA back out.
//!
//! ## Layering
//! * [`FsReq`]/[`FsResp`]/[`FsSolution`]/[`FsCookie`] — the FlareSolverr-v1 wire subset.
//! * [`CloudflareSolver`] — the seam that makes the shim testable without a real WebView.
//! * [`handle_v1`] — pure request→solver→response mapping (unit-tested with fake solvers).
//! * [`CfShim`] — the loopback `tiny_http` listener exposing `POST /v1` (+ `GET /` health).
//! * [`WebviewSolver`] — the real Tauri-WebView solver (compiles + clippy; a live solve
//!   needs a real display + a CF-gated source, so it is not verified here).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::webview::WebviewWindowBuilder;
use tauri::{AppHandle, Manager, Url, WebviewUrl};

/// The cookie the engine's `CloudflareInterceptor` waits for (`COOKIE_NAMES`).
const COOKIE_NAME: &str = "cf_clearance";
/// Loopback-only bind. Port `0` = OS-assigned ephemeral; we read the real port back.
const SHIM_BIND: &str = "127.0.0.1:0";
/// Reported in `FsResp.version` (cosmetic; the engine does not gate on it).
const SHIM_VERSION: &str = "komika-cf-shim/1.0.0";
/// Default solve budget if the engine omits `maxTimeout` (engine default is 60 s).
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// Hard wall-clock cap on a single solve, regardless of the engine-requested
/// `maxTimeout`. Enforced by clamping the timeout handed to the solver, so the
/// solver's own deadline path still runs (the hidden window is always closed on
/// timeout — no leak). Kept well under the old 60 s so a hung WebView solve can
/// never head-of-line-block the listener for long, and shutdown stays prompt.
const MAX_SOLVE_WALL_MS: u64 = 45_000;
/// Cadence for polling the WebView cookie store for `cf_clearance`.
const POLL_INTERVAL: Duration = Duration::from_millis(400);
/// Hidden-solve grace before we surface the window for an interactive challenge.
const INTERACTIVE_GRACE: Duration = Duration::from_secs(8);
/// The **fixed** UA we set on the challenge WebView AND echo back in the solution.
/// UA unification is the #1 failure mode: CF binds `cf_clearance` to the exact UA that
/// solved the challenge, and the engine replays `solution.userAgent` on every later
/// request to that host — so the two MUST be identical. Setting a known UA on the
/// WebView and echoing the same string is the doc-endorsed unification path.
const CHALLENGE_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
  AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// A cookie in FlareSolverr-v1 shape. The engine sends `{name,value}` on the way in and
/// reads `{name,value,domain,path,expires,httpOnly,secure}` on the way out. Optional
/// fields default on deserialize (inbound cookies carry only name/value) and are always
/// emitted on serialize (matching FlareSolverr's real output shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsCookie {
  pub name: String,
  pub value: String,
  #[serde(default)]
  pub domain: Option<String>,
  #[serde(default)]
  pub path: Option<String>,
  #[serde(default)]
  pub expires: Option<f64>,
  #[serde(rename = "httpOnly", default)]
  pub http_only: Option<bool>,
  #[serde(default)]
  pub secure: Option<bool>,
}

/// The FlareSolverr-v1 request the engine POSTs to `/v1`. Unknown keys (`session`,
/// `session_ttl_minutes`, `returnOnlyCookies`, …) are ignored by serde.
#[derive(Debug, Clone, Deserialize)]
pub struct FsReq {
  pub cmd: String,
  pub url: String,
  #[serde(default)]
  pub cookies: Option<Vec<FsCookie>>,
  #[serde(rename = "maxTimeout", default)]
  pub max_timeout: Option<u64>,
  // Form body for `request.post`. Parsed for wire-completeness but not replayed: the
  // WebView solve navigates (GET) to obtain `cf_clearance`; the engine replays the
  // original POST itself once it has the cookie.
  #[serde(rename = "postData", default)]
  #[allow(dead_code)]
  pub post_data: Option<String>,
}

/// The `solution` object inside a [`FsResp`]. The engine injects `cookies` into its jar
/// and unifies its UA from `user_agent` only when `status` is in `200..=299`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsSolution {
  pub url: String,
  pub status: u16,
  pub cookies: Vec<FsCookie>,
  #[serde(rename = "userAgent")]
  pub user_agent: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub headers: Option<serde_json::Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub response: Option<String>,
}

/// The FlareSolverr-v1 reply the engine deserializes (`FlareSolverResponse`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsResp {
  pub solution: FsSolution,
  pub status: String,
  pub message: String,
  #[serde(rename = "startTimestamp")]
  pub start_timestamp: i64,
  #[serde(rename = "endTimestamp")]
  pub end_timestamp: i64,
  pub version: String,
}

/// A solved challenge: the cookies (incl. `cf_clearance`) and the UA that solved it.
#[derive(Debug, Clone, PartialEq)]
pub struct Solved {
  pub cookies: Vec<FsCookie>,
  pub user_agent: String,
}

/// Boxed future alias so [`CloudflareSolver`] stays object-safe. (Native `async fn` in a
/// trait is not yet dyn-compatible on our toolchain, and the shim holds the solver as
/// `dyn CloudflareSolver` — so we hand-desugar to a boxed future instead of pulling in
/// `async_trait`.)
pub type SolveFuture<'a> = Pin<Box<dyn Future<Output = Result<Solved, String>> + Send + 'a>>;

/// The seam between the FlareSolverr shim and the challenge-solving mechanism. Real code
/// uses [`WebviewSolver`]; tests use a fake, so [`handle_v1`] is verifiable without a
/// WebView.
pub trait CloudflareSolver: Send + Sync {
  fn solve<'a>(
    &'a self,
    url: &'a str,
    seed_cookies: &'a [FsCookie],
    timeout: Duration,
  ) -> SolveFuture<'a>;
}

/// Milliseconds since the Unix epoch (FlareSolverr timestamps).
fn now_ms() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_millis() as i64)
    .unwrap_or(0)
}

/// Build a FlareSolverr-v1 error reply. `solution.status` is a non-2xx code so the stock
/// `CFClearance.requestWithFlareSolverr` throws `CloudflareBypassException` and Komika
/// falls back to server-fetch (plan §7). The engine-fallback contract lives here.
fn error_resp(url: &str, message: String, start: i64) -> FsResp {
  FsResp {
    solution: FsSolution {
      url: url.to_string(),
      status: 500,
      cookies: Vec::new(),
      user_agent: CHALLENGE_UA.to_string(),
      headers: None,
      response: None,
    },
    status: "error".to_string(),
    message,
    start_timestamp: start,
    end_timestamp: now_ms(),
    version: SHIM_VERSION.to_string(),
  }
}

/// Map a FlareSolverr-v1 request to a solver call to a FlareSolverr-v1 response. Pure
/// (no I/O of its own beyond the injected solver) and unit-testable.
///
/// On solver success: `status:"ok"`, `solution.status:200`, cookies + unified UA, and a
/// `message` that does **not** contain the substring `"not detected"` (the engine treats
/// that as a no-challenge sentinel and discards the solve). On solver error: an
/// [`error_resp`] the engine turns into a throw → server-fetch fallback.
pub async fn handle_v1(req: FsReq, solver: &dyn CloudflareSolver) -> FsResp {
  let start = now_ms();
  if req.cmd != "request.get" && req.cmd != "request.post" {
    return error_resp(&req.url, format!("unsupported cmd: {}", req.cmd), start);
  }
  let timeout = Duration::from_millis(req.max_timeout.unwrap_or(DEFAULT_TIMEOUT_MS));
  let seed = req.cookies.clone().unwrap_or_default();
  match solver.solve(&req.url, &seed, timeout).await {
    Ok(solved) => FsResp {
      solution: FsSolution {
        url: req.url.clone(),
        status: 200,
        cookies: solved.cookies,
        user_agent: solved.user_agent,
        headers: None,
        response: None,
      },
      status: "ok".to_string(),
      message: "Challenge solved!".to_string(),
      start_timestamp: start,
      end_timestamp: now_ms(),
      version: SHIM_VERSION.to_string(),
    },
    Err(e) => error_resp(&req.url, e, start),
  }
}

// ---- Loopback HTTP shim ----------------------------------------------------

/// The loopback FlareSolverr-v1 listener. Owns the OS-assigned ephemeral port and a
/// dedicated serving thread; drop/`shutdown` unblocks the thread. Not a public API —
/// only the loopback engine ever calls it.
pub struct CfShim {
  port: u16,
  server: Arc<tiny_http::Server>,
  handle: Mutex<Option<std::thread::JoinHandle<()>>>,
  /// Woken on shutdown to cancel any in-flight solve promptly (belt-and-braces
  /// with `MAX_SOLVE_WALL_MS`). The listener itself is stopped via `server.unblock()`;
  /// this only shortens a solve that is already running when the app exits.
  shutdown: Arc<tokio::sync::Notify>,
}

impl CfShim {
  /// Bind loopback, read back the ephemeral port, and spawn the serving thread. The
  /// `solver` runs off the tokio workers (on the serving thread via `block_on`), so the
  /// WebView cookie read is never on a sync command/main thread (Windows deadlock guard).
  pub fn start(solver: Arc<dyn CloudflareSolver>) -> Result<Self, String> {
    let server =
      tiny_http::Server::http(SHIM_BIND).map_err(|e| format!("bind cf shim: {e}"))?;
    let port = server
      .server_addr()
      .to_ip()
      .map(|a| a.port())
      .ok_or_else(|| "cf shim: no ip port".to_string())?;
    let server = Arc::new(server);
    let srv = server.clone();
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_srv = shutdown.clone();
    let handle = std::thread::Builder::new()
      .name("komika-cf-shim".to_string())
      .spawn(move || serve_loop(&srv, solver, shutdown_srv))
      .map_err(|e| format!("spawn cf shim thread: {e}"))?;
    log::info!(target: "suwayomi", "cloudflare shim listening on 127.0.0.1:{port}");
    Ok(CfShim {
      port,
      server,
      handle: Mutex::new(Some(handle)),
      shutdown,
    })
  }

  pub fn port(&self) -> u16 {
    self.port
  }

  /// The `http://127.0.0.1:<port>` URL to hand the engine via `setSettings`.
  pub fn url(&self) -> String {
    format!("http://127.0.0.1:{}", self.port())
  }

  /// Stop the listener and join the serving thread. Idempotent (safe to call twice).
  ///
  /// The join is now bounded: each `/v1` solve runs on its OWN task (not the serving
  /// thread), so the thread only ever loops over `incoming_requests()` and returns as
  /// soon as `unblock()` ends it — it never blocks on a ~60 s solve. `notify_waiters`
  /// additionally cancels any in-flight solve so it doesn't linger past app exit.
  pub fn shutdown(&self) {
    self.shutdown.notify_waiters();
    self.server.unblock();
    if let Some(h) = self.handle.lock().unwrap().take() {
      let _ = h.join();
    }
  }
}

impl Drop for CfShim {
  fn drop(&mut self) {
    // Unblock the serving thread and join it; the process is usually going down anyway.
    self.shutdown();
  }
}

/// Serve until `unblock()` ends `incoming_requests()`. The loop itself does no solving:
/// it routes fast (health/404) responses inline and dispatches each `/v1` solve onto its
/// own task, so a slow (~60 s) WebView solve can never head-of-line-block the next
/// request — nor the join at shutdown.
fn serve_loop(
  server: &tiny_http::Server,
  solver: Arc<dyn CloudflareSolver>,
  shutdown: Arc<tokio::sync::Notify>,
) {
  for request in server.incoming_requests() {
    handle_request(request, &solver, &shutdown);
  }
}

/// `application/json` body response.
fn json_response(resp: &FsResp) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
  let body = serde_json::to_vec(resp).unwrap_or_default();
  let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
    .expect("static content-type header");
  tiny_http::Response::from_data(body).with_header(header)
}

/// Route one request: `GET /` health, `POST /v1` solve; everything else 404. The health
/// and 404 paths respond inline (instant); the `/v1` solve is dispatched onto its own
/// task so a slow solve never blocks the listener.
fn handle_request(
  mut request: tiny_http::Request,
  solver: &Arc<dyn CloudflareSolver>,
  shutdown: &Arc<tokio::sync::Notify>,
) {
  use tiny_http::{Method, Response};

  let method = request.method().clone();
  let path = request.url().to_string();

  if method == Method::Get && (path == "/" || path.starts_with("/?")) {
    let _ = request.respond(Response::from_string("komika-cf-shim"));
    return;
  }
  if method != Method::Post || !(path == "/v1" || path.starts_with("/v1?")) {
    let _ = request.respond(Response::empty(404));
    return;
  }

  // Read the (small) request body synchronously on the listener thread — this is fast;
  // only the solve itself is slow. Then hand the request off to its own task so the
  // next `/v1` POST is serviced immediately instead of queuing behind a ~60 s solve.
  let mut body = String::new();
  if request.as_reader().read_to_string(&mut body).is_err() {
    let resp = error_resp("", "failed to read request body".to_string(), now_ms());
    let _ = request.respond(json_response(&resp));
    return;
  }

  let solver = solver.clone();
  let shutdown = shutdown.clone();
  // Dispatch onto Tauri's async runtime. `tiny_http::Request` is `Send`, so it moves
  // into the task and responds there once the solve completes.
  tauri::async_runtime::spawn(async move {
    let resp = match serde_json::from_str::<FsReq>(&body) {
      Ok(req) => solve_bounded(req, solver.as_ref(), &shutdown).await,
      Err(e) => error_resp("", format!("invalid FlareSolverr request: {e}"), now_ms()),
    };
    let _ = request.respond(json_response(&resp));
  });
}

/// Run one solve under two independent bounds so it can never hang the shim:
/// (1) the engine-requested `maxTimeout` is clamped to `MAX_SOLVE_WALL_MS`, which the
/// solver enforces via its OWN deadline — so it always closes the hidden window on
/// timeout (no leak); (2) a shutdown signal cancels an in-flight solve promptly at app
/// exit. On either bound the engine gets an `error_resp` → clean server-fetch fallback.
async fn solve_bounded(
  mut req: FsReq,
  solver: &dyn CloudflareSolver,
  shutdown: &tokio::sync::Notify,
) -> FsResp {
  let start = now_ms();
  let url = req.url.clone();
  req.max_timeout = Some(
    req
      .max_timeout
      .unwrap_or(DEFAULT_TIMEOUT_MS)
      .min(MAX_SOLVE_WALL_MS),
  );
  tokio::select! {
    biased;
    // Only fires when the app is shutting down (WebViews torn down with the process),
    // so dropping the in-flight `handle_v1` future here cannot leak a window.
    _ = shutdown.notified() => error_resp(&url, "cf shim shutting down".to_string(), start),
    resp = handle_v1(req, solver) => resp,
  }
}

// ---- Real Tauri-WebView solver ---------------------------------------------

/// Solves CF challenges by opening the challenge URL in a hidden Tauri WebView and
/// polling its cookie store (incl. HTTP-only/secure cookies like `cf_clearance`) until
/// the clearance cookie appears. Interactive challenges surface the window after a grace
/// period. Every failure path is a clean `Err` (→ shim error → engine fallback), never a
/// panic or an unbounded hang.
pub struct WebviewSolver {
  app: AppHandle,
}

impl WebviewSolver {
  pub fn new(app: AppHandle) -> Self {
    WebviewSolver { app }
  }
}

impl CloudflareSolver for WebviewSolver {
  fn solve<'a>(
    &'a self,
    url: &'a str,
    seed_cookies: &'a [FsCookie],
    timeout: Duration,
  ) -> SolveFuture<'a> {
    let app = self.app.clone();
    let url = url.to_string();
    // `seed_cookies` are the site's existing non-cf cookies. Tauri exposes no pre-nav
    // cookie-set API, so we cannot inject them into the challenge WebView; a fresh CF
    // JS challenge does not need them. Recorded as a known limitation.
    let _ = seed_cookies;
    Box::pin(async move { solve_in_webview(app, url, timeout).await })
  }
}

/// Monotonic label so re-entrant solves never collide on a WebView window label.
fn challenge_label() -> String {
  static N: AtomicU64 = AtomicU64::new(0);
  format!("komika-cf-{}", N.fetch_add(1, Ordering::Relaxed))
}

/// Core WebView solve: build a hidden challenge window, poll for `cf_clearance`, always
/// close the window before returning.
async fn solve_in_webview(app: AppHandle, url: String, timeout: Duration) -> Result<Solved, String> {
  let parsed = Url::parse(&url).map_err(|e| format!("invalid url: {e}"))?;
  match parsed.scheme() {
    "http" | "https" => {}
    other => return Err(format!("unsupported scheme: {other}")),
  }
  let label = challenge_label();
  if let Err(e) = build_challenge_window(&app, &label, parsed.clone()) {
    // The builder can finish just AFTER the `recv_timeout` fired: in that race the
    // hidden window is created but we're on the error path, so best-effort close it
    // too — otherwise a late build leaks a hidden window for the app's lifetime.
    close_window(&app, &label);
    return Err(e);
  }
  let result = poll_for_clearance(&app, &label, &parsed, timeout).await;
  close_window(&app, &label);
  result
}

/// Build the hidden challenge WebView on the **main thread** (macOS/Windows require
/// window creation there) with the fixed [`CHALLENGE_UA`]. Blocks (bounded) on a channel
/// for the builder result.
fn build_challenge_window(app: &AppHandle, label: &str, url: Url) -> Result<(), String> {
  let (tx, rx) = std::sync::mpsc::channel();
  let app_for_main = app.clone();
  let label = label.to_string();
  app
    .run_on_main_thread(move || {
      let res = WebviewWindowBuilder::new(&app_for_main, &label, WebviewUrl::External(url))
        .title("Verifying your connection…")
        .user_agent(CHALLENGE_UA)
        .visible(false)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string());
      let _ = tx.send(res);
    })
    .map_err(|e| format!("schedule window build: {e}"))?;
  rx.recv_timeout(Duration::from_secs(10))
    .map_err(|e| format!("window build did not complete: {e}"))?
}

/// Poll the WebView's cookie store for `cf_clearance` until it appears or `timeout`
/// elapses. `cookies_for_url` runs here (in an async task, off the main thread) to avoid
/// the documented Windows sync-command deadlock. After [`INTERACTIVE_GRACE`] with no
/// clearance, surface the window so the user can complete an interactive Turnstile
/// (minimal fallback: show + keep polling; a full Turnstile prompt UX is out of scope).
async fn poll_for_clearance(
  app: &AppHandle,
  label: &str,
  url: &Url,
  timeout: Duration,
) -> Result<Solved, String> {
  let deadline = Instant::now() + timeout;
  let show_at = Instant::now() + INTERACTIVE_GRACE.min(timeout);
  let mut shown = false;
  loop {
    let win = app
      .get_webview_window(label)
      .ok_or_else(|| "challenge window closed unexpectedly".to_string())?;
    if let Ok(cookies) = win.cookies_for_url(url.clone()) {
      if cookies.iter().any(|c| c.name() == COOKIE_NAME) {
        return Ok(Solved {
          cookies: to_fs_cookies(&cookies),
          user_agent: CHALLENGE_UA.to_string(),
        });
      }
    }
    if Instant::now() >= deadline {
      return Err(format!(
        "cloudflare challenge unsolved after {}s",
        timeout.as_secs()
      ));
    }
    if !shown && Instant::now() >= show_at {
      show_window(app, label);
      shown = true;
    }
    tokio::time::sleep(POLL_INTERVAL).await;
  }
}

/// Convert WebView cookies to FlareSolverr-v1 cookies (all of them, so the engine caches
/// the full set for the host, not just `cf_clearance`).
fn to_fs_cookies(cookies: &[tauri::webview::Cookie<'static>]) -> Vec<FsCookie> {
  cookies
    .iter()
    .map(|c| FsCookie {
      name: c.name().to_string(),
      value: c.value().to_string(),
      domain: c.domain().map(str::to_string),
      path: c.path().map(str::to_string),
      expires: c.expires_datetime().map(|dt| dt.unix_timestamp() as f64),
      http_only: c.http_only(),
      secure: c.secure(),
    })
    .collect()
}

/// Best-effort `show()` on the main thread (interactive fallback).
fn show_window(app: &AppHandle, label: &str) {
  let app2 = app.clone();
  let label = label.to_string();
  let _ = app.run_on_main_thread(move || {
    if let Some(win) = app2.get_webview_window(&label) {
      let _ = win.show();
    }
  });
}

/// Best-effort `close()` on the main thread. Never propagates an error (cleanup path).
fn close_window(app: &AppHandle, label: &str) {
  let app2 = app.clone();
  let label = label.to_string();
  let _ = app.run_on_main_thread(move || {
    if let Some(win) = app2.get_webview_window(&label) {
      let _ = win.close();
    }
  });
}

// ---- Settings wiring -------------------------------------------------------

/// Point the engine's CF interceptor at our shim: enable the FlareSolverr hook, set its
/// URL to our loopback shim, and keep `flareSolverrAsResponseFallback=false` (so the
/// engine stays on the cookie-injection path, not the response-fallback path). Best
/// effort: a failure only logs — CF sources then just fall back to server-fetch. Called
/// once per engine `Ready` transition (the shim URL is only known post-broker).
pub async fn apply_settings(client: reqwest::Client, port: u16, shim_url: String) {
  let mutation = "mutation SetCf($url: String!) { \
    setSettings(input: { settings: { \
      flareSolverrEnabled: true, \
      flareSolverrUrl: $url, \
      flareSolverrAsResponseFallback: false \
    } }) { settings { flareSolverrEnabled flareSolverrUrl } } }";
  let body = serde_json::json!({
    "query": mutation,
    "variables": { "url": shim_url },
  });
  let resp = client
    .post(format!("http://127.0.0.1:{port}/api/graphql"))
    .header(reqwest::header::CONTENT_TYPE, "application/json")
    .body(body.to_string())
    .send()
    .await;
  match resp {
    Ok(r) => match r.bytes().await {
      Ok(bytes) => {
        let ok = serde_json::from_slice::<serde_json::Value>(&bytes)
          .ok()
          .and_then(|v| {
            v.pointer("/data/setSettings/settings/flareSolverrEnabled")
              .and_then(serde_json::Value::as_bool)
          })
          .unwrap_or(false);
        if ok {
          log::info!(target: "suwayomi", "cloudflare shim wired into engine ({shim_url})");
        } else {
          log::warn!(
            target: "suwayomi",
            "setSettings did not confirm flareSolverr: {}",
            String::from_utf8_lossy(&bytes)
          );
        }
      }
      Err(e) => log::warn!(target: "suwayomi", "setSettings read failed: {e}"),
    },
    Err(e) => log::warn!(target: "suwayomi", "setSettings request failed: {e}"),
  }
}

/// Start the loopback shim backed by the real [`WebviewSolver`]. Best-effort: a bind
/// failure only logs and returns `None` (CF sources then fall back to server-fetch). The
/// whole subsystem is inert until the engine POSTs to it, which only happens once the
/// engine is running and hits a CF challenge.
pub fn start(app: &AppHandle) -> Option<Arc<CfShim>> {
  let solver: Arc<dyn CloudflareSolver> = Arc::new(WebviewSolver::new(app.clone()));
  match CfShim::start(solver) {
    Ok(shim) => Some(Arc::new(shim)),
    Err(e) => {
      log::warn!(target: "suwayomi", "cloudflare shim not started: {e}");
      None
    }
  }
}

// ---- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;

  /// A solver that returns a canned clearance without any WebView.
  struct FakeOkSolver;
  impl CloudflareSolver for FakeOkSolver {
    fn solve<'a>(
      &'a self,
      _url: &'a str,
      _seed: &'a [FsCookie],
      _timeout: Duration,
    ) -> SolveFuture<'a> {
      Box::pin(async move {
        Ok(Solved {
          cookies: vec![FsCookie {
            name: "cf_clearance".to_string(),
            value: "TOKEN123".to_string(),
            domain: Some(".example.com".to_string()),
            path: Some("/".to_string()),
            expires: Some(1_700_000_000.0),
            http_only: Some(true),
            secure: Some(true),
          }],
          user_agent: "UA/1.0".to_string(),
        })
      })
    }
  }

  /// A solver that always fails (timeout / dismissed challenge).
  struct FakeErrSolver;
  impl CloudflareSolver for FakeErrSolver {
    fn solve<'a>(
      &'a self,
      _url: &'a str,
      _seed: &'a [FsCookie],
      _timeout: Duration,
    ) -> SolveFuture<'a> {
      Box::pin(async move { Err("solve timed out".to_string()) })
    }
  }

  fn get_req() -> FsReq {
    FsReq {
      cmd: "request.get".to_string(),
      url: "https://example.com/manga".to_string(),
      cookies: None,
      max_timeout: Some(30_000),
      post_data: None,
    }
  }

  #[tokio::test]
  async fn handle_v1_maps_ok_solver_to_flaresolverr_ok() {
    let resp = handle_v1(get_req(), &FakeOkSolver).await;
    assert_eq!(resp.status, "ok");
    // Must not trip the engine's "not detected" no-challenge sentinel.
    assert!(!resp.message.contains("not detected"));
    assert_eq!(resp.solution.status, 200);
    assert_eq!(resp.solution.url, "https://example.com/manga");
    assert_eq!(resp.solution.user_agent, "UA/1.0");
    let cf = resp
      .solution
      .cookies
      .iter()
      .find(|c| c.name == "cf_clearance")
      .expect("cf_clearance present");
    assert_eq!(cf.value, "TOKEN123");
    assert_eq!(cf.domain.as_deref(), Some(".example.com"));
    assert_eq!(cf.http_only, Some(true));
  }

  #[tokio::test]
  async fn handle_v1_maps_err_solver_to_fallback_contract() {
    let resp = handle_v1(get_req(), &FakeErrSolver).await;
    // status:"error" + non-2xx solution.status makes the engine throw → server-fetch.
    assert_eq!(resp.status, "error");
    assert!(resp.message.contains("solve timed out"));
    assert!(
      !(200..=299).contains(&resp.solution.status),
      "solution.status must be non-2xx so the engine falls back"
    );
    assert!(resp.solution.cookies.is_empty());
  }

  #[tokio::test]
  async fn handle_v1_rejects_unsupported_cmd() {
    let mut req = get_req();
    req.cmd = "sessions.create".to_string();
    let resp = handle_v1(req, &FakeOkSolver).await;
    assert_eq!(resp.status, "error");
    assert!(resp.message.contains("unsupported cmd"));
  }

  #[test]
  fn flaresolverr_request_deserializes_v1_shape() {
    // The exact request shape stock Suwayomi POSTs (findings §A3), incl. keys we ignore.
    let raw = r#"{
      "cmd": "request.get",
      "url": "https://target.example/read",
      "session": "suwayomi",
      "session_ttl_minutes": 15,
      "returnOnlyCookies": true,
      "maxTimeout": 60000,
      "cookies": [ { "name": "sess", "value": "abc" } ],
      "postData": null
    }"#;
    let req: FsReq = serde_json::from_str(raw).expect("parse v1 request");
    assert_eq!(req.cmd, "request.get");
    assert_eq!(req.url, "https://target.example/read");
    assert_eq!(req.max_timeout, Some(60_000));
    let cookies = req.cookies.expect("cookies");
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0].name, "sess");
    assert_eq!(cookies[0].value, "abc");
  }

  #[test]
  fn flaresolverr_response_roundtrips_and_matches_v1_shape() {
    let resp = FsResp {
      solution: FsSolution {
        url: "https://target.example/read".to_string(),
        status: 200,
        cookies: vec![FsCookie {
          name: "cf_clearance".to_string(),
          value: "xyz".to_string(),
          domain: Some(".target.example".to_string()),
          path: Some("/".to_string()),
          expires: Some(1_700_000_000.0),
          http_only: Some(true),
          secure: Some(true),
        }],
        user_agent: CHALLENGE_UA.to_string(),
        headers: None,
        response: None,
      },
      status: "ok".to_string(),
      message: "Challenge solved!".to_string(),
      start_timestamp: 1,
      end_timestamp: 2,
      version: SHIM_VERSION.to_string(),
    };
    let json = serde_json::to_value(&resp).expect("serialize");
    // The exact keys the engine's FlareSolverResponse deserializer reads.
    assert_eq!(json["status"], "ok");
    assert_eq!(json["solution"]["status"], 200);
    assert_eq!(json["solution"]["userAgent"], CHALLENGE_UA);
    assert_eq!(json["solution"]["cookies"][0]["name"], "cf_clearance");
    assert_eq!(json["solution"]["cookies"][0]["httpOnly"], true);
    assert_eq!(json["startTimestamp"], 1);
    assert_eq!(json["endTimestamp"], 2);
    let back: FsResp = serde_json::from_value(json).expect("deserialize");
    assert_eq!(back, resp);
  }

  #[test]
  fn shim_binds_loopback_only() {
    // The bind address is hardcoded to loopback; the shim can never accept a
    // non-loopback peer. This is the SSRF/exposure containment for the shim.
    let addr: std::net::SocketAddr =
      "127.0.0.1:0".parse().expect("parse bind addr");
    assert!(addr.ip().is_loopback(), "shim must bind loopback only");
    assert_eq!(SHIM_BIND, "127.0.0.1:0");
  }

  #[test]
  fn shim_starts_on_loopback_and_serves_health() {
    use std::io::{Read, Write};
    let shim = CfShim::start(Arc::new(FakeOkSolver)).expect("shim starts");
    assert!(shim.port() > 0);
    assert_eq!(shim.url(), format!("http://127.0.0.1:{}", shim.port()));
    // Hit the health route over a real loopback socket (no WebView, no block_on path).
    let mut stream =
      std::net::TcpStream::connect(("127.0.0.1", shim.port())).expect("connect shim");
    stream
      .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
      .expect("write request");
    let mut buf = String::new();
    stream.read_to_string(&mut buf).expect("read response");
    assert!(buf.contains("200 OK"), "health response: {buf}");
    assert!(buf.contains("komika-cf-shim"));
    shim.shutdown();
  }
}
