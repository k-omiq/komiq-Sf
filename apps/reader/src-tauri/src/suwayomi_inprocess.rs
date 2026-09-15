//! In-process embedded Suwayomi engine: REAL iOS devices and Android.
//!
//! A phone has no `java` to start, and neither system lets an app run one as a child
//! process. So unlike the desktop supervisor (`suwayomi.rs`, a `java` child-process
//! sidecar) this module boots the pinned `Suwayomi-Server.jar` INSIDE the app process: it
//! creates a JVM with `JNI_CreateJavaVM`, then invokes
//! `suwayomi.tachidesk.MainKt.main(String[])` (the jar's manifest Main-Class) on a
//! dedicated native thread.
//!
//! This file is everything the two systems share: the supervisor state, the port broker,
//! the readiness gate and heartbeat, the Java exception capture, and the four IPC
//! commands with their path guard. What differs is where the JVM and its files come from,
//! and that is the `platform` child module, one file per system with the same three items
//! (`prepare`, `create_java_vm`, `JVM_OPTIONS`):
//!   * `suwayomi_inprocess_ios.rs`: the Zero (no JIT) OpenJDK, linked statically into the
//!     app binary; runtime files and jar inside the app bundle.
//!   * `suwayomi_inprocess_android.rs`: a HotSpot `libjvm.so` loaded with `dlopen` from
//!     the app's native library dir; runtime files and jar unpacked from the APK.
//!
//! Same public surface as desktop: `SuwayomiSupervisor` (managed state),
//! `start(app)`, and the four `#[tauri::command]`s with identical names and
//! JSON shapes, so the JS `LocalSuwayomiBackend` works unchanged.
//!
//! Security posture, the same as desktop's: the engine binds 127.0.0.1 only, and it
//! requires HTTP Basic auth with a password made once per app run (`engine_auth.rs`,
//! shared with desktop). The bind keeps out the network; the password keeps out other
//! software on the phone, which can reach loopback as easily as this app can. JS
//! reaches the engine only through the `suwayomi_gql` / `suwayomi_image` IPC proxies
//! and never sees the password. Every request this module makes goes through the ONE
//! client the supervisor owns, which carries the credential as a default header. The
//! password is in this process's memory and in `server.conf` inside the app's data
//! container, nowhere else; it is not logged.
//!
//! Lifecycle limits (deliberate for this gate, N4.8 refines):
//!   * ONE boot per process. A JVM cannot be re-created after DestroyJavaVM in
//!     the same process, so there is no crash/restart supervision loop — a dead
//!     engine transitions to `degraded` and stays there until app relaunch.
//!   * `stop_and_wait` does NOT call DestroyJavaVM: it would block until the
//!     engine's non-daemon server threads exit (i.e. forever), and the process is
//!     about to be torn down by the OS anyway. It just flips the
//!     state to `stopped`; the JVM dies with the process. Best-effort by design.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jni_sys::{
  jint, jobject, jthrowable, jvalue, JavaVM, JavaVMInitArgs, JavaVMOption, JNIEnv, JNI_OK,
  JNI_TRUE, JNI_VERSION_1_8,
};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};

// The engine credential, the authenticated client and the `server.conf` template: one
// implementation, shared with the desktop supervisor (`suwayomi.rs`).
#[path = "engine_auth.rs"]
mod engine_auth;

// Where the JVM and its files come from on this system (see the module docs).
#[cfg(target_os = "ios")]
#[path = "suwayomi_inprocess_ios.rs"]
mod platform;
#[cfg(target_os = "android")]
#[path = "suwayomi_inprocess_android.rs"]
mod platform;

#[cfg(target_os = "android")]
pub use platform::runtime_bundled;

/// Cold-start budget for the readiness gate. The desktop JIT JVM needs ~5 s; the
/// Zero interpreter on an iPhone needs about 20 s, and a first start on Android also
/// unpacks the runtime, so the budget is deliberately generous.
const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// How often to poll `aboutServer` while waiting for readiness.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Per-request timeout for the readiness poll and the `suwayomi_gql` proxy.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard cap on an engine image body (kept in lockstep with lib.rs / desktop).
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024; // 32 MiB
/// Native stack for the thread that owns the VM and runs Java `main` — the Zero
/// interpreter (iOS) burns C stack per Java frame, so give it far more than `-Xss`.
const JVM_THREAD_STACK: usize = 16 * 1024 * 1024;
/// The garbage collector, set explicitly because the default is wrong for a phone. A
/// phone has 6 or more cores and 6 GB, so HotSpot calls it a "server-class machine" and picks
/// G1: on the test phone that was 6 parallel, 2 concurrent and 6 refinement worker
/// threads, plus G1's own native tables, all for a 256 MB heap (`-Xlog:gc+init`,
/// 2026-10-06). The serial collector has no worker threads and almost no memory of its
/// own, and it is what HotSpot itself picks for a small machine. Under Zero (iOS) there
/// is a second reason: every G1 write barrier runs in the bytecode interpreter. Cold
/// start with it on the same iPhone: 20.1 s, against 19.1 s with G1.
const GC_OPT: &str = "-XX:+UseSerialGC";
/// Settings on top of the shared `server.conf` template that only the phone engine gets.
/// It fetches chapters for the reader and keeps no library of its own, so the two timed
/// jobs of a Suwayomi server have nothing to do here: the automated backup (daily) and
/// the library update (every 12 h). Each ran at the first start of the app after it came
/// due. `0` switches each off.
const PHONE_SERVER_CONF: &str = "server.backupInterval = 0\nserver.globalUpdateInterval = 0\n";
/// Longest Java stack trace, in lines, copied into the Rust log (a
/// `StackOverflowError` has 1,024 frames).
const MAX_TRACE_LINES: usize = 400;
/// Longest exception summary carried in an error string.
const MAX_SUMMARY_CHARS: usize = 300;
/// How often a Ready engine is re-checked for liveness (see `spawn_heartbeat`).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Per-heartbeat request timeout — deliberately shorter than the interval so a hung
/// engine is observed rather than queued behind the client's 60 s default.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
/// Consecutive heartbeat failures before the engine is declared dead. Three misses
/// (≈90 s) rides out a long GC pause or an app suspend/resume without a false
/// terminal degrade.
const HEARTBEAT_FAILURES_TO_DEGRADE: u32 = 3;
/// Bounded pre-VM boot attempts. A process can create a JVM exactly once
/// (`JNI_CreateJavaVM` is irreversible), so EVERYTHING that can fail before it —
/// port broker, port re-verification, `server.conf` write — is wrapped in this
/// retry: a transient port steal or filesystem hiccup becomes a retry with a fresh
/// port instead of a permanent `degraded` for the whole app session.
const MAX_BOOT_ATTEMPTS: u32 = 5;
/// Short backoff between pre-VM boot attempts (worst case ≈ 4 × this ≈ 0.6 s, only
/// on the rare retry path — startup is otherwise unaffected).
const BOOT_RETRY_BACKOFF: Duration = Duration::from_millis(150);
/// Appended to every TERMINAL degrade reason surfaced to the UI. Once the VM has
/// been created a second boot is impossible in-process (JNI limitation), so the only
/// recovery is an app relaunch — say so, rather than leaving a silent dead engine.
const RESTART_HINT: &str = " — restart the app to retry.";

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

