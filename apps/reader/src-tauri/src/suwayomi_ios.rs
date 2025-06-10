//! In-process embedded Suwayomi engine for REAL iOS DEVICES (N4.2).
//!
//! iOS has no child-process model and forbids app-loaded dylibs, so unlike the
//! desktop supervisor (`suwayomi.rs`, a `java` child-process sidecar) this module
//! boots the stock `Suwayomi-Server.jar` INSIDE the app process: it calls
//! `JNI_CreateJavaVM` against the interpreter-only (Zero, NO-JIT) OpenJDK that
//! N4.1 built as static libs (`jre-ios/aarch64-ios`, statically linked into the
//! app binary — see `build.rs` and docs/plans/n4.1-ios-jvm-findings.md §6), then
//! invokes `suwayomi.tachidesk.MainKt.main(String[])` (the jar's manifest
//! Main-Class) on a dedicated native thread.
//!
//! Runtime layout (why the app bundle carries a root-level `lib/` folder): the
//! statically-linked HotSpot derives `java.home` ITSELF as `<exe dir>/lib` — on
//! iOS `os::init_system_properties_values()` takes the dladdr path of the app
//! executable, strips the binary name, and appends `/lib` because
//! `is_vm_statically_linked()` is true (verified against the pinned
//! openjdk/mobile sources, os_bsd.cpp `__IOS__` branch). `-Djava.home` CANNOT
//! relocate the boot jimage: `set_boot_path()` (=> `<java.home>/lib/modules`)
//! runs during property init, BEFORE `-D` options are applied. The bundle must
//! therefore contain:
//!   Komika.app/lib/lib/modules            <- jimage (all 66 modules, ~149 MB)
//!   Komika.app/lib/lib/{tzdb.dat,security,jfr,jvm.cfg}
//!   Komika.app/lib/{conf,release}
//! staged into the Xcode project by scripts/stage-ios-jvm-runtime.sh (a folder
//! reference in gen/apple/project.yml). The jar itself IS a normal tauri
//! resource (`tauri.ios.conf.json` bundles `suwayomi/`, resolved via
//! `resource_dir()` => Komika.app/assets/suwayomi/Suwayomi-Server.jar).
//!
//! Same public surface as desktop: `SuwayomiSupervisor` (managed state),
//! `start(app)`, and the four `#[tauri::command]`s with identical names and
//! JSON shapes, so the JS `LocalSuwayomiBackend` works unchanged. Security
//! posture also unchanged: the engine binds 127.0.0.1 only; JS reaches it only
//! through the `suwayomi_gql` / `suwayomi_image` IPC proxies.
//!
//! Lifecycle limits (deliberate for this gate, N4.8 refines):
//!   * ONE boot per process. A JVM cannot be re-created after DestroyJavaVM in
//!     the same process, so there is no crash/restart supervision loop — a dead
//!     engine transitions to `degraded` and stays there until app relaunch.
//!   * `stop_and_wait` does NOT call DestroyJavaVM: it would block until the
//!     engine's non-daemon server threads exit (i.e. forever), and on iOS the
//!     process is about to be torn down by the OS anyway. It just flips the
//!     state to `stopped`; the JVM dies with the process. Best-effort by design.

use std::ffi::{c_char, c_void, CString};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jni_sys::{
  jint, jvalue, JavaVM, JavaVMInitArgs, JavaVMOption, JNIEnv, JNI_CreateJavaVM, JNI_OK,
  JNI_TRUE, JNI_VERSION_1_8,
};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};

