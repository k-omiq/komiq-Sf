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
  std::fs::write(
    &bin,
    format!("#!/bin/sh\nexec python3 -u '{}' {mode}\n", fake.display()),
  )
  .unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
  }
  Launch {
    bin,
    web_dir: Some(dir.join("web")),
    data_dir: dir.join("data"),
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
struct FakeEngine(Mutex<Option<Result<EngineLink, String>>>);

impl FakeEngine {
  fn ready(port: u16) -> Arc<Self> {
    Arc::new(FakeEngine(Mutex::new(Some(Ok(link(port))))))
  }
  fn set(&self, v: Option<Result<EngineLink, String>>) {
    *lock(&self.0) = v;
  }
}

impl Engine for FakeEngine {
  fn ensure_started(&self) {}
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

// ---- boot_server ---------------------------------------------------------------------

#[tokio::test]
async fn boot_waits_for_health_and_catches_the_code() {
  let launch = fixture("setup");
  let log = LogBuf::new();
  let slot = Arc::new(Mutex::new(None));
  let port = free_port();
  let mut child = boot_server(
    &launch,
    &attempt(port),
    &log,
    &slot,
    Duration::from_secs(20),
  )
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
    .starts_with(&format!("sqlite://{}", launch.data_dir.display())));

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
    Duration::from_secs(20),
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
    Duration::from_secs(20),
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
  let r = boot_server(
    &launch,
    &attempt(free_port()),
    &LogBuf::new(),
    &Arc::new(Mutex::new(None)),
    Duration::from_secs(1),
  )
  .await;
  match r {
    Err(BootError::Failed(e)) => assert!(e.contains("did not answer"), "{e}"),
    other => panic!("expected a timeout, got {:?}", other.map(|_| ())),
  }
  let pid = runs(&launch)[0].split(' ').next().unwrap().to_string();
  assert!(!alive(&pid), "the silent server was stopped");
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
  let pid = runs(&launch)[0].split(' ').next().unwrap().to_string();
  s.stop().await;
  #[cfg(unix)]
  assert!(!alive(&pid), "server stopped on exit");
  assert_eq!(s.status().stage, "engine");
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
