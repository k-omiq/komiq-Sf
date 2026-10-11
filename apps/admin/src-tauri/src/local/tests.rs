//! Process-management tests. They run `fake_server.py` (needs `python3`) in place of
//! komika-server, so they cover the start sequence without the real server or a GUI.
//! `real_server_*` runs the real binary and is `#[ignore]`d (see its doc).

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

/// A fresh data dir and a launcher script that runs the fake server in `mode`.
fn fixture(mode: &str) -> Launch {
  let dir = std::env::temp_dir().join(format!(
    "komiq-desktop-test-{}-{}",
    std::process::id(),
    NEXT_DIR.fetch_add(1, Ordering::SeqCst)
  ));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/local/fake_server.py");
  let bin = dir.join(format!("fake-{mode}.sh"));
  // Written under another name, closed and renamed; then run once as a probe (it only
  // exits). The product code starts this script without a retry, and on Linux a script
  // that another test thread's forked child still holds open for writing cannot be
  // started ("Text file busy"): the probe waits that moment out.
  let writing = dir.join(format!("fake-{mode}.sh.writing"));
  std::fs::write(
    &writing,
    format!(
      "#!/bin/sh\n[ -n \"$KOMIQ_FAKE_PROBE\" ] && exit 0\nexec python3 -u '{}' {mode}\n",
      fake.display()
    ),
  )
  .unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&writing, std::fs::Permissions::from_mode(0o755)).unwrap();
  }
  std::fs::rename(&writing, &bin).unwrap();
  #[cfg(unix)]
  {
    let probed = crate::child::spawn_when_ready(
      std::process::Command::new(&bin)
        .env("KOMIQ_FAKE_PROBE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null()),
    )
    .wait()
    .unwrap();
    assert!(probed.success(), "{} does not run", bin.display());
  }
  Launch {
    bin,
    web_dir: Some(dir.join("web")),
    data_dir: dir.join("data"),
    postgres_runtime: None,
  }
}

fn link(port: u16) -> EngineLink {
  EngineLink {
    url: format!("http://komika:not-a-real-secret@127.0.0.1:{port}"),
    public_url: format!("http://127.0.0.1:{port}"),
  }
}

fn attempt(port: u16) -> Attempt {
  Attempt {
    bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
    port,
    metrics_port: free_loopback_port().unwrap(),
    engine: link(1),
    database_url: "postgres://fake:fake@127.0.0.1:1/komika".into(),
  }
}

fn fake_env(launch: &Launch) -> serde_json::Value {
  let raw = std::fs::read(launch.data_dir.join("fake-env.json")).unwrap();
  serde_json::from_slice(&raw).unwrap()
}

fn runs(launch: &Launch) -> Vec<String> {
  std::fs::read_to_string(launch.data_dir.join("fake-runs.log"))
    .unwrap_or_default()
    .lines()
    .map(str::to_string)
    .collect()
}