/// Cold-start budget for the readiness gate. The desktop JIT JVM needs ~5 s; the
/// Zero interpreter on a phone is an order of magnitude slower and unmeasured
/// before this spike (OQ1), so the budget is deliberately generous.
const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// How often to poll `aboutServer` while waiting for readiness.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Per-request timeout for the readiness poll and the `suwayomi_gql` proxy.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard cap on an engine image body (kept in lockstep with lib.rs / desktop).
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024; // 32 MiB
/// Native stack for the thread that owns the VM and runs Java `main` — the Zero
/// interpreter burns C stack per Java frame, so give it far more than `-Xss`.
const JVM_THREAD_STACK: usize = 16 * 1024 * 1024;
/// `-Xss` for every thread the VM creates ITSELF (Javalin/OkHttp workers, the
/// extension installer's class rewriting, the class loaders) — the threads that
/// actually run deep recursive Java code. Same reasoning as `JVM_THREAD_STACK`:
/// under Zero every Java frame costs a C frame of the bytecode interpreter, so the
/// HotSpot default (~1 MiB) buys a fraction of the Java depth it buys on a JIT VM,
/// and a StackOverflowError here shows up as an engine that is `ready` but never
/// serves a chapter. Thread stacks are reserved address space committed page by
/// page, so the RSS cost is the depth actually used; the peak thread count is an
/// N4.3 measurement input. Desktop's JIT sidecar sets no `-Xss` at all.
const JAVA_THREAD_STACK_OPT: &str = "-Xss4m";
/// How often a Ready engine is re-checked for liveness (see `spawn_heartbeat`).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Per-heartbeat request timeout — deliberately shorter than the interval so a hung
/// engine is observed rather than queued behind the client's 60 s default.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
/// Consecutive heartbeat failures before the engine is declared dead. Three misses
/// (≈90 s) rides out a long GC pause or an app suspend/resume without a false
/// terminal degrade.
const HEARTBEAT_FAILURES_TO_DEGRADE: u32 = 3;
/// Bounded pre-VM boot attempts. iOS can create a JVM exactly once per process
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
    let client = reqwest::Client::builder()
      .timeout(HTTP_TIMEOUT)
      // Loopback is allowed here (engine transport), same posture as desktop.
      .build()
      .expect("build suwayomi http client");
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

/// Render the authoritative HOCON `server.conf` — VERBATIM the desktop template
/// (suwayomi.rs): kcefEnabled=false is critical there and harmless-but-correct
/// here (no Chromium download attempts on a phone).
fn render_server_conf(port: u16) -> String {
  format!(
    "server.ip = \"127.0.0.1\"\n\
     server.port = {port}\n\
     server.webUIEnabled = false\n\
     server.initialOpenInBrowserEnabled = false\n\
     server.systemTrayEnabled = false\n\
     server.kcefEnabled = false\n\
     server.flareSolverrEnabled = false\n"
  )
}

/// The engine's temp dir, pinned as `java.io.tmpdir` (see `jvm_options`). Kept in
/// one place so `prepare_data_dir` and `run_jvm` cannot disagree about it.
fn engine_tmp_dir(data_dir: &Path) -> PathBuf {
  data_dir.join("tmp")
}

/// Write `server.conf` into the data dir (creating it), mirroring desktop, and
/// create the app-private temp dir the VM is pointed at.
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

/// Describe + clear any pending Java exception (stack trace goes to stderr,
/// which devicectl's console capture streams).
unsafe fn describe_exception(env: *mut JNIEnv) {
  if let Some(check) = (**env).ExceptionCheck {
    if check(env) == JNI_TRUE {
      if let Some(describe) = (**env).ExceptionDescribe {
        describe(env);
      }
      if let Some(clear) = (**env).ExceptionClear {
        clear(env);
      }
    }
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
    describe_exception(env);
    return Err("FindClass suwayomi/tachidesk/MainKt failed (classpath?)".into());
  }

  let name = CString::new("main").unwrap();
  let sig = CString::new("([Ljava/lang/String;)V").unwrap();
  let mid = ((**env).GetStaticMethodID.unwrap())(env, main_cls, name.as_ptr(), sig.as_ptr());
  if mid.is_null() {
    describe_exception(env);
    return Err("GetStaticMethodID main([Ljava/lang/String;)V failed".into());
  }

  let string_cls_name = CString::new("java/lang/String").unwrap();
  let string_cls = ((**env).FindClass.unwrap())(env, string_cls_name.as_ptr());
  if string_cls.is_null() {
    describe_exception(env);
    return Err("FindClass java/lang/String failed".into());
  }
  let argv = ((**env).NewObjectArray.unwrap())(env, 0, string_cls, ptr::null_mut());
  if argv.is_null() {
    describe_exception(env);
    return Err("NewObjectArray(String[0]) failed".into());
  }

  log::info!(target: "suwayomi", "invoking suwayomi.tachidesk.MainKt.main");
  let args = [jvalue { l: argv }];
  ((**env).CallStaticVoidMethodA.unwrap())(env, main_cls, mid, args.as_ptr());
  if let Some(check) = (**env).ExceptionCheck {
    if check(env) == JNI_TRUE {
      describe_exception(env);
      return Err("Suwayomi main threw".into());
    }
  }
  Ok(())
}