/// JSON shape returned by `suwayomi_status` — identical to desktop's.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Status {
  pub state: String,
  pub version: Option<String>,
  pub last_error: Option<String>,
}

struct Inner {
  port: Option<u16>,
  state: EngineState,
  version: Option<String>,
  last_error: Option<String>,
}

/// Owns the embedded engine's shared state and HTTP client. Unlike desktop there
/// is no child process, no restart loop, and no lockfile — the VM lives and dies
/// with this process.
pub struct SuwayomiSupervisor {
  inner: Mutex<Inner>,
  client: reqwest::Client,
  /// One JVM per process, ever (see module docs).
  boot_attempted: AtomicBool,
}

impl SuwayomiSupervisor {
  pub fn new() -> Self {
    // The ONLY client in this module. It carries the engine credential as a default
    // header, so the readiness poll, the heartbeat and both IPC proxies are authenticated.
    let client = engine_auth::build_client(HTTP_TIMEOUT);
    SuwayomiSupervisor {
      inner: Mutex::new(Inner {
        port: None,
        state: EngineState::Starting,
        version: None,
        last_error: None,
      }),
      client,
      boot_attempted: AtomicBool::new(false),
    }
  }

  fn set_ready(&self, port: u16, version: String) {
    let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    g.port = Some(port);
    g.state = EngineState::Ready;
    g.version = Some(version);
    g.last_error = None;
  }

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