#[cfg(unix)]
fn alive(pid: &str) -> bool {
  std::process::Command::new("kill")
    .args(["-0", pid])
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

/// An engine the test steers.
struct FakeEngine(
  Mutex<Option<Result<EngineLink, String>>>,
  /// How often the start sequence asked for the engine.
  std::sync::atomic::AtomicUsize,
);

impl FakeEngine {
  fn ready(port: u16) -> Arc<Self> {
    Arc::new(FakeEngine(
      Mutex::new(Some(Ok(link(port)))),
      Default::default(),
    ))
  }
  fn set(&self, v: Option<Result<EngineLink, String>>) {
    *lock(&self.0) = v;
  }
}

impl Engine for FakeEngine {
  fn ensure_started(&self) {
    self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
  }
  fn state(&self) -> EngineState {
    match lock(&self.0).clone() {
      None => EngineState::Starting,
      Some(Ok(l)) => EngineState::Ready(l),
      Some(Err(e)) => EngineState::Failed(e),
    }
  }
}

fn server(launch: Launch, engine: Arc<FakeEngine>, port: u16) -> Arc<LocalServer> {
  std::fs::create_dir_all(&launch.data_dir).unwrap();
  std::fs::write(
    launch.data_dir.join("desktop.json"),
    format!("{{\"port\":{port}}}"),
  )
  .unwrap();
  Arc::new(LocalServer::new(launch, engine, LogBuf::new(), true))
}

/// Poll status until `ready` or an error, for at most `secs`.
async fn settle(s: &LocalServer, secs: u64) -> Status {
  let deadline = Instant::now() + Duration::from_secs(secs);
  loop {
    let st = s.status();
    if st.stage == "ready" || st.error.is_some() || Instant::now() > deadline {
      return st;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
}

fn free_port() -> u16 {
  free_loopback_port().unwrap()
}

/// Waits for a test: `normal` for an ordinary start, a generous conversion allowance.
fn within(normal: u64) -> Bounds {
  bounds(normal, 600, 3600)
}

fn bounds(normal: u64, inactivity: u64, total: u64) -> Bounds {
  Bounds {
    normal: Duration::from_secs(normal),
    inactivity: Duration::from_secs(inactivity),
    total: Duration::from_secs(total),
  }
}

// ---- boot_server ---------------------------------------------------------------------

/// No processes are started: changing the saved choice must not pretend the old
/// loopback listener is ready for other devices while its restart is still pending.
#[test]
fn firewall_uses_actual_listener() {
  let s = LocalServer::new(
    Launch {
      bin: PathBuf::new(),
      web_dir: None,
      data_dir: PathBuf::new(),
      postgres_runtime: None,
    },
    FakeEngine::ready(1),
    LogBuf::new(),
    false,
  );
  assert_eq!(s.firewall_port(), None);
  lock(&s.state).running = true;
  s.set_ready(7580, false, None);
  lock(&s.prefs).lan = true;
  assert_eq!(
    s.firewall_port(),
    None,
    "preference changed before loopback restarted"
  );
  s.set_ready(7580, true, None);
  assert_eq!(s.firewall_port(), Some(7580));
  lock(&s.prefs).lan = false;
  assert_eq!(
    s.firewall_port(),
    None,
    "turning LAN off rejects immediately"
  );
  lock(&s.prefs).lan = true;
  s.set_stage(Stage::Server);
  assert_eq!(s.firewall_port(), None, "restarting listener is not ready");
  s.set_ready(7581, true, None);
  assert_eq!(s.firewall_port(), Some(7581), "use the current port");
  lock(&s.state).running = false;
  assert_eq!(s.firewall_port(), None, "stopped server is not listening");
}

#[tokio::test]
async fn boot_waits_for_health_and_catches_the_code() {
  let launch = fixture("setup");
  let log = LogBuf::new();
  let slot = Arc::new(Mutex::new(None));
  let port = free_port();
  let mut child = boot_server(&launch, &attempt(port), &log, &slot, &within(20))
    .await
    .expect("fake server boots");
  assert_eq!(lock(&slot).as_deref(), Some("TEST-CODE-0042"));
  let text = log.text();
  assert!(
    !text.contains("TEST-CODE-0042"),
    "the code never reaches the log"
  );
  assert!(text.contains(&format!("setup code {MASK}")));
  assert!(!text.contains('\u{1b}'), "colours stripped");

  let env = fake_env(&launch);
  assert_eq!(env["BIND_ADDR"], "127.0.0.1");
  assert_eq!(env["PORT"], port.to_string());
  assert_eq!(env["SUWAYOMI_URL"], link(1).url);
  assert_eq!(env["SUWAYOMI_PUBLIC_URL"], "http://127.0.0.1:1");
  assert!(env["CORS_ORIGINS"]
    .as_str()
    .unwrap()
    .contains("tauri://localhost"));
  assert_eq!(
    env["KOMIKA_WEB_DIR"],
    launch.web_dir.as_ref().unwrap().display().to_string()
  );
  assert!(env["DATABASE_URL"]
    .as_str()
    .unwrap()
    .starts_with("postgres://fake:fake@127.0.0.1:1/komika"));

  crate::suwayomi::stop_child(&mut child).await;
  assert!(child.try_wait().unwrap().is_some(), "no orphan");
}

#[tokio::test]
async fn boot_names_a_taken_port() {
  let launch = fixture("port");
  let r = boot_server(
    &launch,
    &attempt(free_port()),
    &LogBuf::new(),
    &Arc::new(Mutex::new(None)),
    &within(20),
  )
  .await;
  assert_eq!(r.err(), Some(BootError::PortInUse));
}

#[tokio::test]
async fn boot_reports_an_early_exit() {
  let launch = fixture("crash");
  let log = LogBuf::new();
  let r = boot_server(
    &launch,
    &attempt(free_port()),
    &log,
    &Arc::new(Mutex::new(None)),
    &within(20),
  )
  .await;
  match r {
    Err(BootError::Failed(e)) => assert!(e.contains("stopped while starting"), "{e}"),
    other => panic!("expected a failure, got {:?}", other.map(|_| ())),
  }
  assert!(log.text().contains("database is locked"), "cause kept");
}

#[cfg(unix)]
#[tokio::test]
async fn boot_gives_up_on_a_silent_server_and_kills_it() {
  let launch = fixture("hang");
  // A SQLite file in the data folder changes nothing: only the server's own lines can
  // lengthen the wait (an empty or stale file used to grant six hours).
  std::fs::create_dir_all(&launch.data_dir).unwrap();
  std::fs::write(launch.data_dir.join("komika.sqlite3"), "").unwrap();
  std::fs::write(
    launch.data_dir.join("komika.sqlite3.import-work-state"),
    "not a journal",
  )
  .unwrap();
  let started = Instant::now();
  let r = boot_server(
    &launch,
    &attempt(free_port()),
    &LogBuf::new(),
    &Arc::new(Mutex::new(None)),
    &bounds(1, 30, 60),
  )
  .await;
  match r {
    Err(BootError::Failed(e)) => assert_eq!(e, "The server did not answer within 1 seconds."),
    other => panic!("expected a timeout, got {:?}", other.map(|_| ())),
  }
  assert!(
    started.elapsed() < Duration::from_secs(15),
    "the normal bound applied, not a conversion allowance: {:?}",
    started.elapsed()
  );
  let pid = runs(&launch)[0].split(' ').next().unwrap().to_string();
  assert!(!alive(&pid), "the silent server was stopped");
}

// ---- the start wait and the server's import lines (N2) -----------------------------------
// These run the real `boot_server` against a process that prints the server's real lines.

#[cfg(unix)]
async fn boot_import(mode: &str, bounds: &Bounds) -> (Result<(), String>, Arc<LogBuf>, Duration) {
  let launch = fixture(mode);
  let log = LogBuf::new();
  let started = Instant::now();
  let result = boot_server(
    &launch,
    &attempt(free_port()),
    &log,
    &Arc::new(Mutex::new(None)),
    bounds,
  )
  .await;
  let elapsed = started.elapsed();
  let result = match result {
    Ok(mut child) => {
      crate::suwayomi::stop_child(&mut child).await;
      Ok(())
    }
    Err(BootError::Failed(e)) => Err(e),
    Err(BootError::PortInUse) => Err("port in use".into()),
  };
  (result, log, elapsed)
}

#[cfg(unix)]
#[tokio::test]
async fn a_no_op_warning_keeps_the_normal_bound() {
  let (result, log, elapsed) = boot_import("import-noop", &bounds(1, 30, 60)).await;
  assert_eq!(
    result.err().as_deref(),
    Some("The server did not answer within 1 seconds.")
  );
  assert!(
    elapsed < Duration::from_secs(15),
    "a hung ordinary start is not given the conversion allowance: {elapsed:?}"
  );
  let import = log.import();
  assert_eq!(import.state(), crate::import::ImportState::NoOp);
  assert_eq!(import.progress_lines(), 0);
  assert_eq!(import.warnings().len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn checking_the_old_database_is_bounded_by_the_normal_bound() {
  let (result, log, elapsed) = boot_import("import-prepare", &bounds(1, 30, 60)).await;
  assert_eq!(
    result.err().as_deref(),
    Some("The server did not answer within 1 seconds. Its last report: checking the old database.")
  );
  assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");
  assert_eq!(log.import().state(), crate::import::ImportState::Preparing);
}

/// A server that prints its check line and then starts (it does so on every start once
/// a conversion is finished, or only when an old file is there): an ordinary start.
#[cfg(unix)]
#[tokio::test]
async fn a_check_with_nothing_to_convert_is_an_ordinary_start() {
  let (result, log, _) = boot_import("import-prepare-ready", &bounds(20, 30, 60)).await;
  assert_eq!(result, Ok(()));
  let import = log.import();
  assert_eq!(import.state(), crate::import::ImportState::Idle);
  assert_eq!(import.bound_kind(), "normal");
  assert_eq!(import.error(), None);
  assert!(import.warnings().is_empty());
  assert_eq!(import.progress_lines(), 0);
  assert!(log.text().contains("sqlite import: preparing"));
}

/// A finished import whose work copy was left over (the server's import fix round 5):
/// the server prints its check line, the scan line, reads whole files for longer than the
/// normal bound, warns, and starts. The scan is waited for; and once the server answers,
/// nothing is "converting" any more (before, that state stayed for as long as the server
/// ran).
#[cfg(unix)]
#[tokio::test]
async fn a_scan_that_converts_nothing_is_waited_for_and_ends_as_a_started_server() {
  let (result, log, elapsed) = boot_import("import-scan-ready", &bounds(1, 30, 60)).await;
  assert_eq!(result, Ok(()));
  assert!(
    elapsed >= Duration::from_secs(2),
    "the scan took longer than the normal bound of this test: {elapsed:?}"
  );
  let import = log.import();
  assert_eq!(import.state(), crate::import::ImportState::NoOp);
  assert_eq!(import.bound_kind(), "normal");
  assert_eq!(import.progress(), None);
  assert_eq!(import.error(), None);
  assert_eq!(import.progress_lines(), 1, "the scan line");
  assert_eq!(import.warnings().len(), 1);
  assert!(
    import.warnings()[0].starts_with("PostgreSQL already holds the imported data; leaving "),
    "{:?}",
    import.warnings()
  );
}

/// A start that is refused after the server read two kept copies (the server's import
/// fix round 6): `preparing`, a scan line before each file, one error line, and the
/// process ends. Each scan takes longer than the normal bound of this test. The error
/// is the failure of the start, in the server's words and whole: it is several thousand
/// characters long and the steps to take are at its end.
#[cfg(unix)]
#[tokio::test]
async fn a_start_refused_after_scans_of_kept_copies_fails_with_the_whole_error() {
  let (result, log, elapsed) = boot_import("import-scan-refused", &bounds(1, 30, 60)).await;
  let error = result.expect_err("refused");
  assert!(
    elapsed >= Duration::from_secs(3),
    "the two scans took longer than the normal bound of this test: {elapsed:?}"
  );
  assert!(
    error.starts_with(
      "The database conversion failed: recovery stage: the SQLite file /Users/José Müller/\
       Library/Application Support/komiq desktop/server/komika.sqlite3 named by the unfinished \
       import is missing"
    ),
    "{error}"
  );
  assert!(
    error.chars().count() > crate::logbuf::MAX_LINE_CHARS,
    "{}",
    error.chars().count()
  );
  assert!(
    error.contains("(comments: 4, user_library: 12, users: 3, work: 15)"),
    "{error}"
  );
  assert!(
    error.ends_with(
      "komika.sqlite3.import-kept-20261011T070000Z out of the data directory and start again"
    ),
    "the steps at the end are not cut: {error}"
  );
  assert!(!error.contains('…'), "{error}");
  // Not left as a running conversion: the state is the failure, and no bound runs.
  let import = log.import();
  assert_eq!(import.state(), crate::import::ImportState::Failed);
  assert_eq!(import.bound_kind(), "failed");
  assert_eq!(import.progress(), None);
  assert_eq!(import.progress_lines(), 2, "the two scan lines");
  assert_eq!(
    import.failure_text(),
    error,
    "what the start reports is the tracker's text"
  );
  // The log view has the line whole as well.
  assert!(log
    .text()
    .contains(&error["The database conversion failed: ".len()..]));
}

#[cfg(unix)]
#[tokio::test]
async fn a_stalled_conversion_is_stopped_by_the_inactivity_bound() {
  let (result, log, elapsed) = boot_import("import-stall", &bounds(1, 4, 60)).await;
  let error = result.expect_err("stopped");
  assert!(
    error.starts_with("The database conversion made no progress for 1 minutes"),
    "{error}"
  );
  assert!(error.contains("Its last step: copy start."), "{error}");
  assert!(
    elapsed >= Duration::from_secs(4),
    "it was given more than the normal bound: {elapsed:?}"
  );
  assert!(
    elapsed < Duration::from_secs(20),
    "the warning and the unknown line did not extend the wait: {elapsed:?}"
  );
  assert_eq!(log.import().warnings().len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn an_import_error_fails_at_once_with_the_repair_lines() {
  const EXPECTED: &str = "The database conversion failed: verify stage: verifier did not pass\n\
     Repair step output:\n  chapter: 2 rows would change\n  chapter: 2 rows applied";
  // The process stays alive after its error line: nothing is waited out.
  let (result, _, elapsed) = boot_import("import-error-hang", &bounds(120, 600, 3600)).await;
  assert_eq!(result.err().as_deref(), Some(EXPECTED));
  assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");
  // The real server exits right after the line: the same words, not "stopped while starting".
  let (result, log, elapsed) = boot_import("import-error", &bounds(120, 600, 3600)).await;
  assert_eq!(result.err().as_deref(), Some(EXPECTED));
  assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");
  assert!(log
    .text()
    .contains("sqlite import: error: verify stage: verifier did not pass"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_running_conversion_outlives_the_normal_bound_and_becomes_ready() {
  // About 3 seconds of conversion against a 1 second normal bound.
  let (result, log, elapsed) = boot_import("import-ok", &bounds(1, 5, 60)).await;
  assert_eq!(result, Ok(()));
  assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");
  let import = log.import();
  assert_eq!(import.state(), crate::import::ImportState::Done);
  assert_eq!(import.bound_kind(), "normal");
  assert_eq!(
    import.verifier_pass(),
    Some("sqlite import: verifier PASS: 63 tables")
  );
  assert!(import.renamed().is_some());
  assert!(import.progress_lines() >= 12);
}

#[cfg(unix)]
#[tokio::test]
async fn a_finished_conversion_returns_to_the_normal_bound() {
  let (result, _, elapsed) = boot_import("import-done-hang", &bounds(1, 30, 60)).await;
  assert_eq!(
    result.err().as_deref(),
    Some("The database was converted, but the server did not answer within 1 seconds after that.")
  );
  assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");
}

/// Poll until the start status reports the import state `wanted`.
#[cfg(unix)]
async fn until_import(s: &LocalServer, wanted: &str) {
  let deadline = Instant::now() + Duration::from_secs(20);
  while s.status().import != wanted {
    assert!(
      Instant::now() < deadline,
      "import state {wanted} not reached: {:?}",
      s.status()
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
}

/// A5: the page asks to start again while the server converts the old database (a
/// reload, or Back and then Run). That used to end the conversion and start it over.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_during_a_conversion_is_ignored_and_the_conversion_goes_on() {
  let launch = fixture("import-ok"); // about 3 seconds of conversion
  let s = server(launch.clone(), FakeEngine::ready(9), free_port());
  s.start();
  until_import(&s, "converting").await;
  s.start();
  s.start();
  // A request that was already on its way to the supervisor when the conversion began.
  lock(&s.tx).as_ref().unwrap().send(Cmd::Retry).unwrap();
  let during = s.status();
  assert_eq!(
    (during.stage, during.error.clone()),
    ("server", None),
    "the page still sees the running start"
  );
  let st = settle(&s, 30).await;
  assert_eq!((st.stage, st.error.clone()), ("ready", None));
  assert_eq!(
    st.import, "done",
    "the conversion that was running finished"
  );
  assert_eq!(
    runs(&launch).len(),
    1,
    "the server process was not started a second time: {:?}",
    runs(&launch)
  );
  assert!(
    s.log_text().contains("a retry request was ignored"),
    "{}",
    s.log_text()
  );
  s.stop().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_while_the_old_database_is_checked_is_ignored() {
  let launch = fixture("import-prepare"); // checks for ever
  let s = server(launch.clone(), FakeEngine::ready(9), free_port());
  s.start();
  until_import(&s, "preparing").await;
  s.start();
  lock(&s.tx).as_ref().unwrap().send(Cmd::Retry).unwrap();
  tokio::time::sleep(Duration::from_millis(1500)).await;
  assert_eq!(runs(&launch).len(), 1, "{:?}", runs(&launch));
  assert_eq!(s.status().import, "preparing");
  // Stop is never ignored.
  let pid = runs(&launch)[0].split(' ').next().unwrap().to_string();
  s.stop().await;
  assert!(!alive(&pid), "the server was stopped");
}

/// M6: the server prints its first conversion line after `set_network` has looked at the
/// conversion and before the supervisor reads the restart request. The supervisor puts
/// the restart off. The caller used to wait for a server that was never restarted and
/// got "The server did not come back in time."; the true answer is the one for a
/// conversion: saved, applied when it is finished.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_network_change_that_meets_the_first_conversion_line_is_answered_as_put_off() {
  let launch = fixture("hang"); // the server process starts and prints nothing
  let s = server(launch.clone(), FakeEngine::ready(9), free_port());
  s.start();
  let deadline = Instant::now() + Duration::from_secs(30);
  while runs(&launch).is_empty() {
    assert!(Instant::now() < deadline, "the server process was started");
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  assert_eq!(
    s.status().import,
    "idle",
    "control: no conversion is seen yet"
  );
  let asked = Instant::now();
  let answer = tokio::time::timeout(
    Duration::from_secs(30),
    s.set_network_with(true, || {
      s.server_log
        .push("sqlite import: source inventory start: 1060864 bytes");
    }),
  )
  .await
  .expect("the switch is answered while the conversion runs");
  assert_eq!(answer, Err(NETWORK_AFTER_CONVERSION.to_string()));
  assert!(
    asked.elapsed() < Duration::from_secs(5),
    "{:?}",
    asked.elapsed()
  );
  assert!(s.network().enabled, "the choice is saved");
  assert_eq!(s.status().import, "converting");
  assert_eq!(
    runs(&launch).len(),
    1,
    "the server process was not restarted: {:?}",
    runs(&launch)
  );
  assert!(s.log_text().contains("was put off"), "{}", s.log_text());
  s.stop().await;
}

/// F5: "Allow other devices" is switched while the server converts the old database.
/// The restart that applies the choice used to end the conversion. It is put off: the
/// conversion finishes, then the server starts again on the chosen address.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_network_change_during_a_conversion_is_applied_after_it() {
  let launch = fixture("import-ok"); // about 3 seconds of conversion
  let port = free_port();
  let s = server(launch.clone(), FakeEngine::ready(9), port);
  s.start();
  until_import(&s, "converting").await;
  // The switch answers at once, with words for the page, and saves the choice.
  let asked = Instant::now();
  let answer = s.set_network(true).await;
  assert_eq!(answer, Err(NETWORK_AFTER_CONVERSION.to_string()));
  assert!(answer
    .unwrap_err()
    .contains("as soon as the conversion is finished"));
  assert!(
    asked.elapsed() < Duration::from_secs(2),
    "{:?}",
    asked.elapsed()
  );
  assert!(s.network().enabled, "the choice is saved");
  // A restart request that was already on its way to the supervisor.
  lock(&s.tx).as_ref().unwrap().send(Cmd::Restart).unwrap();
  tokio::time::sleep(Duration::from_millis(700)).await;
  assert_eq!(
    runs(&launch).len(),
    1,
    "the converting server was not ended: {:?}",
    runs(&launch)
  );
  let during = s.status();
  assert_eq!(
    (during.stage, during.error.clone(), during.import),
    ("server", None, "converting")
  );
  assert_eq!(fake_env(&launch)["BIND_ADDR"], "127.0.0.1");
  // The conversion finishes; only then the server is started again, on every interface.
  let st = settle(&s, 40).await;
  assert_eq!((st.stage, st.error.clone()), ("ready", None));
  let all = runs(&launch);
  assert_eq!(all.len(), 2, "one restart, after the conversion: {all:?}");
  assert_eq!(fake_env(&launch)["BIND_ADDR"], "0.0.0.0");
  assert!(
    !alive(all[0].split(' ').next().unwrap()),
    "old process gone"
  );
  let log = s.log_text();
  assert!(
    log.contains("a restart for a changed network choice was put off"),
    "{log}"
  );
  let finished = log
    .find("sqlite import: complete:")
    .expect("the first conversion finished");
  let restarted = log
    .find("the network choice changed while the server started")
    .expect("restarted for the choice");
  assert!(finished < restarted, "{log}");
  assert_eq!(s.network().port, Some(port));
  s.stop().await;
}

/// Retry stays what it was once the conversion has failed: it starts the server again.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_a_failed_conversion_starts_the_server_again() {
  let launch = fixture("import-error");
  let s = server(launch.clone(), FakeEngine::ready(9), free_port());
  s.start();
  let st = settle(&s, 30).await;
  assert_eq!(st.stage, "server");
  assert!(
    st.error
      .as_deref()
      .is_some_and(|e| e.starts_with("The database conversion failed")),
    "{st:?}"
  );
  assert_eq!(runs(&launch).len(), 1);
  s.start(); // Retry
  let deadline = Instant::now() + Duration::from_secs(20);
  while runs(&launch).len() < 2 && Instant::now() < deadline {
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  assert_eq!(runs(&launch).len(), 2, "Retry started the server again");
  let st = settle(&s, 30).await;
  assert!(st.error.is_some(), "and it failed the same way");
  s.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn import_state_and_warnings_reach_the_start_status_and_the_log_view() {
  let s = server(
    fixture("import-noop-ready"),
    FakeEngine::ready(9),
    free_port(),
  );
  s.start();
  let st = settle(&s, 30).await;
  assert_eq!((st.stage, st.error.clone()), ("ready", None));
  assert_eq!(st.import, "noop");
  assert_eq!(
    st.warnings,
    vec![
      "Database conversion: PostgreSQL already holds data or a completed import; leaving \
       SQLite in place"
        .to_string()
    ]
  );
  assert_eq!(st.progress, None);
  let log = s.log_text();
  assert!(
    log.starts_with(
      "── Warnings ──\nDatabase conversion: PostgreSQL already holds data or a completed import"
    ),
    "{log}"
  );
  s.stop().await;
}

/// N3: what the supervisor found about an earlier run's processes is in the status the
/// start screen reads and in the log view, and is not lost when the log ring turns over.
#[test]
fn supervisor_warnings_reach_status_and_log_view() {
  let _serial = crate::child::WARNING_TEST_LOCK
    .lock()
    .unwrap_or_else(|e| e.into_inner());
  let mut launch = fixture("ready");
  // The real app (a PostgreSQL runtime is configured) also takes the child module's
  // own warnings. Nothing is started in this test.
  launch.postgres_runtime = Some(PathBuf::from("/nonexistent/runtime"));
  let s = LocalServer::new(launch, FakeEngine::ready(1), LogBuf::new(), false);
  assert!(s.status().warnings.is_empty());
  assert!(!s.log_text().contains("── Warnings ──"));
  let orphan = "A program from an earlier run may still be running and was left alone: \
                process 4242 (/Applications/Komiq Server.app/Contents/MacOS/komika-server)."
    .to_string();
  s.note_warning(orphan.clone());
  s.note_warning(orphan.clone());
  let queued = format!(
    "Could not record which program process {} is",
    std::process::id()
  );
  crate::child::warn(queued.clone());
  let st = s.status();
  assert_eq!(st.warnings, vec![orphan.clone(), queued.clone()]);
  for i in 0..(crate::logbuf::MAX_LINES * 2) {
    s.database_log.push(&format!("line {i}"));
  }
  let log = s.log_text();
  assert!(log.starts_with("── Warnings ──\n"), "{log}");
  assert!(log.contains(&orphan) && log.contains(&queued));
  assert_eq!(s.status().warnings.len(), 2, "kept for the whole app run");
}

#[test]
fn pick_port_moves_past_a_taken_port() {
  let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
  let taken = held.local_addr().unwrap().port();
  let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
  let got = pick_port(lo, taken, &[]).unwrap();
  assert_ne!(got, taken);
  let again = pick_port(lo, got, &[got]).unwrap();
  assert_ne!(again, got, "avoid list respected");
  drop(held);
}

// ---- the start sequence -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn start_reports_ready_and_hands_out_the_code_once() {
  let launch = fixture("setup");
  let port = free_port();
  let s = server(launch.clone(), FakeEngine::ready(9), port);
  s.start();
  let st = settle(&s, 30).await;
  assert_eq!(st.error, None);
  assert_eq!(st.stage, "ready");
  assert_eq!(st.address, Some(format!("http://127.0.0.1:{port}")));
  assert_eq!(st.setup_code.as_deref(), Some("TEST-CODE-0042"));
  assert_eq!(s.status().setup_code, None, "handed out once");

  let log = s.log_text();
  assert!(log.contains("── Source engine ──") && log.contains("── Server ──"));
  assert!(!log.contains("TEST-CODE-0042"));
  assert_eq!(fake_env(&launch)["SUWAYOMI_URL"], link(9).url);

  // Quit: the process is gone.
  #[cfg(unix)]
  let pid = runs(&launch)[0].split(' ').next().unwrap().to_string();
  s.stop().await;
  #[cfg(unix)]
  assert!(!alive(&pid), "server stopped on exit");
  assert_eq!(s.status().stage, "database");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_set_up_server_reports_no_code() {
  let s = server(fixture("ready"), FakeEngine::ready(9), free_port());
  s.start();
  let st = settle(&s, 30).await;
  assert_eq!(st.stage, "ready");
  assert_eq!(st.setup_code, None);
  s.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_taken_port_moves_the_server_and_is_remembered() {
  let launch = fixture("ready");
  let held = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
  let taken = held.local_addr().unwrap().port();
  let s = server(launch.clone(), FakeEngine::ready(9), taken);
  s.start();
  let st = settle(&s, 30).await;
  let address = st.address.expect("ready");
  assert_ne!(address, format!("http://127.0.0.1:{taken}"));
  let saved: serde_json::Value =
    serde_json::from_slice(&std::fs::read(launch.data_dir.join("desktop.json")).unwrap()).unwrap();
  assert_eq!(
    format!("http://127.0.0.1:{}", saved["port"]),
    address,
    "new port remembered"
  );
  s.stop().await;
  drop(held);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_failure_is_reported_and_retry_recovers() {
  let engine = FakeEngine::ready(9);
  engine.set(Some(Err("no java runtime found".into())));
  let s = server(fixture("ready"), engine.clone(), free_port());
  s.start();
  let st = settle(&s, 10).await;
  assert_eq!(st.stage, "engine");
  assert!(st.error.unwrap().contains("no java runtime found"));

  engine.set(Some(Ok(link(9))));
  s.start(); // Retry
  let st = settle(&s, 30).await;
  assert_eq!((st.stage, st.error), ("ready", None));
  // Retry asks for the engine again: one that never launched gets a second start.
  assert_eq!(engine.1.load(std::sync::atomic::Ordering::SeqCst), 2);
  s.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_failure_is_reported_and_retry_recovers() {
  let launch = fixture("crash1");
  let s = server(launch.clone(), FakeEngine::ready(9), free_port());
  s.start();
  let st = settle(&s, 20).await;
  assert_eq!(st.stage, "server");
  assert!(st.error.unwrap().contains("stopped while starting"));
  assert!(
    s.log_text().contains("database is locked"),
    "cause in the log view"
  );

  s.start(); // Retry
  let st = settle(&s, 30).await;
  assert_eq!((st.stage, st.error), ("ready", None));
  assert_eq!(runs(&launch).len(), 2);
  s.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crashing_server_is_restarted_then_reported() {
  let launch = fixture("flaky");
  let s = server(launch.clone(), FakeEngine::ready(9), free_port());
  s.start();
  assert_eq!(settle(&s, 30).await.stage, "ready");
  // Each run lives ~1 s; after MAX_RESTARTS_PER_MIN restarts the start reports failure.
  let deadline = Instant::now() + Duration::from_secs(40);
  let st = loop {
    let st = s.status();
    if st.error.is_some() || Instant::now() > deadline {
      break st;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
  };
  assert!(st.error.unwrap().contains("keeps stopping"));
  assert_eq!(runs(&launch).len(), MAX_RESTARTS_PER_MIN + 1);
  s.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn home_network_rebinds_on_the_same_port() {
  let launch = fixture("ready");
  let port = free_port();
  let s = server(launch.clone(), FakeEngine::ready(9), port);
  assert!(!s.network().enabled, "off by default");
  s.start();
  assert_eq!(settle(&s, 30).await.stage, "ready");

  let net = s.set_network(true).await.expect("restarts");
  assert!(net.enabled);
  assert_eq!(net.port, Some(port));
  assert!(net
    .addresses
    .iter()
    .all(|a| a.starts_with("http://") && a.ends_with(&format!(":{port}"))));
  assert_eq!(fake_env(&launch)["BIND_ADDR"], "0.0.0.0");
  assert_eq!(s.status().address, Some(format!("http://127.0.0.1:{port}")));

  let net = s.set_network(false).await.expect("restarts");
  assert!(!net.enabled && net.addresses.is_empty());
  assert_eq!(fake_env(&launch)["BIND_ADDR"], "127.0.0.1");
  let all = runs(&launch);
  assert_eq!(all.len(), 3, "{all:?}");
  #[cfg(unix)]
  for run in &all[..2] {
    assert!(!alive(run.split(' ').next().unwrap()), "old process gone");
  }
  s.stop().await;

  // The choice is remembered for the next launch.
  let saved: serde_json::Value =
    serde_json::from_slice(&std::fs::read(launch.data_dir.join("desktop.json")).unwrap()).unwrap();
  assert_eq!(saved["lan"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_follows_an_engine_restart() {
  let launch = fixture("ready");
  let engine = FakeEngine::ready(9);
  let s = server(launch.clone(), engine.clone(), free_port());
  s.start();
  assert_eq!(settle(&s, 30).await.stage, "ready");
  engine.set(Some(Ok(link(10))));
  let deadline = Instant::now() + Duration::from_secs(20);
  while runs(&launch).len() < 2 && Instant::now() < deadline {
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
  assert_eq!(settle(&s, 30).await.stage, "ready");
  assert_eq!(fake_env(&launch)["SUWAYOMI_URL"], link(10).url);
  s.stop().await;
}

/// The real komika-server as the local server: fresh data dir → a setup code is handed
/// out; after the owner is created, a second launch on the same data dir hands out none.
/// Run with `KOMIKA_TEST_SERVER_BIN=/path/to/komika-server cargo test -- --ignored real_server`
/// (and optionally `KOMIKA_TEST_PORT`). The code is used, never printed.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn real_server_first_run_then_restart() {
  let Some(bin) = std::env::var_os("KOMIKA_TEST_SERVER_BIN") else {
    return;
  };
  let port: u16 = std::env::var("KOMIKA_TEST_PORT")
    .ok()
    .and_then(|p| p.parse().ok())
    .unwrap_or_else(free_port);
  let mut launch = fixture("ready");
  launch.bin = PathBuf::from(bin);
  launch.web_dir = None;
  launch.postgres_runtime = Some(PathBuf::from(
    std::env::var_os("KOMIKA_POSTGRES_RUNTIME").expect("staged runtime required"),
  ));

  let s = server(launch.clone(), FakeEngine::ready(1), port);
  s.start();
  let st = settle(&s, 90).await;
  assert_eq!(
    (st.stage, st.error.clone()),
    ("ready", None),
    "{}",
    s.log_text()
  );
  let address = st.address.unwrap();
  let code = st.setup_code.expect("a fresh server hands out its code");
  assert!(crate::logbuf::find_setup_code(&format!("setup code {code}")).is_some());
  assert!(!s.log_text().contains(&code), "code kept out of the log");

  let q = serde_json::json!({
    "query": "mutation($c: String!) { createOwner(setupCode: $c, username: \"owner\", \
              email: \"owner@example.test\", password: \"made-up-test-pass\") { token } }",
    "variables": { "c": code },
  });
  let raw = reqwest::Client::new()
    .post(format!("{address}/graphql"))
    .header(reqwest::header::CONTENT_TYPE, "application/json")
    .body(q.to_string())
    .send()
    .await
    .unwrap()
    .bytes()
    .await
    .unwrap();
  let resp: serde_json::Value = serde_json::from_slice(&raw).unwrap();
  assert!(resp["errors"].is_null(), "createOwner failed");
  s.stop().await;
  assert!(
    !wait_healthy(port, Duration::from_millis(300)).await,
    "stopped"
  );

  // Second launch: same data, no code, straight to sign-in.
  let s = Arc::new(LocalServer::new(
    launch,
    FakeEngine::ready(1),
    LogBuf::new(),
    true,
  ));
  s.start();
  let st = settle(&s, 90).await;
  assert_eq!(st.stage, "ready");
  assert_eq!(st.address.as_deref(), Some(address.as_str()), "same port");
  assert_eq!(st.setup_code, None);
  assert!(!needs_setup(port).await);
  s.stop().await;
}

/// The server is tied to the app (src/child.rs): when what spawned it goes away without
/// stopping it (a force-killed app), it ends too and its port is free again.
#[cfg(target_os = "linux")]
#[test]
fn the_server_ends_when_the_app_is_killed() {
  let launch = fixture("ready");
  let port = free_port();
  std::thread::spawn({
    let launch = launch.clone();
    move || {
      let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
      rt.block_on(async {
        let child = boot_server(
          &launch,
          &attempt(port),
          &LogBuf::new(),
          &Arc::new(Mutex::new(None)),
          &within(20),
        )
        .await
        .expect("fake server boots");
        // No stop, no kill_on_drop: only the kernel can end it now.
        std::mem::forget(child);
      })
    }
  })
  .join()
  .unwrap();
  let deadline = Instant::now() + Duration::from_secs(10);
  while std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok()
    && Instant::now() < deadline
  {
    std::thread::sleep(Duration::from_millis(100));
  }
  assert!(
    std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(),
    "the server stopped with the app"
  );
}

#[test]
fn database_stage_import_progress_and_server_environment_contract() {
  let launch = fixture("ready");
  let s = LocalServer::new(launch.clone(), FakeEngine::ready(1), LogBuf::new(), false);
  assert_eq!(s.status().stage, "database");
  s.set_stage(Stage::Engine);
  assert_eq!(s.status().stage, "engine");
  s.set_stage(Stage::Server);
  assert_eq!(s.status().import, "idle");
  s.server_log
    .push("sqlite import: preparing (checking source and target)");
  assert_eq!(s.status().import, "preparing");
  s.server_log
    .push("sqlite import: table users complete: 3 rows, 0.001s, 553 COPY bytes");
  let status = s.status();
  assert_eq!(status.import, "converting");
  assert_eq!(
    status.progress.as_deref(),
    Some("sqlite import: table users complete: 3 rows, 0.001s, 553 COPY bytes")
  );
  s.server_log.reset_import();
  let status = s.status();
  assert!(
    status.progress.is_none() && status.import == "idle",
    "previous launch cannot extend this launch's health wait"
  );
  s.set_ready(7580, false, None);
  assert_eq!(s.status().stage, "ready");
  let env = server_env(
    &launch,
    &Attempt {
      bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
      port: 7580,
      metrics_port: 7581,
      engine: link(1),
      database_url: "postgres://komika_app:masked@127.0.0.1:1234/komika".into(),
    },
  )
  .into_iter()
  .collect::<std::collections::BTreeMap<_, _>>();
  assert!(env["DATABASE_URL"].starts_with("postgres://"));
  assert_eq!(
    env["COVERS_DATABASE_URL"],
    format!(
      "sqlite://{}",
      launch.data_dir.join("covers.sqlite3").display()
    )
  );
  assert_eq!(
    env["KOMIKA_SQLITE_IMPORT_PATH"],
    launch.data_dir.join("komika.sqlite3").display().to_string()
  );
}