/// JVM options for the in-process engine, as plain strings (split out so the pinned
/// sandbox paths are assertable in tests).
///
/// `-Xmx256m` is the jetsam-informed cap from the spike plan (§2 Q1);
/// `JAVA_THREAD_STACK_OPT` sizes per-Java-thread stacks for the Zero interpreter. NO
/// `-Djava.home`: the statically-linked VM derives it from the executable path (see
/// module docs) and the bundle layout satisfies it.
///
/// `java.io.tmpdir` and `user.home` are pinned because their UNIX defaults (`/tmp`
/// and the passwd home, `/var/mobile`) are outside the iOS app sandbox and not
/// writable: the extension installer's temp files, H2 spill files and any library
/// dotfile would fail with "Read-only file system" while the engine still reported
/// `ready`. Both targets live under the app-data dir this process already owns.
fn jvm_options(jar: &Path, data_dir: &Path) -> Vec<String> {
  // `data_dir` is `<app_data_dir>/suwayomi`; hand `user.home` its parent so stray
  // dotfiles land beside the engine's rootDir rather than inside it.
  let user_home = data_dir.parent().unwrap_or(data_dir);
  vec![
    format!("-Djava.class.path={}", jar.display()),
    format!(
      "-Dsuwayomi.tachidesk.config.server.rootDir={}",
      data_dir.display()
    ),
    format!("-Djava.io.tmpdir={}", engine_tmp_dir(data_dir).display()),
    format!("-Duser.home={}", user_home.display()),
    "-Djava.awt.headless=true".to_string(),
    "-Xmx256m".to_string(),
    JAVA_THREAD_STACK_OPT.to_string(),
  ]
}