  /// Terminal degrade for the single-boot engine: flip to `Degraded` and record the
  /// reason WITH the app-relaunch hint. JNI can't recreate the VM in-process, so a
  /// degrade here is final — surfacing an actionable `lastError` keeps the failure
  /// legible to the UI instead of a silently dead engine (JS reads `{state,lastError}`).
  fn degrade(&self, reason: impl std::fmt::Display) {
    self.transition(EngineState::Degraded, Some(format!("{reason}{RESTART_HINT}")));
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

  fn is_ready(&self) -> bool {
    let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    g.state == EngineState::Ready
  }

  fn ready_endpoint(&self) -> Option<(u16, reqwest::Client)> {
    let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
    if g.state == EngineState::Ready {
      g.port.map(|p| (p, self.client.clone()))
    } else {
      None
    }
  }

  /// Best-effort shutdown (see module docs): the in-process JVM is NOT destroyed
  /// (DestroyJavaVM would block on the engine's non-daemon threads); we only
  /// reflect `Stopped` so the JS side stops routing to the engine.
  pub async fn stop_and_wait(&self) {
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

/// Is `port` bindable on loopback right now? Binding `127.0.0.1:port` (not `:0`) and
/// immediately dropping proves nobody else holds it at this instant — the last check
/// we can make before the (irreversible) VM creation.
fn port_is_bindable(port: u16) -> bool {
  std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Acquire a loopback port for the engine with a BOUNDED pre-VM retry. Each attempt
/// brokers a fresh ephemeral port (`broker`) and re-verifies it's still free right
/// now (`is_bindable`); a port stolen in the broker→verify window, or a transient
/// broker error, retries with a fresh port (via `backoff`) instead of permanently
/// degrading the single-boot engine. Returns the secured port, or an error once
/// `MAX_BOOT_ATTEMPTS` is exhausted.
///
/// The closures are injected so the retry logic is unit-testable without real sockets.
///
/// Residual race (honest scope): this is the LAST check possible before the
/// irreversible `JNI_CreateJavaVM`; the engine binds the port fresh, seconds later,
/// from inside the JVM. A steal in that final window still fails the readiness gate
/// (→ honest degrade). Closing it entirely needs FD hand-off or an engine-side
/// bind-retry, neither available: the engine ships as a compiled jar with no
/// adoptable-FD entrypoint and no reachable source in this repo.
fn acquire_boot_port(
  mut broker: impl FnMut() -> std::io::Result<u16>,
  mut is_bindable: impl FnMut(u16) -> bool,
  mut backoff: impl FnMut(u32),
) -> Result<u16, String> {
  let mut last_err = String::from("no boot attempt was made");
  for attempt in 1..=MAX_BOOT_ATTEMPTS {
    match broker() {
      Ok(port) if is_bindable(port) => return Ok(port),
      Ok(port) => {
        last_err = format!("brokered port {port} was taken before boot");
        log::warn!(
          target: "suwayomi",
          "boot port attempt {attempt}/{MAX_BOOT_ATTEMPTS}: {last_err}"
        );
      }
      Err(e) => {
        last_err = format!("port broker: {e}");
        log::warn!(
          target: "suwayomi",
          "boot port attempt {attempt}/{MAX_BOOT_ATTEMPTS}: {last_err}"
        );
      }
    }
    if attempt < MAX_BOOT_ATTEMPTS {
      backoff(attempt);
    }
  }
  Err(format!(
    "could not secure a free loopback port after {MAX_BOOT_ATTEMPTS} attempts: {last_err}"
  ))
}

/// The engine's temp dir, pinned as `java.io.tmpdir` (see `jvm_options`). Kept in
/// one place so `prepare_data_dir` and `run_jvm` cannot disagree about it.
fn engine_tmp_dir(data_dir: &Path) -> PathBuf {
  data_dir.join("tmp")
}

/// The phone engine's `server.conf`: the template desktop also uses (Basic auth on), then
/// the phone-only settings.
fn render_server_conf(port: u16) -> String {
  format!("{}{PHONE_SERVER_CONF}", engine_auth::render_server_conf(port))
}

/// Write `server.conf` into the data dir (creating it), and create the app-private temp
/// dir the VM is pointed at.
fn prepare_data_dir(data_dir: &Path, port: u16) -> Result<(), String> {
  std::fs::create_dir_all(data_dir).map_err(|e| format!("create data dir: {e}"))?;
  // Must exist BEFORE the VM starts: the JVM does not create `java.io.tmpdir`, it
  // just fails every `File.createTempFile` if the directory is missing.
  std::fs::create_dir_all(engine_tmp_dir(data_dir))
    .map_err(|e| format!("create engine temp dir: {e}"))?;
  std::fs::write(data_dir.join("server.conf"), render_server_conf(port))
    .map_err(|e| format!("write server.conf: {e}"))?;
  let kcef = data_dir.join("bin").join("kcef");
  let _ = std::fs::remove_dir_all(&kcef);
  let _ = std::fs::remove_file(&kcef);
  Ok(())
}

/// Poll `POST /api/graphql { aboutServer { version } }` until a version comes
/// back or the timeout elapses (same shape as desktop's readiness gate).
async fn poll_ready(
  client: &reqwest::Client,
  port: u16,
  timeout: Duration,
) -> Result<String, String> {
  let url = format!("http://127.0.0.1:{port}/api/graphql");
  let body = json!({ "query": "{ aboutServer { version } }" }).to_string();
  let deadline = Instant::now() + timeout;
  loop {
    if let Ok(resp) = client
      .post(&url)
      .header(reqwest::header::CONTENT_TYPE, "application/json")
      .body(body.clone())
      .send()
      .await
    {
      if let Ok(bytes) = resp.bytes().await {
        if let Ok(val) = serde_json::from_slice::<Value>(&bytes) {
          if let Some(ver) = val
            .pointer("/data/aboutServer/version")
            .and_then(|v| v.as_str())
          {
            return Ok(ver.to_string());
          }
        }
      }
    }
    if Instant::now() >= deadline {
      return Err(format!(
        "engine did not become ready within {}s",
        timeout.as_secs()
      ));
    }
    tokio::time::sleep(POLL_INTERVAL).await;
  }
}

/// One-line form of `Throwable.toString()` (class name and message) for an error
/// string: the first line only, bounded, because it travels to the UI as `lastError`.
fn exception_summary(to_string: Option<&str>) -> String {
  let line = to_string
    .and_then(|s| s.lines().next())
    .map(str::trim)
    .filter(|s| !s.is_empty());
  match line {
    Some(l) if l.chars().count() > MAX_SUMMARY_CHARS => {
      format!("{}…", l.chars().take(MAX_SUMMARY_CHARS).collect::<String>())
    }
    Some(l) => l.to_string(),
    None => "<exception text unavailable>".to_string(),
  }
}

/// A Java stack trace as it goes into the Rust log: at most `MAX_TRACE_LINES` lines,
/// with a count of the lines that were cut.
fn bounded_trace(trace: &str) -> String {
  let lines: Vec<&str> = trace.lines().map(str::trim_end).collect();
  if lines.len() <= MAX_TRACE_LINES {
    return lines.join("\n");
  }
  format!(
    "{}\n\t... {} more lines not logged",
    lines[..MAX_TRACE_LINES].join("\n"),
    lines.len() - MAX_TRACE_LINES
  )
}

/// Is a Java exception pending on `env`?
unsafe fn exception_pending(env: *mut JNIEnv) -> bool {
  matches!((**env).ExceptionCheck, Some(check) if check(env) == JNI_TRUE)
}

/// Clear a pending Java exception, if any.
unsafe fn clear_exception(env: *mut JNIEnv) {
  if exception_pending(env) {
    if let Some(clear) = (**env).ExceptionClear {
      clear(env);
    }
  }
}

/// `Some(handle)` when a JNI call returned a non-null handle and threw nothing. A
/// failure inside the exception reporter itself is cleared and gives `None`: the
/// reporter must not leave a second exception pending.
unsafe fn checked<T>(env: *mut JNIEnv, handle: *mut T) -> Option<*mut T> {
  if exception_pending(env) {
    clear_exception(env);
    return None;
  }
  (!handle.is_null()).then_some(handle)
}

/// `obj.toString()` as a Rust string.
unsafe fn java_to_string(env: *mut JNIEnv, obj: jobject) -> Option<String> {
  let f = &**env;
  let cls = checked(env, (f.GetObjectClass?)(env, obj))?;
  let mid = checked(
    env,
    (f.GetMethodID?)(env, cls, c"toString".as_ptr(), c"()Ljava/lang/String;".as_ptr()),
  )?;
  let text = checked(env, (f.CallObjectMethodA?)(env, obj, mid, ptr::null()))?;
  let chars = (f.GetStringUTFChars?)(env, text, ptr::null_mut());
  if chars.is_null() {
    clear_exception(env);
    return None;
  }
  let out = CStr::from_ptr(chars).to_string_lossy().into_owned();
  (f.ReleaseStringUTFChars?)(env, text, chars);
  Some(out)
}

/// The output of `thrown.printStackTrace(PrintWriter)`: class, message, frames and the
/// whole "Caused by" chain, the same text `ExceptionDescribe` sends to stderr.
unsafe fn stack_trace_text(env: *mut JNIEnv, thrown: jthrowable) -> Option<String> {
  let f = &**env;
  let find_class = f.FindClass?;
  let method_id = f.GetMethodID?;
  let new_object = f.NewObjectA?;
  let call_void = f.CallVoidMethodA?;

  let writer_cls = checked(env, find_class(env, c"java/io/StringWriter".as_ptr()))?;
  let writer_init = checked(env, method_id(env, writer_cls, c"<init>".as_ptr(), c"()V".as_ptr()))?;
  let writer = checked(env, new_object(env, writer_cls, writer_init, ptr::null()))?;

  let printer_cls = checked(env, find_class(env, c"java/io/PrintWriter".as_ptr()))?;
  let printer_init = checked(
    env,
    method_id(env, printer_cls, c"<init>".as_ptr(), c"(Ljava/io/Writer;)V".as_ptr()),
  )?;
  let printer = checked(
    env,
    new_object(env, printer_cls, printer_init, [jvalue { l: writer }].as_ptr()),
  )?;

  let throwable_cls = checked(env, find_class(env, c"java/lang/Throwable".as_ptr()))?;
  let print = checked(
    env,
    method_id(
      env,
      throwable_cls,
      c"printStackTrace".as_ptr(),
      c"(Ljava/io/PrintWriter;)V".as_ptr(),
    ),
  )?;
  call_void(env, thrown, print, [jvalue { l: printer }].as_ptr());
  clear_exception(env);
  let flush = checked(env, method_id(env, printer_cls, c"flush".as_ptr(), c"()V".as_ptr()))?;
  call_void(env, printer, flush, ptr::null());
  clear_exception(env);
  java_to_string(env, writer)
}

/// Take the pending Java exception (if any) off `env`, write its class, message and
/// stack trace to the Rust log under `context`, and return its one-line summary for the
/// caller's error string.
///
/// `ExceptionDescribe` is not used: it prints to stderr, and on a phone nothing reads
/// the app's stderr, so the cause of a failed boot was lost (found 2026-10-05).
unsafe fn take_exception(env: *mut JNIEnv, context: &str) -> Option<String> {
  let f = &**env;
  // The local frame frees every reference made below. This thread never returns to
  // Java, so nothing else would.
  let framed = matches!(f.PushLocalFrame, Some(push) if push(env, 16) == JNI_OK);
  let thrown = match f.ExceptionOccurred {
    Some(occurred) => occurred(env),
    None => ptr::null_mut(),
  };
  let summary = if thrown.is_null() {
    None
  } else {
    // No other JNI call is allowed while an exception is pending.
    clear_exception(env);
    let summary = exception_summary(java_to_string(env, thrown).as_deref());
    match stack_trace_text(env, thrown) {
      Some(trace) => log::error!(
        target: "suwayomi",
        "Java exception, {context}: {summary}\n{}",
        bounded_trace(&trace)
      ),
      None => log::error!(
        target: "suwayomi",
        "Java exception, {context}: {summary} (no stack trace available)"
      ),
    }
    Some(summary)
  };
  if framed {
    if let Some(pop) = f.PopLocalFrame {
      pop(env, ptr::null_mut());
    }
  }
  summary
}

/// `what`, plus the summary of the pending Java exception when there is one (which is
/// logged and cleared).
unsafe fn jni_failure(env: *mut JNIEnv, what: &str) -> String {
  match take_exception(env, what) {
    Some(summary) => format!("{what}: {summary}"),
    None => what.to_string(),
  }
}

/// Invoke `suwayomi.tachidesk.MainKt.main(String[])` on the CURRENT thread, which the
/// caller has already attached to the VM. Returns when Java `main` returns (the
/// engine's server threads keep running), or `Err` if the class/method/argv could not
/// be resolved or `main` threw. Attach/detach pairing is the caller's job — see
/// `run_jvm`'s epilogue.
///
/// # Safety
/// `env` must be a valid `JNIEnv*` for the calling thread.
unsafe fn invoke_main(env: *mut JNIEnv) -> Result<(), String> {
  // Manifest Main-Class of Suwayomi-Server.jar v2.3.2243 (verified from
  // META-INF/MANIFEST.MF): suwayomi.tachidesk.MainKt.
  let main_cls_name = CString::new("suwayomi/tachidesk/MainKt").unwrap();
  let main_cls = ((**env).FindClass.unwrap())(env, main_cls_name.as_ptr());
  if main_cls.is_null() {
    return Err(jni_failure(
      env,
      "FindClass suwayomi/tachidesk/MainKt failed (classpath?)",
    ));
  }

  let name = CString::new("main").unwrap();
  let sig = CString::new("([Ljava/lang/String;)V").unwrap();
  let mid = ((**env).GetStaticMethodID.unwrap())(env, main_cls, name.as_ptr(), sig.as_ptr());
  if mid.is_null() {
    return Err(jni_failure(
      env,
      "GetStaticMethodID main([Ljava/lang/String;)V failed",
    ));
  }

  let string_cls_name = CString::new("java/lang/String").unwrap();
  let string_cls = ((**env).FindClass.unwrap())(env, string_cls_name.as_ptr());
  if string_cls.is_null() {
    return Err(jni_failure(env, "FindClass java/lang/String failed"));
  }
  let argv = ((**env).NewObjectArray.unwrap())(env, 0, string_cls, ptr::null_mut());
  if argv.is_null() {
    return Err(jni_failure(env, "NewObjectArray(String[0]) failed"));
  }

  log::info!(target: "suwayomi", "invoking suwayomi.tachidesk.MainKt.main");
  let args = [jvalue { l: argv }];
  ((**env).CallStaticVoidMethodA.unwrap())(env, main_cls, mid, args.as_ptr());
  if exception_pending(env) {
    return Err(jni_failure(env, "Suwayomi main threw"));
  }
  Ok(())
}

/// JVM options for the in-process engine, as plain strings (split out so the pinned
/// sandbox paths are assertable in tests): the ones every phone gets, then the system's
/// own (`platform::JVM_OPTIONS`).
///
/// `-Xmx256m` is the jetsam-informed cap from the spike plan (§2 Q1); `GC_OPT` picks
/// the collector that fits it. NO `-Djava.home`: HotSpot sets up its boot class path
/// before it reads any `-D` option, so each `platform` makes the VM derive the right
/// one by itself.
///
/// `java.io.tmpdir` and `user.home` are pinned because their UNIX defaults (`/tmp`
/// and the passwd home) are outside the app sandbox and not
/// writable: the extension installer's temp files, H2 spill files and any library
/// dotfile would fail with "Read-only file system" while the engine still reported
/// `ready`. Both targets live under the app-data dir this process already owns.
fn jvm_options(class_path: &str, data_dir: &Path) -> Vec<String> {
  // `data_dir` is `<app_data_dir>/suwayomi`; hand `user.home` its parent so stray
  // dotfiles land beside the engine's rootDir rather than inside it.
  let user_home = data_dir.parent().unwrap_or(data_dir);
  let mut options = vec![
    format!("-Djava.class.path={class_path}"),
    format!(
      "-Dsuwayomi.tachidesk.config.server.rootDir={}",
      data_dir.display()
    ),
    format!("-Djava.io.tmpdir={}", engine_tmp_dir(data_dir).display()),
    format!("-Duser.home={}", user_home.display()),
    "-Djava.awt.headless=true".to_string(),
    "-Xmx256m".to_string(),
    GC_OPT.to_string(),
  ];
  options.extend(platform::JVM_OPTIONS.iter().map(|o| o.to_string()));
  options
}

/// Create the in-process JVM and run the engine's `main` on THIS thread. Blocks
/// for the lifetime of the engine (i.e. of the app) on success; returns `Err`
/// only when the boot itself failed.
fn run_jvm(class_path: &str, data_dir: &Path) -> Result<(), String> {
  let opt_strings: Vec<CString> = jvm_options(class_path, data_dir)
    .into_iter()
    .map(|s| CString::new(s).expect("jvm option contains NUL"))
    .collect();

  let mut options: Vec<JavaVMOption> = opt_strings
    .iter()
    .map(|s| JavaVMOption {
      optionString: s.as_ptr() as *mut c_char,
      extraInfo: ptr::null_mut(),
    })
    .collect();

  let mut init_args = JavaVMInitArgs {
    version: JNI_VERSION_1_8,
    nOptions: options.len() as jint,
    options: options.as_mut_ptr(),
    ignoreUnrecognized: JNI_TRUE,
  };

  let mut vm: *mut JavaVM = ptr::null_mut();
  let mut env_ptr: *mut c_void = ptr::null_mut();
  let t0 = Instant::now();
  let rc = unsafe {
    platform::create_java_vm(
      &mut vm,
      &mut env_ptr,
      &mut init_args as *mut JavaVMInitArgs as *mut c_void,
    )
  }?;
  if rc != JNI_OK || vm.is_null() || env_ptr.is_null() {
    return Err(format!("JNI_CreateJavaVM failed: rc={rc}"));
  }
  log::info!(
    target: "suwayomi",
    "in-process JVM created in {} ms",
    t0.elapsed().as_millis()
  );

  let env = env_ptr as *mut JNIEnv;
  // `JNI_CreateJavaVM` attached THIS thread as the VM's primordial Java thread, and
  // HotSpot never auto-detaches: a native thread that exits while attached leaves a
  // JavaThread pointing at a stack the pthread has already unmapped, so the still-live
  // VM faults on its next safepoint stack scan. Every failure path below therefore
  // goes through one epilogue that detaches before this thread dies — including a
  // panic, which `catch_unwind` converts into the same honest terminal degrade.
  let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || unsafe {
    invoke_main(env)
  })) {
    Ok(res) => res,
    Err(_) => Err("Suwayomi main panicked in the VM thread".to_string()),
  };
  if let Err(e) = outcome {
    unsafe {
      if let Some(detach) = (**vm).DetachCurrentThread {
        let drc = detach(vm);
        if drc != JNI_OK {
          log::warn!(target: "suwayomi", "DetachCurrentThread failed: rc={drc}");
        }
      }
    }
    return Err(e);
  }

  // main() returned; the engine's non-daemon server threads keep the VM alive.
  // DestroyJavaVM parks this thread until they exit — effectively for the app's
  // lifetime. That is intentional (this thread owns the VM).
  log::info!(target: "suwayomi", "engine main returned; parking VM thread in DestroyJavaVM");
  unsafe {
    ((**vm).DestroyJavaVM.unwrap())(vm);
  }
  log::warn!(target: "suwayomi", "in-process JVM destroyed (all non-daemon threads exited)");
  Ok(())
}

/// Pre-VM boot steps, ALL of them retryable because they run before the
/// irreversible `JNI_CreateJavaVM`. The class-path and data-dir resolution are stable
/// across attempts (resolve once); only the port acquisition — the sole
/// TOCTOU-prone step — spins in `acquire_boot_port`.
///
/// Runs on the VM thread, never on the caller's: it hits the filesystem and, on the
/// retry path, sleeps.
fn boot(app: &AppHandle) -> Result<(String, PathBuf, u16), String> {
  let class_path = platform::prepare(app)?;
  let data_dir = app
    .path()
    .app_data_dir()
    .map_err(|e| format!("resolve app_data_dir: {e}"))?
    .join("suwayomi");
  // Bounded retry: broker a port, re-verify it's still free right before we commit
  // to the irreversible VM creation, and retry with a fresh port on a transient
  // steal. Only once a port is secured do we write the conf and boot the VM.
  let port = acquire_boot_port(broker_port, port_is_bindable, |attempt| {
    log::info!(
      target: "suwayomi",
      "retrying boot port acquisition (attempt {attempt} failed)"
    );
    std::thread::sleep(BOOT_RETRY_BACKOFF);
  })?;
  prepare_data_dir(&data_dir, port)?;
  Ok((class_path, data_dir, port))
}

/// Readiness gate on the async runtime, mirroring desktop's Status transitions.
/// Hands off to `spawn_heartbeat` once the engine answers.
fn spawn_readiness_gate(sup: Arc<SuwayomiSupervisor>, port: u16, boot_started: Instant) {
  tauri::async_runtime::spawn(async move {
    match poll_ready(&sup.client.clone(), port, READY_TIMEOUT).await {
      Ok(version) => {
        log::info!(
          target: "suwayomi",
          "engine ready ({version}) on port {port} — cold start {} ms",
          boot_started.elapsed().as_millis()
        );
        sup.set_ready(port, version);
        spawn_heartbeat(sup.clone(), port);
      }
      Err(e) => {
        // Readiness failed after the retryable pre-VM steps were exhausted, so this
        // means "genuinely couldn't boot," not "lost a port race once." Don't clobber
        // a more precise error already recorded by the JVM thread.
        let already_degraded = sup.status().state == "degraded";
        log::error!(target: "suwayomi", "engine readiness failed: {e}");
        if !already_degraded {
          sup.degrade(e);
        }
      }
    }
  });
}

/// Liveness watch for a Ready engine. Desktop learns the engine died from
/// `child.wait()`; the in-process VM has NO observable exit, so without this an
/// engine that dies after readiness (OOM under `-Xmx`, an uncaught error in a
/// Javalin thread, a Zero StackOverflowError) would leave `state=ready` forever
/// while every request failed and the JS side quietly memoized each work as
/// unusable. Only `HEARTBEAT_FAILURES_TO_DEGRADE` CONSECUTIVE misses degrade, and
/// the task exits as soon as the state leaves Ready (stop, or a degrade from the VM
/// thread) so it never resurrects or clobbers another transition.
fn spawn_heartbeat(sup: Arc<SuwayomiSupervisor>, port: u16) {
  tauri::async_runtime::spawn(async move {
    let url = format!("http://127.0.0.1:{port}/api/graphql");
    let body = json!({ "query": "{ aboutServer { version } }" }).to_string();
    let mut failures: u32 = 0;
    loop {
      tokio::time::sleep(HEARTBEAT_INTERVAL).await;
      if !sup.is_ready() {
        return;
      }
      // Any HTTP answer means the engine's server is alive; a GraphQL-level error
      // is not our concern here.
      let alive = sup
        .client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(HEARTBEAT_TIMEOUT)
        .body(body.clone())
        .send()
        .await
        .map(|resp| resp.status().is_success())
        .unwrap_or(false);
      if alive {
        failures = 0;
        continue;
      }
      failures += 1;
      log::warn!(
        target: "suwayomi",
        "engine heartbeat missed ({failures}/{HEARTBEAT_FAILURES_TO_DEGRADE})"
      );
      if failures >= HEARTBEAT_FAILURES_TO_DEGRADE {
        log::error!(target: "suwayomi", "engine stopped responding; degrading");
        sup.degrade("engine stopped responding");
        return;
      }
    }
  });
}

/// Initialize and launch the in-process engine. Non-fatal by design: any
/// resolution failure logs, sets `degraded`, and returns — app startup never
/// aborts, and the JS fallback ladder routes to the hosted backend.
///
/// This can be called from Tauri's `setup()` hook, i.e. the app's MAIN thread, so it does
/// nothing but spawn: the filesystem work and the port retry's backoff sleeps
/// (worst case ≈0.6 s) belong on the VM thread, not in the app's launch path.
pub fn start(app: AppHandle) {
  let sup = match app.try_state::<Arc<SuwayomiSupervisor>>() {
    Some(s) => s.inner().clone(),
    None => {
      log::error!(target: "suwayomi", "supervisor state not managed; skipping start");
      return;
    }
  };

  if sup.boot_attempted.swap(true, Ordering::SeqCst) {
    log::warn!(target: "suwayomi", "in-process engine already booted once; not booting again");
    return;
  }

  // The VM owner thread: resolves the boot inputs, creates the JVM, runs Java main,
  // then parks for the app's lifetime. Any failure transitions to degraded here (the
  // readiness poll would otherwise only report the generic timeout).
  let sup_jvm = sup.clone();
  let spawned = std::thread::Builder::new()
    .name("suwayomi-jvm".into())
    .stack_size(JVM_THREAD_STACK)
    .spawn(move || {
      let boot_started = Instant::now();
      let (class_path, data_dir, port) = match boot(&app) {
        Ok(v) => v,
        Err(e) => {
          log::warn!(target: "suwayomi", "engine unavailable: {e}");
          sup_jvm.degrade(e);
          return;
        }
      };
      log::info!(
        target: "suwayomi",
        "launching in-process engine on 127.0.0.1:{port} (class path: {class_path}, rootDir: {})",
        data_dir.display()
      );
      // Start watching before `run_jvm` blocks for the app's lifetime.
      spawn_readiness_gate(sup_jvm.clone(), port, boot_started);
      if let Err(e) = run_jvm(&class_path, &data_dir) {
        // Failure here is AFTER `JNI_CreateJavaVM` (or a classpath fault) and is
        // genuinely unrecoverable in-process — a terminal, actionable degrade.
        log::error!(target: "suwayomi", "engine boot failed: {e}");
        sup_jvm.degrade(e);
      }
    });
  if let Err(e) = spawned {
    let msg = format!("spawn suwayomi-jvm thread: {e}");
    log::error!(target: "suwayomi", "{msg}");
    sup.degrade(msg);
  }
}

// ---- JS-facing commands (identical signatures/shapes to desktop) ------------

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

/// IPC-proxy transport: POST a GraphQL query to the loopback engine and return
/// the parsed JSON (verbatim desktop behavior — HTTP over loopback).
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

/// Guard for `suwayomi_image`'s `path` — verbatim desktop's SSRF containment:
/// only rooted `/api/...` engine routes, no traversal, no backslashes.
fn validate_image_path(path: &str) -> Result<(), String> {
  if !path.starts_with("/api/") {
    return Err(format!(
      "suwayomi_image: path must start with /api/, got {path:?}"
    ));
  }
  // Percent-decode ONCE (mirrors the engine's own HTTP-layer decode) before the
  // traversal checks, so `%2e%2e` / `%5c` can't slip past the literal guards.
  let decoded = percent_decode_once(path);
  if decoded.contains("..") {
    return Err(format!(
      "suwayomi_image: path must not contain '..', got {path:?}"
    ));
  }
  if decoded.contains('\\') {
    return Err(format!(
      "suwayomi_image: path must not contain '\\', got {path:?}"
    ));
  }
  Ok(())
}

/// Single-pass, lossy percent-decode — verbatim desktop's (`suwayomi.rs`). One
/// decode level matches the engine's HTTP router, catching `%2e%2e`/`%5c` without
/// over-decoding (`%252e` stays `%2e`).
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

/// One image from the engine: its bytes and the content type the engine gave it. The
/// `engine-image` address handler in `lib.rs` serves this to the web view, which then
/// loads a chapter's pages by itself, one by one (verbatim desktop behavior).
pub async fn engine_image(
  sup: &SuwayomiSupervisor,
  path: &str,
) -> Result<(Vec<u8>, Option<String>), String> {
  validate_image_path(path)?;
  // Share the process-wide image concurrency cap with `fetch_image`. Especially
  // load-bearing on a phone: the in-process JVM runs under `-Xmx256m` in a memory-
  // limited process, so unbounded 32 MiB buffers here trip the OS OOM killer.
  let _permit = crate::image_fetch_semaphore()
    .acquire()
    .await
    .map_err(|_| "image fetch pool closed".to_string())?;
  let (port, client) = sup
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
  if let Some(len) = resp.content_length() {
    if len > MAX_IMAGE_BYTES as u64 {
      return Err(format!("image too large: {len} bytes"));
    }
  }
  let content_type = resp
    .headers()
    .get(reqwest::header::CONTENT_TYPE)
    .and_then(|value| value.to_str().ok())
    .map(str::to_owned);
  let mut buf: Vec<u8> = Vec::new();
  let mut resp = resp;
  while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
    if buf.len() + chunk.len() > MAX_IMAGE_BYTES {
      return Err("image exceeds size cap".to_string());
    }
    buf.extend_from_slice(&chunk);
  }
  Ok((buf, content_type))
}

/// IPC transport for the same bytes. The reader's pages use the `engine-image` address
/// now; this stays for callers that want the bytes in JavaScript.
#[tauri::command]
pub async fn suwayomi_image(
  state: State<'_, Arc<SuwayomiSupervisor>>,
  path: String,
) -> Result<tauri::ipc::Response, String> {
  let (bytes, _) = engine_image(&state, &path).await?;
  Ok(tauri::ipc::Response::new(bytes))
}

// ---- Tests (compile only when testing a phone target) ------------------------

#[cfg(test)]
mod tests {
  use super::*;

  /// The conf written to the phone is the shared template, with the three auth lines
  /// and this run's password. (The template's own tests run on the host, in
  /// `engine_auth.rs`; this one pins what `prepare_data_dir` puts on disk.)
  #[test]
  fn prepare_data_dir_writes_the_auth_enabled_conf() {
    let dir = std::env::temp_dir().join(format!("komika-engine-conf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    prepare_data_dir(&dir, 54321).expect("prepare data dir");
    let conf = std::fs::read_to_string(dir.join("server.conf")).expect("read server.conf");
    assert!(conf.contains("server.ip = \"127.0.0.1\""));
    assert!(conf.contains("server.port = 54321"));
    assert!(conf.contains("server.kcefEnabled = false"));
    assert!(conf.contains("server.backupInterval = 0"));
    assert!(conf.contains("server.globalUpdateInterval = 0"));
    assert!(conf.contains("server.authMode = \"BASIC_AUTH\""));
    assert!(conf.contains(&format!(
      "server.authUsername = \"{}\"",
      engine_auth::ENGINE_AUTH_USER
    )));
    assert!(conf.contains(&format!(
      "server.authPassword = \"{}\"",
      engine_auth::engine_password()
    )));
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// The JVM's UNIX defaults for these two properties point outside the app sandbox,
  /// so both must be pinned inside the app-data dir, and the temp dir must be the one
  /// `prepare_data_dir` creates.
  #[test]
  fn jvm_options_pin_sandbox_writable_dirs() {
    let data_dir = Path::new("/var/mobile/Containers/Data/Application/ABC/suwayomi");
    let opts = jvm_options("/app/assets/suwayomi/Suwayomi-Server.jar", data_dir);
    let has = |v: &str| opts.iter().any(|o| o.as_str() == v);
    assert!(has("-Djava.class.path=/app/assets/suwayomi/Suwayomi-Server.jar"), "opts: {opts:?}");
    assert!(
      has(&format!("-Djava.io.tmpdir={}", engine_tmp_dir(data_dir).display())),
      "opts: {opts:?}"
    );
    assert!(
      has("-Duser.home=/var/mobile/Containers/Data/Application/ABC"),
      "opts: {opts:?}"
    );
    assert!(has("-XX:+UseSerialGC"), "a phone must not get G1's worker threads");
    for platform_option in platform::JVM_OPTIONS {
      assert!(has(platform_option), "opts: {opts:?}");
    }
  }

  #[test]
  fn exception_summary_is_one_bounded_line() {
    assert_eq!(
      exception_summary(Some("java.lang.UnsatisfiedLinkError: 'int a.B.chmod(java.lang.String, int)'")),
      "java.lang.UnsatisfiedLinkError: 'int a.B.chmod(java.lang.String, int)'"
    );
    // A message with more lines keeps the first one only.
    assert_eq!(exception_summary(Some("a.B: first\nsecond")), "a.B: first");
    assert_eq!(exception_summary(None), "<exception text unavailable>");
    assert_eq!(exception_summary(Some("  \n")), "<exception text unavailable>");
    let long = exception_summary(Some(&"é".repeat(MAX_SUMMARY_CHARS + 50)));
    assert_eq!(long.chars().count(), MAX_SUMMARY_CHARS + 1, "bounded, plus the ellipsis");
    assert!(long.ends_with('…'));
  }

  #[test]
  fn bounded_trace_keeps_a_short_trace_and_cuts_a_long_one() {
    let short = "a.B: boom\n\tat a.B.c(B.java:1)\nCaused by: d.E: why\n\tat d.E.f(E.java:2)\n";
    assert_eq!(
      bounded_trace(short),
      "a.B: boom\n\tat a.B.c(B.java:1)\nCaused by: d.E: why\n\tat d.E.f(E.java:2)"
    );
    let long: String = (0..MAX_TRACE_LINES + 7).map(|i| format!("\tat x.Y.z(Y.java:{i})\r\n")).collect();
    let cut = bounded_trace(&long);
    assert_eq!(cut.lines().count(), MAX_TRACE_LINES + 1);
    assert!(cut.starts_with("\tat x.Y.z(Y.java:0)\n"), "CR is trimmed, frames kept in order");
    assert!(cut.ends_with("\t... 7 more lines not logged"), "tail: {:?}", cut.lines().last());
  }

  #[test]
  fn image_path_guard_matches_desktop() {
    assert!(validate_image_path("/api/v1/manga/3/thumbnail").is_ok());
    for bad in [
      "", "/foo", "http://evil", "//evil", "/api/../s", "/api/\\x",
      "/api/%2e%2e/s", "/api/%2E%2E/s", "/api/x%5cy",
    ] {
      assert!(validate_image_path(bad).is_err(), "{bad:?} must be rejected");
    }
  }

  use std::cell::Cell;

  /// A port stolen on the first attempt must NOT permanently degrade the single-boot
  /// engine: `acquire_boot_port` retries with a fresh port and succeeds.
  #[test]
  fn acquire_boot_port_retries_past_a_stolen_port() {
    let brokered = Cell::new(0u32);
    let backoffs = Cell::new(0u32);
    let port = acquire_boot_port(
      // Each attempt brokers a distinct, ever-increasing port.
      || {
        let n = brokered.get() + 1;
        brokered.set(n);
        Ok(40000 + n as u16)
      },
      // First brokered port is "stolen" (not bindable); anything from attempt 2 on is free.
      |_p| brokered.get() >= 2,
      |_attempt| backoffs.set(backoffs.get() + 1),
    )
    .expect("should secure a port on a later attempt, not degrade");
    assert_eq!(port, 40002, "should boot on the 2nd port after the 1st was stolen");
    assert_eq!(brokered.get(), 2, "should have brokered exactly twice");
    assert_eq!(backoffs.get(), 1, "should have backed off once between the attempts");
  }

  /// The first-brokered port being free is the happy path: one attempt, no backoff.
  #[test]
  fn acquire_boot_port_succeeds_first_try_without_backoff() {
    let backoffs = Cell::new(0u32);
    let port = acquire_boot_port(|| Ok(45678), |_| true, |_| backoffs.set(backoffs.get() + 1))
      .expect("a free first port should boot immediately");
    assert_eq!(port, 45678);
    assert_eq!(backoffs.get(), 0, "no retry, so no backoff");
  }

  /// Only after the bounded budget is exhausted (a port taken on EVERY attempt) does
  /// it give up — that is the point where an honest `degraded` is warranted.
  #[test]
  fn acquire_boot_port_degrades_only_after_exhausting_attempts() {
    let backoffs = Cell::new(0u32);
    let err = acquire_boot_port(|| Ok(50000), |_| false, |_| backoffs.set(backoffs.get() + 1))
      .expect_err("a port taken on every attempt must eventually fail");
    assert!(err.contains(&format!("after {MAX_BOOT_ATTEMPTS} attempts")), "err: {err}");
    assert_eq!(
      backoffs.get(),
      MAX_BOOT_ATTEMPTS - 1,
      "backs off between attempts but not after the last"
    );
  }

  /// A transient broker error is a retry, not a degrade, as long as a later attempt
  /// yields a bindable port.
  #[test]
  fn acquire_boot_port_retries_past_a_broker_error() {
    let calls = Cell::new(0u32);
    let port = acquire_boot_port(
      || {
        let n = calls.get() + 1;
        calls.set(n);
        if n == 1 {
          Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "transient"))
        } else {
          Ok(46000)
        }
      },
      |_| true,
      |_| {},
    )
    .expect("a transient broker error should retry, not degrade");
    assert_eq!(port, 46000);
    assert_eq!(calls.get(), 2);
  }
}