/// Create the in-process JVM and run the engine's `main` on THIS thread. Blocks
/// for the lifetime of the engine (i.e. of the app) on success; returns `Err`
/// only when the boot itself failed.
fn run_jvm(jar: &Path, data_dir: &Path) -> Result<(), String> {
  let opt_strings: Vec<CString> = jvm_options(jar, data_dir)
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
    JNI_CreateJavaVM(
      &mut vm,
      &mut env_ptr,
      &mut init_args as *mut JavaVMInitArgs as *mut c_void,
    )
  };
  if rc != JNI_OK || vm.is_null() || env_ptr.is_null() {
    return Err(format!("JNI_CreateJavaVM failed: rc={rc}"));
  }
  log::info!(
    target: "suwayomi",
    "in-process Zero JVM created in {} ms",
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

/// Resolve the bundled jar via tauri's resource resolver
/// (=> Komika.app/assets/suwayomi/Suwayomi-Server.jar on iOS).
fn resolve_jar(app: &AppHandle) -> Result<PathBuf, String> {
  let res = app
    .path()
    .resource_dir()
    .map_err(|e| format!("resolve resource_dir: {e}"))?;
  let cand = res.join("suwayomi").join("Suwayomi-Server.jar");
  if cand.exists() {
    Ok(cand)
  } else {
    Err(format!(
      "Suwayomi-Server.jar not bundled at {} (tauri.ios.conf.json resources)",
      cand.display()
    ))
  }
}

/// Sanity-check the JVM runtime the app bundle must carry at <exe dir>/lib (the
/// java.home the statically-linked VM derives — see module docs). Catching this
/// here yields a clean `degraded` + actionable error instead of a VM abort.
fn check_bundled_runtime() -> Result<(), String> {
  let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
  let app_dir = exe
    .parent()
    .ok_or_else(|| "executable has no parent dir".to_string())?;
  let jimage = app_dir.join("lib").join("lib").join("modules");
  if jimage.exists() {
    Ok(())
  } else {
    Err(format!(
      "bundled JVM runtime missing: {} not found (stage it with \
       scripts/stage-ios-jvm-runtime.sh and rebuild)",
      jimage.display()
    ))
  }
}

/// Pre-VM boot steps, ALL of them retryable because they run before the
/// irreversible `JNI_CreateJavaVM`. The jar and data-dir resolution are stable
/// across attempts (resolve once); only the port acquisition — the sole
/// TOCTOU-prone step — spins in `acquire_boot_port`.
///
/// Runs on the VM thread, never on the caller's: it hits the filesystem and, on the
/// retry path, sleeps.
fn boot(app: &AppHandle) -> Result<(PathBuf, PathBuf, u16), String> {
  check_bundled_runtime()?;
  let jar = resolve_jar(app)?;
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
  Ok((jar, data_dir, port))
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
/// This is called from Tauri's `setup()` hook, i.e. the iOS MAIN thread, so it does
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
      let (jar, data_dir, port) = match boot(&app) {
        Ok(v) => v,
        Err(e) => {
          log::warn!(target: "suwayomi", "engine unavailable: {e}");
          sup_jvm.degrade(e);
          return;
        }
      };
      log::info!(
        target: "suwayomi",
        "launching in-process engine on 127.0.0.1:{port} (jar: {}, rootDir: {})",
        jar.display(),
        data_dir.display()
      );
      // Start watching before `run_jvm` blocks for the app's lifetime.
      spawn_readiness_gate(sup_jvm.clone(), port, boot_started);
      if let Err(e) = run_jvm(&jar, &data_dir) {
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

/// IPC-proxy transport for engine image bytes (verbatim desktop behavior).
#[tauri::command]
pub async fn suwayomi_image(
  state: State<'_, Arc<SuwayomiSupervisor>>,
  path: String,
) -> Result<tauri::ipc::Response, String> {
  validate_image_path(&path)?;
  // Share the process-wide image concurrency cap with `fetch_image`. Especially
  // load-bearing on iOS: the in-process JVM runs under `-Xmx256m` in a jetsam-
  // limited process, so unbounded 32 MiB buffers here trip the OS OOM killer.
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

// ---- Tests (compile only when testing the ios-device target) ----------------

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn server_conf_matches_desktop_template() {
    let conf = render_server_conf(54321);
    assert!(conf.contains("server.ip = \"127.0.0.1\""));
    assert!(conf.contains("server.port = 54321"));
    assert!(conf.contains("server.kcefEnabled = false"));
    assert!(conf.contains("server.flareSolverrEnabled = false"));
  }

  /// The JVM's UNIX defaults for these two properties point outside the iOS sandbox,
  /// so both must be pinned inside the app-data dir, and the temp dir must be the one
  /// `prepare_data_dir` creates.
  #[test]
  fn jvm_options_pin_sandbox_writable_dirs() {
    let data_dir = Path::new("/var/mobile/Containers/Data/Application/ABC/suwayomi");
    let opts = jvm_options(Path::new("/app/assets/suwayomi/Suwayomi-Server.jar"), data_dir);
    let has = |v: &str| opts.iter().any(|o| o.as_str() == v);
    assert!(
      has(&format!("-Djava.io.tmpdir={}", engine_tmp_dir(data_dir).display())),
      "opts: {opts:?}"
    );
    assert!(
      has("-Duser.home=/var/mobile/Containers/Data/Application/ABC"),
      "opts: {opts:?}"
    );
    assert!(has(JAVA_THREAD_STACK_OPT), "opts: {opts:?}");
    assert!(!has("-Xss1m"), "the Zero interpreter needs more than the default stack");
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
