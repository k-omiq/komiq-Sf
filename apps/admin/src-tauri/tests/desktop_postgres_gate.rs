#![cfg(feature = "desktop-gate")]
//! Native-only gate. Run with --ignored --exact desktop_postgres_gate.
//! The worker drives LocalServer (the app's exact database/server lifecycle) headlessly.
use app_lib::{local, logbuf, postgres};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The longest the gate waits for one start. The supervisor's own waits (src/local.rs
/// SERVER_READY_TIMEOUT) end a start that hangs.
const GATE_READY: Duration = Duration::from_secs(15 * 60);

/// Tables whose row count background projection, maintenance and retention jobs can
/// change while the gate runs. Their content hashes are not compared either.
const NOT_COMPARED: &[(&str, &str)] = &[
  (
    "maintenance_flag",
    "start work records that a one-time repair is done (1 to 2 rows, 10 s)",
  ),
  (
    "browse_catalogue",
    "projection rebuilt from the works by a start job (1 to 15 rows, 20 s)",
  ),
  (
    "feed_series_updates",
    "projection rebuilt from the chapters by a start job (1 to 14 rows, 20 s)",
  ),
  (
    "feed_updates",
    "projection rebuilt from the chapters by a start job (1 to 14 rows, 20 s)",
  ),
  (
    "feed_genre_facet",
    "genre counts rebuilt from the works by a start job (1 to 3 rows, 20 s)",
  ),
  (
    "tag",
    "tag list rebuilt from the works by a start job (1 to 3 rows, 20 s)",
  ),
  (
    "release_event",
    "release ledger filled in for the existing chapters (1 to 19 rows, 91 s)",
  ),
  (
    "notifications",
    "the server's cleanup job deletes notifications that were read and are older than its kept window (apps/server/src/gc.rs); the fixture's one read notification, of 2020, goes (3 to 2 rows, 151 s)",
  ),
  (
    "series_view_bucket",
    "hourly view counts older than the kept window are pruned (1 to 0 rows, 151 s)",
  ),
  (
    "source_health_bucket",
    "hourly source health older than the kept window is pruned (1 to 0 rows, 151 s)",
  ),
];
/// `instance_setting` is compared by content: every seeded row must keep its value and
/// time. Extra rows are accepted only for keys the server writes by itself.
const SETTINGS_TABLE: &str = "instance_setting";

/// The rows of `instance_setting` as one JSON text: `[[key, value, updated_at], ...]`,
/// ordered by key, the time in UTC with microseconds.
const SETTINGS_ROWS: &str = "SELECT coalesce(jsonb_agg(jsonb_build_array(key, value, \
  to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')) ORDER BY key), \
  '[]'::jsonb)::text FROM instance_setting";

/// Why a row of `instance_setting` that the fixture does not have may be there, or `None`
/// when it may not.
///
/// * `direct_tls_generation`, a UUID: written by every server start that finds none (see
///   [`SETTINGS_TABLE`]). This is the row the gate meets.
/// * `catalogue_sync_generation:<job>`, a whole number: written when the server resets the
///   state of a catalogue job (`reset_sync_state`, apps/server/src/catalog/mod.rs). The
///   gate runs the server with catalogue sync off and has never met such a row; the key
///   is accepted by decision of the round 5 brief, because a server with other settings
///   writes it.
fn server_written_setting(key: &str, value: &str) -> Option<&'static str> {
  let uuid = value.len() == 36
    && value.bytes().enumerate().all(|(at, byte)| match at {
      8 | 13 | 18 | 23 => byte == b'-',
      _ => byte.is_ascii_hexdigit(),
    });
  if key == "direct_tls_generation" && uuid {
    return Some("written by every server start that finds none (resume_direct_tls)");
  }
  if key
    .strip_prefix("catalogue_sync_generation:")
    .is_some_and(|job| !job.is_empty())
    && !value.is_empty()
    && value.bytes().all(|byte| byte.is_ascii_digit())
  {
    return Some("written when the server resets a catalogue job (reset_sync_state)");
  }
  None
}

/// `[[key, value, updated_at], ...]` as rows.
fn parse_settings(text: &str) -> Result<Vec<[String; 3]>, String> {
  serde_json::from_str(text).map_err(|e| format!("Invalid instance_setting rows ({e}): {text}"))
}

/// The rule of [`SETTINGS_TABLE`] for the rows `now` of a database. Gives the extra rows
/// that were accepted, each with the reason.
fn settings_check(
  what: &str,
  fixture: &[[String; 3]],
  now: &[[String; 3]],
) -> Result<Value, String> {
  let instant = |text: &str| {
    chrono::DateTime::parse_from_rfc3339(text)
      .map_err(|e| format!("{what}: instance_setting time `{text}` is not a time: {e}"))
  };
  for [key, value, at] in fixture {
    let Some([_, now_value, now_at]) = now.iter().find(|row| &row[0] == key) else {
      return Err(format!(
        "{what}: the fixture's instance_setting row `{key}` is not in the database"
      ));
    };
    if now_value != value || instant(now_at)? != instant(at)? {
      return Err(format!(
        "{what}: the instance_setting row `{key}` differs from the fixture: fixture ({value}, {at}), now ({now_value}, {now_at})"
      ));
    }
  }
  let mut extra = Vec::new();
  for [key, value, _] in now {
    if fixture.iter().any(|row| &row[0] == key) {
      continue;
    }
    match server_written_setting(key, value) {
      Some(reason) => extra.push(json!({"key": key, "accepted_because": reason})),
      None => {
        return Err(format!(
          "{what}: instance_setting has a row that neither the fixture nor a known server start writes: `{key}` = `{value}`"
        ))
      }
    }
  }
  if now
    .iter()
    .map(|row| &row[0])
    .collect::<std::collections::BTreeSet<_>>()
    .len()
    != now.len()
  {
    return Err(format!("{what}: instance_setting has a key twice: {now:?}"));
  }
  Ok(
    json!({"fixture_rows_present_with_equal_content": fixture.len(), "rows_now": now.len(),
    "extra_rows_written_by_the_server": extra}),
  )
}

/// Every table of the `public` schema with its row count, in one snapshot: `name|rows`.
const COUNT_ALL: &str = "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; \
  SELECT table_name || '|' || (xpath('/row/c/text()', query_to_xml(format(\
  'SELECT count(*) AS c FROM public.%I', table_name), false, true, '')))[1]::text \
  FROM information_schema.tables WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
  ORDER BY table_name; COMMIT;";

/// Every table of the `public` schema with a hash of its whole content, in one snapshot:
/// `name|md5`. Each row is taken as text and the rows are ordered, so the hash does not
/// depend on the order in which they are stored.
const HASH_ALL: &str = "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; \
  SELECT table_name || '|' || (xpath('/row/h/text()', query_to_xml(format(\
  'SELECT md5(coalesce(string_agg(t::text, chr(10) ORDER BY t::text), '''')) AS h \
  FROM public.%I AS t', table_name), false, true, '')))[1]::text \
  FROM information_schema.tables WHERE table_schema = 'public' AND table_type = 'BASE TABLE' \
  ORDER BY table_name; COMMIT;";

type Counts = std::collections::BTreeMap<String, u64>;
type Hashes = std::collections::BTreeMap<String, String>;

fn parse_hashes(rows: &str) -> Result<Hashes, String> {
  rows
    .lines()
    .map(|line| {
      let (table, hash) = line
        .split_once('|')
        .filter(|(_, hash)| hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| format!("Invalid hash line: {line}"))?;
      Ok((table.to_string(), hash.to_string()))
    })
    .collect()
}

/// Compared tables whose content hash in `other` is not the one in `live`.
fn hash_differences(live: &Hashes, other: &Hashes) -> Vec<String> {
  live
    .iter()
    .filter(|(table, _)| !NOT_COMPARED.iter().any(|(name, _)| name == table))
    .filter(|(table, hash)| other.get(*table) != Some(hash))
    .map(|(table, _)| table.clone())
    .collect()
}

/// Compared tables whose content in a restored dump (`restored`) is not content the live
/// database had when the dump was made: neither what it held right before the dump
/// (`before`) nor what it held right after (`after`).
///
/// A dump is the database at one moment, and the server writes by itself while the gate
/// works (its scanner tries a series a moment after the start, fails without a source
/// engine, and writes the next time into `series_scan_state`; the moment is random). So
/// the live database a few seconds later is not what the dump has to equal. A table
/// that was the same before and after must be that in the dump; a table that the server
/// changed in between must be one of the two.
///
/// The three lists of tables must be the same as well: a table that is in only one or
/// two of the live database before, the live database after and the dump is a
/// difference, whatever it holds.
fn dump_differences(before: &Hashes, after: &Hashes, restored: &Hashes) -> Vec<String> {
  let tables: std::collections::BTreeSet<&String> = before
    .keys()
    .chain(after.keys())
    .chain(restored.keys())
    .collect();
  tables
    .into_iter()
    .filter(|table| !NOT_COMPARED.iter().any(|(name, _)| name == *table))
    .filter(
      |table| match (before.get(*table), after.get(*table), restored.get(*table)) {
        (Some(before), Some(after), Some(restored)) => restored != before && restored != after,
        _ => true,
      },
    )
    .cloned()
    .collect()
}

fn parse_counts(rows: &str) -> Result<Counts, String> {
  rows
    .lines()
    .map(|line| {
      let (table, rows) = line
        .split_once('|')
        .ok_or_else(|| format!("Invalid count line: {line}"))?;
      Ok((
        table.to_string(),
        rows
          .parse::<u64>()
          .map_err(|e| format!("Invalid count for {table}: {e}"))?,
      ))
    })
    .collect()
}

/// Fixture tables whose count in `now` is not the fixture's, compared tables only.
/// [`SETTINGS_TABLE`] is compared by content ([`settings_check`]), not here; it must
/// still be in the database and can never have fewer rows than the fixture.
fn differences(fixture: &Counts, now: &Counts) -> Vec<String> {
  fixture
    .iter()
    .filter(|(table, _)| !NOT_COMPARED.iter().any(|(name, _)| name == table))
    .filter(|(table, rows)| {
      if table.as_str() == SETTINGS_TABLE {
        !now.get(*table).is_some_and(|now| now >= *rows)
      } else {
        now.get(*table) != Some(rows)
      }
    })
    .map(|(table, rows)| format!("{table}: fixture {rows}, now {:?}", now.get(table)))
    .collect()
}

/// A path the gate used, with what the brief's path rule asks about it.
fn path_fact(path: &Path) -> Value {
  let text = path.to_string_lossy();
  json!({"path": text, "has_space": text.contains(' '), "has_non_ascii": !text.is_ascii()})
}

/// The same, failing when the path lacks a space or a non-ASCII character.
fn spaced_non_ascii(what: &str, path: &Path) -> Result<Value, String> {
  let fact = path_fact(path);
  if fact["has_space"] != true || fact["has_non_ascii"] != true {
    return Err(format!(
      "The {what} path must have a space and a non-ASCII character: {fact}"
    ));
  }
  Ok(fact)
}

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
fn atomic_json(path: &Path, value: &Value) -> Result<(), String> {
  let temporary = path.with_extension("json.tmp");
  fs::write(
    &temporary,
    serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
  )
  .map_err(|e| e.to_string())?;
  // On Windows rename doesn't replace existing destination. Output is owned by
  // one writer; readers tolerate the brief absence.
  if path.exists() {
    fs::remove_file(path).map_err(|e| e.to_string())?;
  }
  fs::rename(temporary, path).map_err(|e| e.to_string())
}

#[test]
#[ignore = "native process worker; called only by desktop_postgres_gate"]
fn desktop_gate_worker() {
  if std::env::var("KOMIKA_DESKTOP_GATE_WORKER").as_deref() != Ok("1") {
    return;
  }
  let root = PathBuf::from(std::env::var_os("KOMIKA_DESKTOP_GATE_DATA").expect("private data"));
  let runtime = PathBuf::from(std::env::var_os("KOMIKA_POSTGRES_RUNTIME").expect("staged tree"));
  let server_bin = PathBuf::from(std::env::var_os("KOMIKA_TEST_SERVER_BIN").expect("real server"));
  let launch = local::Launch {
    bin: server_bin,
    web_dir: None,
    data_dir: root.join("server"),
    postgres_runtime: Some(runtime),
  };
  if std::env::var("KOMIKA_DESKTOP_GATE_CLEANUP").as_deref() == Ok("1") {
    let rt = tokio::runtime::Builder::new_multi_thread()
      .enable_all()
      .build()
      .unwrap();
    let result = rt.block_on(local::cleanup_gate(&launch));
    if let Ok(log) = &result {
      let _ = fs::write(root.join("cleanup.log"), log);
    }
    assert!(result.is_ok(), "Gate cleanup failed: {:?}", result.err());
    return;
  }
  let server = Arc::new(local::LocalServer::new(
    launch,
    Arc::new(NoEngine),
    logbuf::LogBuf::new(),
    true,
  ));
  let rt = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
    .unwrap();
  let result: Result<(), String> = rt.block_on(async {
    server.start();
    let deadline = Instant::now() + GATE_READY;
    loop {
      let status = server.status();
      if status.error.is_some() {
        atomic_json(
          &root.join("ready.json"),
          &json!({"status":"fail", "error": status.error, "logs": server.log_text()}),
        )?;
        return Err("Supervisor failed to become ready".into());
      }
      if status.stage == "ready" {
        atomic_json(
          &root.join("ready.json"),
          &json!({"status":"pass", "stage":status.stage,"address":status.address,
            "warnings":status.warnings}),
        )?;
        break;
      }
      if root.join("stop").exists() {
        return Ok(());
      }
      if Instant::now() >= deadline {
        return Err("Worker readiness budget exhausted".into());
      }
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
    loop {
      if root.join("stop").exists() {
        break;
      }
      let request = root.join("request.json");
      if request.exists() {
        let action = serde_json::from_slice::<local::GateAction>(
          &fs::read(&request).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::remove_file(request).map_err(|e| e.to_string())?;
        let result = server.gate(action).await;
        atomic_json(
          &root.join("response.json"),
          &match result {
            Ok(value) => json!({"status":"pass", "value":value}),
            Err(error) => json!({"status":"fail", "error":error}),
          },
        )?;
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
  });
  // Runs on success and reported errors. A forced kill deliberately bypasses it.
  rt.block_on(server.stop());
  let _ = fs::write(root.join("database-and-server.log"), server.log_text());
  assert!(result.is_ok(), "{}", result.err().unwrap_or_default());
}

struct Worker {
  child: Child,
  root: PathBuf,
  runtime: PathBuf,
  server: PathBuf,
  stopped: bool,
  cleanup_pending: bool,
  /// `ready.json` of this start.
  ready: Value,
}
impl Worker {
  /// Start the worker process. `extra`: environment of this start only.
  fn spawn(
    root: &Path,
    runtime: &Path,
    server: &Path,
    extra: &[(&str, &str)],
  ) -> Result<Self, String> {
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    for name in [
      "stop",
      "ready.json",
      "response.json",
      "request.json",
      "cleanup-status.json",
      PRESTART,
    ] {
      let _ = fs::remove_file(root.join(name));
    }
    let out = fs::File::create(root.join("worker.log")).map_err(|e| e.to_string())?;
    let child = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
      .args(["--ignored", "--exact", "desktop_gate_worker", "--nocapture"])
      .env("CATALOGUE_SYNC", "off")
      .env("KOMIKA_MANGADEX", "off")
      .env("COVER_CACHE", "off")
      .env("METADATA_BACKFILL", "off")
      .env("SCAN_TICK_SECONDS", "86400")
      .env("SOURCE_SYNC_INTERVAL_SECONDS", "86400")
      .env("EXTENSION_AUTO_UPDATE_INTERVAL_SECONDS", "0")
      // Never create accounts from the caller's environment (see NOT_COMPARED).
      .env_remove("KOMIKA_ADMIN_USERS")
      .env_remove("KOMIKA_ADMIN_PASSWORD")
      .env_remove("KOMIKA_ADMIN_EMAIL")
      .env_remove("KOMIKA_DESKTOP_GATE_BLIND_STOP")
      .env("KOMIKA_DESKTOP_GATE_WORKER", "1")
      .env(
        "KOMIKA_DESKTOP_GATE_AVOID_PG_PORT",
        fs::read_to_string(root.join("gate-last-port")).unwrap_or_default(),
      )
      .env("KOMIKA_DESKTOP_GATE_DATA", root)
      .env("KOMIKA_DESKTOP_GATE_PRESTART", root.join(PRESTART))
      .env("KOMIKA_POSTGRES_RUNTIME", runtime)
      .env("KOMIKA_TEST_SERVER_BIN", server)
      .envs(extra.iter().copied())
      .stdin(Stdio::null())
      .stdout(out.try_clone().map_err(|e| e.to_string())?)
      .stderr(out)
      .spawn()
      .map_err(|e| e.to_string())?;
    Ok(Self {
      child,
      root: root.to_path_buf(),
      runtime: runtime.to_path_buf(),
      server: server.to_path_buf(),
      stopped: false,
      cleanup_pending: false,
      ready: Value::Null,
    })
  }
  fn start(root: &Path, runtime: &Path, server: &Path) -> Result<Self, String> {
    Self::start_with(root, runtime, server, &[])
  }
  fn start_with(
    root: &Path,
    runtime: &Path,
    server: &Path,
    extra: &[(&str, &str)],
  ) -> Result<Self, String> {
    let mut worker = Self::spawn(root, runtime, server, extra)?;
    let ready = worker.wait_json("ready.json", GATE_READY + Duration::from_secs(30))?;
    if ready["status"] != "pass" {
      return Err(format!("Headless startup failed: {ready}"));
    }
    worker.ready = ready;
    let port = worker.sql("SHOW port")?;
    fs::write(root.join("gate-last-port"), port).map_err(|e| e.to_string())?;
    Ok(worker)
  }
  /// A start that must fail: the error the start page would show.
  fn start_refused(root: &Path, runtime: &Path, server: &Path) -> Result<String, String> {
    let mut worker = Self::spawn(root, runtime, server, &[])?;
    let ready = worker.wait_json("ready.json", GATE_READY + Duration::from_secs(30))?;
    // A start that became ready was not refused. (Dropping the worker stops it.)
    if ready["status"] != "fail" {
      return Err(format!("The start was expected to fail: {ready}"));
    }
    // The worker stops its supervisor and exits by itself after a failed start.
    let deadline = Instant::now() + Duration::from_secs(90);
    while worker
      .child
      .try_wait()
      .map_err(|e| e.to_string())?
      .is_none()
    {
      if Instant::now() >= deadline {
        return Err("The worker of a refused start did not exit".into());
      }
      std::thread::sleep(Duration::from_millis(100));
    }
    worker.stopped = true;
    ready["error"]
      .as_str()
      .map(str::to_string)
      .ok_or_else(|| format!("A failed start without an error: {ready}"))
  }
  fn wait_json(&mut self, name: &str, bound: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + bound;
    loop {
      if let Ok(bytes) = fs::read(self.root.join(name)) {
        if let Ok(json) = serde_json::from_slice(&bytes) {
          return Ok(json);
        }
      }
      if self.child.try_wait().map_err(|e| e.to_string())?.is_some() {
        // It may have written the file in its last moment.
        if let Ok(bytes) = fs::read(self.root.join(name)) {
          if let Ok(json) = serde_json::from_slice(&bytes) {
            return Ok(json);
          }
        }
        return Err(format!(
          "Headless worker exited; inspect {}",
          self.root.join("worker.log").display()
        ));
      }
      if Instant::now() >= deadline {
        return Err(format!("Headless worker timed out waiting for {name}"));
      }
      std::thread::sleep(Duration::from_millis(100));
    }
  }
  fn request_raw(&mut self, value: Value) -> Result<Value, String> {
    let _ = fs::remove_file(self.root.join("response.json"));
    atomic_json(&self.root.join("request.json"), &value)?;
    self.wait_json("response.json", Duration::from_secs(3700))
  }
  fn request(&mut self, value: Value) -> Result<String, String> {
    let answer = self.request_raw(value)?;
    if answer["status"] != "pass" {
      return Err(
        answer["error"]
          .as_str()
          .unwrap_or("Unknown worker error")
          .into(),
      );
    }
    answer["value"]
      .as_str()
      .map(str::to_string)
      .ok_or("Worker response missing value".into())
  }
  fn sql(&mut self, sql: &str) -> Result<String, String> {
    self.request(json!({"action":"sql", "value":sql}))
  }
  /// A content hash of every table of the application database, or of `database`.
  fn hashes(&mut self, database: Option<&str>) -> Result<Hashes, String> {
    parse_hashes(&match database {
      None => self.sql(HASH_ALL)?,
      Some(database) => self.request(json!({"action":"sql_in","value":[database, HASH_ALL]}))?,
    })
  }
  /// Row counts of every table of the application database, or of `database`.
  fn counts(&mut self, database: Option<&str>) -> Result<Counts, String> {
    parse_counts(&match database {
      None => self.sql(COUNT_ALL)?,
      Some(database) => self.request(json!({"action":"sql_in","value":[database, COUNT_ALL]}))?,
    })
  }
  /// The rows of `instance_setting` in the application database, or in `database`.
  fn settings(&mut self, database: Option<&str>) -> Result<Vec<[String; 3]>, String> {
    parse_settings(&match database {
      None => self.sql(SETTINGS_ROWS)?,
      Some(database) => {
        self.request(json!({"action":"sql_in","value":[database, SETTINGS_ROWS]}))?
      }
    })
  }
  /// The Database section of the log view of this start.
  fn database_log(&mut self) -> Result<String, String> {
    self.request(json!({"action":"database_log"}))
  }
  /// The process numbers of this start's database: the one in the lock file, and every
  /// process the database itself lists (its workers and connections).
  fn database_pids(&mut self) -> Result<(u32, Vec<u32>), String> {
    let postmaster = lock_file_pid(&self.root)?;
    let mut all = vec![postmaster];
    for pid in self
      .sql("SELECT string_agg(pid::text, ',') FROM pg_stat_activity")?
      .split(',')
    {
      all.push(pid.trim().parse().map_err(|e| format!("pid {pid}: {e}"))?);
    }
    Ok((postmaster, all))
  }
  fn stop(&mut self) -> Result<(), String> {
    fs::write(self.root.join("stop"), "stop").map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(65);
    loop {
      if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
        self.stopped = status.success();
        if status.success() {
          atomic_json(
            &self.root.join("cleanup-status.json"),
            &json!({"status":"pass","method":"clean_stop"}),
          )?;
        }
        return if status.success() {
          Ok(())
        } else {
          Err(format!("Worker stop failed: {status}"))
        };
      }
      if Instant::now() >= deadline {
        return Err("Worker did not stop cleanly within 65 seconds".into());
      }
      std::thread::sleep(Duration::from_millis(100));
    }
  }
  /// The app is force-quit: SIGKILL on THIS supervisor process, not pg_ctl and not a
  /// name, port or process-tree kill. The database it started is left running (the
  /// next start must find and stop it).
  #[cfg(unix)]
  fn hard_kill(&mut self) -> Result<(), String> {
    self.child.kill().map_err(|e| e.to_string())?;
    self.child.wait().map_err(|e| e.to_string())?;
    self.stopped = true;
    self.cleanup_pending = true;
    std::thread::sleep(Duration::from_secs(2));
    Ok(())
  }
  /// A crash of the whole thing (power loss, as far as a test can go): the database's
  /// main process AND the supervisor are killed without notice. `postmaster` is the
  /// process number read from the lock file before. The database goes first: on Linux
  /// the death of the supervisor sends the database a request to shut down, and a
  /// database that had time to do so did not crash. The supervisor waits two seconds
  /// before it restarts a database, and is killed long before that. Windows: ending the
  /// supervisor ends the database through the Job Object, which is a forced end too.
  fn crash(&mut self, postmaster: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
      let killed = Command::new("kill")
        .args(["-KILL", &postmaster.to_string()])
        .status()
        .map_err(|e| e.to_string())?;
      if !killed.success() {
        return Err(format!("Could not kill the database process {postmaster}"));
      }
    }
    #[cfg(not(unix))]
    let _ = postmaster;
    self.child.kill().map_err(|e| e.to_string())?;
    self.child.wait().map_err(|e| e.to_string())?;
    self.stopped = true;
    self.cleanup_pending = true;
    Ok(())
  }
}

/// `<data>/server/postgres/18/data`.
fn cluster_data(root: &Path) -> PathBuf {
  postgres::cluster_dir(&root.join("server")).join("data")
}

/// The file in a data root where the worker's supervisor writes one line at each of these
/// moments of a start (gate-only hooks in src/postgres.rs), in the order they happen:
/// `stray` (a PostgreSQL of this data path that runs in another folder was dealt with),
/// `initdb` (right before a new cluster is made), `spawn` (right before each PostgreSQL
/// spawn). Every line also has the hash of the control file at the data path and the
/// process numbers of every PostgreSQL of this data path that runs elsewhere, both as
/// they are at that moment.
const PRESTART: &str = "prestart.jsonl";

/// Every line of the latest start on `root`, in order.
fn start_events(root: &Path) -> Result<Vec<Value>, String> {
  let path = root.join(PRESTART);
  fs::read_to_string(&path)
    .map_err(|e| format!("{}: {e}", path.display()))?
    .lines()
    .map(|line| serde_json::from_str(line).map_err(|e| format!("{}: {e}", path.display())))
    .collect()
}

/// What the supervisor recorded at the moment it spawned PostgreSQL for the first time
/// in the latest start on `root`: the lock file as it was then, and the PID given in
/// PG_GRANDPARENT_PID (`null`: the variable was not passed).
fn prestart(root: &Path) -> Result<Value, String> {
  start_events(root)?
    .into_iter()
    .find(|record| record["event"] == "spawn")
    .ok_or_else(|| {
      format!(
        "{} has no spawn: PostgreSQL was never spawned",
        root.join(PRESTART).display()
      )
    })
}

/// One statement sent, as the application role, to the database that runs on the data
/// folder of `root` without an app: its port is in its lock file, the password in the
/// folder's credentials file. With the bundled `psql`, the way the app sends SQL.
#[cfg(unix)]
fn sql_without_app(runtime: &Path, root: &Path, sql: &str) -> Result<String, String> {
  let cluster = postgres::cluster_dir(&root.join("server"));
  let lock = cluster.join("data").join("postmaster.pid");
  let port = fs::read_to_string(&lock)
    .map_err(|e| format!("{}: {e}", lock.display()))?
    .lines()
    .nth(3)
    .map(|line| line.trim().to_string())
    .filter(|port| port.parse::<u16>().is_ok())
    .ok_or_else(|| format!("{} has no port", lock.display()))?;
  let credentials: Value = serde_json::from_slice(
    &fs::read(cluster.join("credentials")).map_err(|e| format!("credentials: {e}"))?,
  )
  .map_err(|e| format!("credentials: {e}"))?;
  let mut command = Command::new(runtime.join("bin").join("psql"));
  for (key, _) in std::env::vars_os() {
    if key.to_string_lossy().to_ascii_uppercase().starts_with("PG") {
      command.env_remove(key);
    }
  }
  let out = command
    .args([
      "-X",
      "-w",
      "-h",
      "127.0.0.1",
      "-p",
      &port,
      "-U",
      "komika_app",
    ])
    .args([
      "-d",
      "komika",
      "-v",
      "ON_ERROR_STOP=1",
      "-A",
      "-t",
      "-q",
      "-c",
      sql,
    ])
    .env(
      "PGPASSWORD",
      credentials["application"]
        .as_str()
        .ok_or("credentials without an application password")?,
    )
    .env("PGCONNECT_TIMEOUT", "5")
    .env("PGCLIENTENCODING", "UTF8")
    .stdin(Stdio::null())
    .output()
    .map_err(|e| e.to_string())?;
  if !out.status.success() {
    return Err(format!(
      "psql on the database without an app failed: {}",
      String::from_utf8_lossy(&out.stderr)
    ));
  }
  Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The system identifier of the cluster whose data folder is `data`, read from its
/// control file (the first eight bytes). Two clusters never have the same one.
#[cfg(unix)]
fn system_identifier(data: &Path) -> Result<u64, String> {
  let path = data.join("global").join("pg_control");
  let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
  bytes
    .get(..8)
    .and_then(|first| first.try_into().ok())
    .map(u64::from_ne_bytes)
    .ok_or_else(|| format!("{} is too short", path.display()))
}

/// A copy of the app data folder `source` at `target`. The files of the database's data
/// folder are not copied: they are the same files under a second name (hard links, as
/// `cp -al` makes them). Everything else is an ordinary copy.
#[cfg(unix)]
fn copy_sharing_database_files(source: &Path, target: &Path, shared: &Path) -> Result<(), String> {
  use std::os::unix::fs::PermissionsExt;
  let io = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
  let meta = fs::symlink_metadata(source).map_err(|e| io(source, e))?;
  fs::create_dir(target).map_err(|e| io(target, e))?;
  fs::set_permissions(
    target,
    fs::Permissions::from_mode(meta.permissions().mode() & 0o7777),
  )
  .map_err(|e| io(target, e))?;
  for entry in fs::read_dir(source).map_err(|e| io(source, e))? {
    let entry = entry.map_err(|e| io(source, e))?;
    let (from, to) = (entry.path(), target.join(entry.file_name()));
    let kind = entry.file_type().map_err(|e| io(&from, e))?;
    if kind.is_dir() {
      copy_sharing_database_files(&from, &to, shared)?;
    } else if kind.is_file() && from.starts_with(shared) {
      fs::hard_link(&from, &to).map_err(|e| io(&to, e))?;
    } else if kind.is_file() {
      fs::copy(&from, &to).map_err(|e| io(&to, e))?;
    }
  }
  Ok(())
}

/// The lock file of `root` as it is now: exists, inode, content hash, lines, first line.
fn lock_state(root: &Path) -> Value {
  postgres::gate_lock_state(&cluster_data(root))
}

/// The supervisor must have spawned PostgreSQL over exactly the lock file `before`
/// (same file, same content: the app neither removed nor rewrote it), and must have
/// passed `grandparent` in PG_GRANDPARENT_PID (`None`: not passed).
fn expect_prestart(root: &Path, before: &Value, grandparent: Option<u32>) -> Result<Value, String> {
  let record = prestart(root)?;
  if &record["lock_file"] != before {
    return Err(format!(
      "The app changed the lock file before PostgreSQL started. Before the start: {before}. At the spawn: {}",
      record["lock_file"]
    ));
  }
  if record["grandparent_pid"] != json!(grandparent) {
    return Err(format!(
      "PG_GRANDPARENT_PID at the spawn was {}, expected {grandparent:?}",
      record["grandparent_pid"]
    ));
  }
  Ok(record)
}

/// Start `command`, trying again for a moment while Linux answers "Text file busy" for
/// a program file that was just written.
#[cfg(unix)]
fn spawn_when_ready(command: &mut Command) -> Result<Child, String> {
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    match command.spawn() {
      Ok(child) => return Ok(child),
      // ETXTBSY
      Err(error) if error.raw_os_error() == Some(26) && Instant::now() < deadline => {
        std::thread::sleep(Duration::from_millis(20));
      }
      Err(error) => return Err(format!("{command:?}: {error}")),
    }
  }
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: &str) -> bool {
  Command::new("kill")
    .args([signal, &pid.to_string()])
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|status| status.success())
}

/// A real database on a data folder that the app did not start in this run: started by
/// the gate itself from a `postgres` program, on a free loopback port, no socket file.
#[cfg(unix)]
struct GatePostmaster {
  child: Child,
}
#[cfg(unix)]
impl GatePostmaster {
  /// `data_argument`: `-D <data>`, the way the app starts its database. Without it the
  /// data folder is given in `PGDATA`, so the process shows no `-D`.
  fn start(program: &Path, data: &Path, data_argument: bool, log: &Path) -> Result<Self, String> {
    let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
      .and_then(|socket| socket.local_addr())
      .map_err(|e| e.to_string())?
      .port();
    let out = fs::File::create(log).map_err(|e| e.to_string())?;
    let mut command = Command::new(program);
    for (key, _) in std::env::vars_os() {
      if key.to_string_lossy().to_ascii_uppercase().starts_with("PG") {
        command.env_remove(key);
      }
    }
    if data_argument {
      command.arg("-D").arg(data);
    } else {
      command.env("PGDATA", data);
    }
    command
      .args(["-p", &port.to_string()])
      .args(["-c", "listen_addresses=127.0.0.1"])
      .args(["-c", "unix_socket_directories="])
      .args(["-c", "logging_collector=off"])
      .stdin(Stdio::null())
      .stdout(out.try_clone().map_err(|e| e.to_string())?)
      .stderr(out);
    let mut started = Self {
      child: spawn_when_ready(&mut command)?,
    };
    // Until its lock file is complete: its own PID first, `ready` in the status line.
    let lock = data.join("postmaster.pid");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
      let text = fs::read_to_string(&lock).unwrap_or_default();
      let mut lines = text.lines();
      if lines.next() == Some(started.pid().to_string().as_str())
        && lines.any(|line| line.trim() == "ready")
      {
        return Ok(started);
      }
      if !started.alive() || Instant::now() >= deadline {
        return Err(format!(
          "The gate's own database did not become ready:\n{}",
          fs::read_to_string(log).unwrap_or_default()
        ));
      }
      std::thread::sleep(Duration::from_millis(50));
    }
  }
  fn pid(&self) -> u32 {
    self.child.id()
  }
  fn alive(&mut self) -> bool {
    matches!(self.child.try_wait(), Ok(None))
  }
  /// Wait for it to end by itself (somebody else asked it to stop). Its exit status.
  fn ended(&mut self, bound: Duration) -> Result<std::process::ExitStatus, String> {
    let deadline = Instant::now() + bound;
    loop {
      if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
        return Ok(status);
      }
      if Instant::now() >= deadline {
        return Err(format!("Database process {} did not end", self.pid()));
      }
      std::thread::sleep(Duration::from_millis(50));
    }
  }
  /// The owner ends it: a fast shutdown request, then the wait.
  fn stop(&mut self) -> Result<std::process::ExitStatus, String> {
    if self.alive() {
      send_signal(self.pid(), "-INT");
    }
    self.ended(Duration::from_secs(60))
  }
}
#[cfg(unix)]
impl Drop for GatePostmaster {
  fn drop(&mut self) {
    if self.stop().is_err() {
      let _ = self.child.kill();
      let _ = self.child.wait();
    }
  }
}

/// A database left running on a data folder without an app: by the force-quit app
/// (macOS), or started by the gate where the system ends the database with the app
/// (Linux). Dropped: whatever is still running of it is ended, so that a failed step
/// leaves no process.
#[cfg(unix)]
struct LeftRunning {
  pid: u32,
  created: Option<String>,
  by_gate: Option<GatePostmaster>,
}
#[cfg(unix)]
impl LeftRunning {
  /// Force-quit the app of `worker`. If its database outlives it, that is the database
  /// left running. Otherwise (Linux asks the database to shut down when the app dies)
  /// the gate starts the database again by itself from `program`, with the same `-D`
  /// argument the app gave it.
  fn after_force_quit(worker: &mut Worker, program: &Path, log: &Path) -> Result<Self, String> {
    let data = cluster_data(&worker.root);
    let postmaster = lock_file_pid(&worker.root)?;
    worker.hard_kill()?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while !cfg!(target_os = "macos")
      && (runs_program(postmaster, program) || data.join("postmaster.pid").exists())
      && Instant::now() < deadline
    {
      std::thread::sleep(Duration::from_millis(100));
    }
    if runs_program(postmaster, program) {
      return Ok(Self {
        pid: postmaster,
        created: postgres::gate_process_created(postmaster),
        by_gate: None,
      });
    }
    let started = GatePostmaster::start(program, &data, true, log)?;
    Ok(Self {
      pid: started.pid(),
      created: postgres::gate_process_created(started.pid()),
      by_gate: Some(started),
    })
  }
  fn left_by(&self) -> &'static str {
    match self.by_gate {
      None => "the force-quit app",
      Some(_) => "the gate (on this system the database ends with the app)",
    }
  }
  /// Still the same live process.
  fn running(&mut self) -> bool {
    match &mut self.by_gate {
      Some(started) => started.alive(),
      None => self.created.is_some() && postgres::gate_process_created(self.pid) == self.created,
    }
  }
  /// It must be gone. `cleanly`: when the gate is its parent, it must also have ended
  /// by an ordinary shutdown (exit status 0).
  fn expect_stopped(&mut self, cleanly: bool) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while self.running() {
      if Instant::now() >= deadline {
        return Err(format!(
          "The earlier database process {} still runs",
          self.pid
        ));
      }
      std::thread::sleep(Duration::from_millis(50));
    }
    match &mut self.by_gate {
      Some(started) => {
        let status = started.ended(Duration::from_secs(5))?;
        if cleanly && !status.success() {
          return Err(format!(
            "The earlier database process {} did not end by an ordinary shutdown: {status}",
            self.pid
          ));
        }
        Ok(json!(status.to_string()))
      }
      None => Ok(json!(
        "not a child of the gate; its exit status is not visible"
      )),
    }
  }
}
#[cfg(unix)]
impl Drop for LeftRunning {
  fn drop(&mut self) {
    if self.by_gate.is_none() && self.running() {
      send_signal(self.pid, "-INT");
      let deadline = Instant::now() + Duration::from_secs(30);
      while self.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
      }
      if self.running() {
        send_signal(self.pid, "-KILL");
      }
    }
  }
}

/// The start of `worker` found `left` (this folder's database, still running), named it,
/// asked it to stop with `pg_ctl` and started over a data folder without a lock file,
/// with nothing passed in PG_GRANDPARENT_PID. `other_program`: the earlier database ran
/// another program file than the one this start uses. `renamed`: its data folder was
/// renamed under it. PostgreSQL opens its control file by the path it was started with,
/// so such a database cannot finish its shutdown: it ends by its own PANIC (PostgreSQL
/// 18.6: `could not open file ".../global/pg_control"`), and the next start recovers
/// what was committed from the write-ahead log, like after a crash.
#[cfg(unix)]
fn expect_adopted(
  worker: &mut Worker,
  left: &mut LeftRunning,
  other_program: bool,
  renamed: bool,
) -> Result<Value, String> {
  let log = worker.database_log()?;
  let line = log
    .lines()
    .find(|line| line.contains(&format!("names process {}", left.pid)))
    .ok_or_else(|| format!("No log line names process {}:\n{log}", left.pid))?
    .to_string();
  if !line.contains("this folder's database is still running") {
    return Err(format!(
      "The earlier database was not seen as this folder's database: {line}"
    ));
  }
  if line.contains("its program is not the one this install starts") != other_program {
    return Err(format!(
      "The log line is wrong about the program of the earlier database: {line}"
    ));
  }
  if log.contains("PG_GRANDPARENT_PID") {
    return Err(format!(
      "PostgreSQL was told to disregard a process:\n{log}"
    ));
  }
  if !log.contains(&format!(
    "The earlier database process {} stopped after pg_ctl asked it to",
    left.pid
  )) {
    return Err(format!(
      "The earlier database was not stopped by pg_ctl:\n{log}"
    ));
  }
  let record = prestart(&worker.root)?;
  if !record["grandparent_pid"].is_null() || record["lock_file"]["exists"] != false {
    return Err(format!(
      "PostgreSQL was not started over a free data folder with nothing to disregard: {record}"
    ));
  }
  let exit = left.expect_stopped(!renamed)?;
  let recovered = log.contains("database system was not properly shut down")
    && log.contains("automatic recovery in progress");
  let clean = log.contains("database system was shut down at") && !recovered;
  if renamed && !recovered {
    return Err(format!(
      "The start after a database that could not finish its shutdown shows no recovery:\n{log}"
    ));
  }
  if !renamed && !clean {
    return Err(format!(
      "The earlier database did not shut down in the ordinary way:\n{log}"
    ));
  }
  Ok(
    json!({"log_line": line, "left_running_by": left.left_by(), "earlier_process": left.pid,
    "earlier_process_exit": exit, "pg_grandparent_pid_passed": false,
    "lock_file_at_spawn": record["lock_file"], "next_start_after_clean_shutdown": clean,
    "next_start_recovered_from_the_write_ahead_log": recovered}),
  )
}

/// The latest start on `root` found the PostgreSQL `pid`, which was started on this data
/// path and ran in another folder, and ended it with exactly `signals` before it made
/// (`new_cluster`) or started a cluster here. At every later moment of the start, the
/// making of the cluster and each spawn, no PostgreSQL of this data path ran elsewhere:
/// no second server on this path's files, and nothing left that writes its control file
/// here.
#[cfg(unix)]
fn expect_ended_before_the_start(
  root: &Path,
  pid: u32,
  signals: &[&str],
  new_cluster: bool,
) -> Result<Value, String> {
  let events = start_events(root)?;
  let kinds: Vec<&str> = events
    .iter()
    .map(|event| event["event"].as_str().unwrap_or(""))
    .collect();
  let expected: &[&str] = if new_cluster {
    &["stray", "initdb", "spawn"]
  } else {
    &["stray", "spawn"]
  };
  if !kinds.starts_with(expected) || kinds[expected.len()..].iter().any(|kind| *kind != "spawn") {
    return Err(format!(
      "The start did not go: {expected:?}. It went: {kinds:?}"
    ));
  }
  let stray = &events[0];
  if stray["pid"] != json!(pid) || stray["signals"] != json!(signals) {
    return Err(format!(
      "Process {pid} was not ended with {signals:?}: {stray}"
    ));
  }
  if stray["still_running"] != json!([]) {
    return Err(format!(
      "Processes of the earlier database were left: {stray}"
    ));
  }
  if let Some(event) = events
    .iter()
    .find(|event| event["started_here_running_elsewhere"] != json!([]))
  {
    return Err(format!(
      "A PostgreSQL of this data path still ran in another folder at `{}`: {event}",
      event["event"]
    ));
  }
  Ok(json!({"order": kinds, "earlier_database": stray,
    "postgresql_of_this_path_running_elsewhere_at_initdb_and_at_every_spawn": 0}))
}

/// The process number in the first line of `postmaster.pid`.
fn lock_file_pid(root: &Path) -> Result<u32, String> {
  let path = cluster_data(root).join("postmaster.pid");
  fs::read_to_string(&path)
    .map_err(|e| format!("{}: {e}", path.display()))?
    .lines()
    .next()
    .and_then(|line| line.trim().parse().ok())
    .ok_or_else(|| format!("{} has no process number", path.display()))
}

/// Whether `pid` is a live process started from the program file `program`.
fn runs_program(pid: u32, program: &Path) -> bool {
  postgres::gate_process_image(pid)
    .ok()
    .flatten()
    .is_some_and(|image| image.canonicalize().ok() == program.canonicalize().ok())
}

/// A live process of this user that is not the database: killed when dropped.
struct Bystander(Child);
impl Bystander {
  /// `sleep 300` (Windows: `ping`, which waits about as long).
  fn unrelated() -> Result<Self, String> {
    let mut command = if cfg!(windows) {
      let mut ping = Command::new("ping");
      ping.args(["-n", "300", "127.0.0.1"]);
      ping
    } else {
      let mut sleep = Command::new("sleep");
      sleep.arg("300");
      sleep
    };
    command
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .map(Self)
      .map_err(|e| e.to_string())
  }
  /// This test program copied to `image` (so its program file is called `postgres`) and
  /// run as [`gate_idle_helper`]: a process that looks like a PostgreSQL by name, has no
  /// data folder argument and is nobody's database.
  #[cfg(unix)]
  fn named_like_postgresql(image: &Path) -> Result<Self, String> {
    fs::create_dir_all(image.parent().ok_or("no parent")?).map_err(|e| e.to_string())?;
    // Written under another name, closed, renamed; started with the "Text file busy" wait.
    let writing = image.with_file_name(".writing");
    fs::copy(
      std::env::current_exe().map_err(|e| e.to_string())?,
      &writing,
    )
    .map_err(|e| e.to_string())?;
    fs::rename(&writing, image).map_err(|e| e.to_string())?;
    let child = spawn_when_ready(
      Command::new(image)
        .args(["--ignored", "--exact", "gate_idle_helper", "--nocapture"])
        .env(IDLE_HELPER, "300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null()),
    )?;
    // Until the copy runs under its own name (the exec is done).
    let deadline = Instant::now() + Duration::from_secs(10);
    while !runs_program(child.id(), image) {
      if Instant::now() >= deadline {
        return Err("The stand-in process did not start".into());
      }
      std::thread::sleep(Duration::from_millis(20));
    }
    Ok(Self(child))
  }
  fn pid(&self) -> u32 {
    self.0.id()
  }
  fn alive(&mut self) -> bool {
    matches!(self.0.try_wait(), Ok(None))
  }
}
impl Drop for Bystander {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

const IDLE_HELPER: &str = "KOMIKA_DESKTOP_GATE_IDLE_SECONDS";

/// Not a test: the body of [`Bystander::named_like_postgresql`].
#[test]
#[ignore = "helper process of the gate"]
fn gate_idle_helper() {
  if let Ok(seconds) = std::env::var(IDLE_HELPER) {
    std::thread::sleep(Duration::from_secs(seconds.parse().unwrap()));
  }
}

impl Drop for Worker {
  fn drop(&mut self) {
    if !self.stopped || self.cleanup_pending {
      let stopped = if self.cleanup_pending {
        Err("Supervisor was hard-killed before successful recovery".into())
      } else {
        self.stop()
      };
      if let Err(error) = stopped {
        eprintln!(
          "Gate worker cleanup: {error}; owned data retained at {}",
          self.root.display()
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
        // macOS has no parent-death primitive. Use a new, bounded cleanup worker
        // with the same app lock and exact executable/data ownership proof.
        let cleanup = (|| -> Result<(), String> {
          let out =
            fs::File::create(self.root.join("cleanup-worker.log")).map_err(|e| e.to_string())?;
          let mut child = Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
            .args(["--ignored", "--exact", "desktop_gate_worker", "--nocapture"])
            .env("KOMIKA_DESKTOP_GATE_WORKER", "1")
            .env("KOMIKA_DESKTOP_GATE_CLEANUP", "1")
            .env("KOMIKA_DESKTOP_GATE_DATA", &self.root)
            .env("KOMIKA_POSTGRES_RUNTIME", &self.runtime)
            .env("KOMIKA_TEST_SERVER_BIN", &self.server)
            .stdout(out.try_clone().map_err(|e| e.to_string())?)
            .stderr(out)
            .spawn()
            .map_err(|e| e.to_string())?;
          let deadline = Instant::now() + Duration::from_secs(90);
          loop {
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
              return if status.success() {
                Ok(())
              } else {
                Err(format!("Cleanup worker failed: {status}"))
              };
            }
            if Instant::now() >= deadline {
              let _ = child.kill();
              let _ = child.wait();
              return Err("Cleanup worker exceeded 90 seconds; data retained".into());
            }
            std::thread::sleep(Duration::from_millis(100));
          }
        })();
        let evidence = match &cleanup {
          Ok(()) => json!({"status":"pass","method":"identified_fallback"}),
          Err(error) => json!({"status":"fail","method":"identified_fallback","error":error}),
        };
        if let Err(error) = atomic_json(&self.root.join("cleanup-status.json"), &evidence) {
          eprintln!("Could not write cleanup evidence: {error}");
        }
        if let Err(error) = cleanup {
          eprintln!("{error}");
        }
      }
    }
  }
}
/// The app data folder and its server folder must be private to this user after a
/// start (Unix mode 0700).
/// Windows has no such mode: `null`.
fn private_modes(root: &Path) -> Result<Value, String> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let mut modes = serde_json::Map::new();
    for (name, dir) in [
      ("app_data", root.to_path_buf()),
      ("server", root.join("server")),
    ] {
      let mode = fs::symlink_metadata(&dir)
        .map_err(|e| e.to_string())?
        .mode()
        & 0o7777;
      if mode != 0o700 {
        return Err(format!(
          "{} has mode {mode:o} after a start, not 700",
          dir.display()
        ));
      }
      modes.insert(name.into(), json!(format!("{mode:o}")));
    }
    Ok(Value::Object(modes))
  }
  #[cfg(not(unix))]
  {
    let _ = root;
    Ok(Value::Null)
  }
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
  fs::create_dir(target).map_err(|e| e.to_string())?;
  for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
    let entry = entry.map_err(|e| e.to_string())?;
    let from = entry.path();
    let to = target.join(entry.file_name());
    // Resolve validated runtime links to files for a portable fresh installation.
    if from.is_dir() {
      copy_tree(&from, &to)?;
    } else {
      fs::copy(&from, &to).map_err(|e| e.to_string())?;
    }
  }
  Ok(())
}

/// Windows: `postgres.exe` does not run with administrator rights ("Execution of
/// PostgreSQL by a user with administrative permissions is not permitted"), and the app
/// starts it with the rights the app has. A person's app has none (User Account Control
/// gives it a token with the Administrators group switched off). A GitHub runner is
/// different: everything there runs with full administrator rights. So that the gate
/// tests the app as a person runs it, the driver starts this very command line again
/// with a restricted token, made the way PostgreSQL's own `pg_ctl` and `initdb` make
/// theirs (`CreateRestrictedProcess` in its source), and takes that process's result.
/// Every worker, database and server of the gate is then a child of that process and
/// has its rights.
#[cfg(windows)]
mod rights {
  use core::ffi::c_void;
  use std::mem::{size_of, zeroed};
  use std::os::windows::io::AsRawHandle;
  use std::ptr::{null, null_mut};
  use windows_sys::Win32::Foundation::{
    CloseHandle, SetHandleInformation, GENERIC_ALL, HANDLE, HANDLE_FLAG_INHERIT,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
  };
  use windows_sys::Win32::Security::{
    AclSizeInformation, AddAccessAllowedAceEx, AddAce, AllocateAndInitializeSid,
    CheckTokenMembership, CreateRestrictedToken, FreeSid, GetAce, GetAclInformation, GetLengthSid,
    GetTokenInformation, InitializeAcl, SetTokenInformation, TokenDefaultDacl, TokenUser,
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, DISABLE_MAX_PRIVILEGE,
    OBJECT_INHERIT_ACE, PSID, SECURITY_NT_AUTHORITY, SID_AND_ATTRIBUTES, TOKEN_ALL_ACCESS,
    TOKEN_DEFAULT_DACL, TOKEN_INFORMATION_CLASS, TOKEN_USER,
  };
  use windows_sys::Win32::System::Environment::GetCommandLineW;
  use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
    WaitForSingleObject, INFINITE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
  };

  /// Set for the second process: it must not start a third.
  const DROPPED: &str = "KOMIKA_DESKTOP_GATE_RIGHTS_DROPPED";

  fn failed(call: &str) -> String {
    format!("{call}: {}", std::io::Error::last_os_error())
  }

  /// The two groups `postgres.exe` refuses (`pgwin32_is_admin`): the built-in
  /// Administrators (S-1-5-32-544) and Power Users (S-1-5-32-547).
  struct Groups([PSID; 2]);
  impl Groups {
    fn new() -> Result<Self, String> {
      let mut groups = Self([null_mut(); 2]);
      for (sid, group) in groups.0.iter_mut().zip([544u32, 547]) {
        // SAFETY: the authority is a constant, two sub-authorities are given, and `sid`
        // is a writable pointer that `Drop` frees.
        let made = unsafe {
          AllocateAndInitializeSid(&SECURITY_NT_AUTHORITY, 2, 32, group, 0, 0, 0, 0, 0, 0, sid)
        };
        if made == 0 {
          return Err(failed("AllocateAndInitializeSid"));
        }
      }
      Ok(groups)
    }
  }
  impl Drop for Groups {
    fn drop(&mut self) {
      for sid in self.0 {
        if !sid.is_null() {
          // SAFETY: made by AllocateAndInitializeSid and freed once.
          unsafe { FreeSid(sid) };
        }
      }
    }
  }

  /// Whether this process has the rights `postgres.exe` refuses, asked the way it asks.
  fn administrator() -> Result<bool, String> {
    let groups = Groups::new()?;
    for sid in groups.0 {
      let mut member = 0;
      // SAFETY: a null token means the token of this process; `member` is writable.
      if unsafe { CheckTokenMembership(null_mut(), sid, &mut member) } == 0 {
        return Err(failed("CheckTokenMembership"));
      }
      if member != 0 {
        return Ok(true);
      }
    }
    Ok(false)
  }

  /// One piece of token information, in a buffer aligned for any of its structures.
  fn token_information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u64>, String> {
    let mut size = 0u32;
    // SAFETY: a null buffer of length 0 only asks for the size.
    unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut size) };
    if size == 0 {
      return Err(failed("GetTokenInformation (size)"));
    }
    let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: the buffer is writable for at least `size` bytes.
    if unsafe { GetTokenInformation(token, class, buffer.as_mut_ptr().cast(), size, &mut size) }
      == 0
    {
      return Err(failed("GetTokenInformation"));
    }
    Ok(buffer)
  }

  /// PostgreSQL's `AddUserToTokenDacl`. What a process creates gets the default access
  /// list of its token. For an administrator that list names the Administrators group
  /// and the system, not the user. With the group switched off, the new process could
  /// not open what it created itself: its own child processes, and PostgreSQL's shared
  /// memory and signal pipes. So the user is added to the list.
  fn add_user_to_default_access(token: HANDLE) -> Result<(), String> {
    let default = token_information(token, TokenDefaultDacl)?;
    // SAFETY: the buffer holds a TOKEN_DEFAULT_DACL and is aligned for it.
    let old = unsafe { (*default.as_ptr().cast::<TOKEN_DEFAULT_DACL>()).DefaultDacl };
    if old.is_null() {
      return Ok(()); // No list: what the process creates is open to its user anyway.
    }
    let user_information = token_information(token, TokenUser)?;
    // SAFETY: the buffer holds a TOKEN_USER and is aligned for it; it outlives `user`.
    let user = unsafe { (*user_information.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut sizes = ACL_SIZE_INFORMATION {
      AceCount: 0,
      AclBytesInUse: 0,
      AclBytesFree: 0,
    };
    // SAFETY: `old` points into `default`; `sizes` is writable for its own size.
    let read = unsafe {
      GetAclInformation(
        old,
        (&mut sizes as *mut ACL_SIZE_INFORMATION).cast(),
        size_of::<ACL_SIZE_INFORMATION>() as u32,
        AclSizeInformation,
      )
    };
    if read == 0 {
      return Err(failed("GetAclInformation"));
    }
    // SAFETY: `user` is a valid SID inside `user_information`.
    let size =
      sizes.AclBytesInUse + size_of::<ACCESS_ALLOWED_ACE>() as u32 + unsafe { GetLengthSid(user) }
        - size_of::<u32>() as u32;
    let mut list = vec![0u64; (size as usize).div_ceil(8)];
    let new = list.as_mut_ptr().cast::<ACL>();
    // SAFETY: `new` is writable for `size` bytes and aligned for an ACL.
    if unsafe { InitializeAcl(new, size, ACL_REVISION) } == 0 {
      return Err(failed("InitializeAcl"));
    }
    for index in 0..sizes.AceCount {
      let mut entry: *mut c_void = null_mut();
      // SAFETY: `index` is below the count of `old`; an entry starts with its header,
      // which holds its size; `new` has room for every entry of `old` and one more.
      let copied = unsafe {
        GetAce(old, index, &mut entry) != 0
          && AddAce(
            new,
            ACL_REVISION,
            u32::MAX,
            entry,
            u32::from((*entry.cast::<ACE_HEADER>()).AceSize),
          ) != 0
      };
      if !copied {
        return Err(failed("GetAce/AddAce"));
      }
    }
    // SAFETY: `new` has room for this entry (counted above); `user` is a valid SID.
    if unsafe { AddAccessAllowedAceEx(new, ACL_REVISION, OBJECT_INHERIT_ACE, GENERIC_ALL, user) }
      == 0
    {
      return Err(failed("AddAccessAllowedAceEx"));
    }
    let replaced = TOKEN_DEFAULT_DACL { DefaultDacl: new };
    // SAFETY: the structure and the list it points to live until the call returns. The
    // length is that of the structure that is passed (one pointer), not that of the
    // list it points to.
    let set = unsafe {
      SetTokenInformation(
        token,
        TokenDefaultDacl,
        (&replaced as *const TOKEN_DEFAULT_DACL).cast(),
        size_of::<TOKEN_DEFAULT_DACL>() as u32,
      )
    };
    if set == 0 {
      return Err(failed("SetTokenInformation"));
    }
    Ok(())
  }

  /// Start the command line of this process again under `token`, with the same console
  /// or log as its output, wait for it and give back its exit code.
  fn run_again(token: HANDLE) -> Result<u32, String> {
    // A stream this process does not have is null here, or the invalid value the system
    // gives when it could not get one. Both are passed on as null: no stream.
    let standard = [
      std::io::stdin().as_raw_handle(),
      std::io::stdout().as_raw_handle(),
      std::io::stderr().as_raw_handle(),
    ]
    .map(|handle| {
      if handle == INVALID_HANDLE_VALUE {
        null_mut()
      } else {
        handle
      }
    });
    for (name, handle) in ["input", "output", "error output"]
      .into_iter()
      .zip(standard)
    {
      // A stream that cannot be made inheritable would leave the second process without
      // its output, and a gate whose output is lost is no evidence: that is a failure
      // with its reason.
      // SAFETY: a handle of this process.
      if !handle.is_null()
        && unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0
      {
        return Err(failed(&format!(
          "SetHandleInformation (the standard {name} of this process)"
        )));
      }
    }
    // SAFETY: the command line of this process is a NUL-terminated string that lives as
    // long as the process. It is copied, because CreateProcess may write into its buffer.
    let mut command: Vec<u16> = unsafe {
      let line = GetCommandLineW();
      let mut length = 0;
      while *line.add(length) != 0 {
        length += 1;
      }
      std::slice::from_raw_parts(line, length + 1).to_vec()
    };
    // SAFETY: both are plain structures for which all zeroes is a valid value.
    let (mut startup, mut process): (STARTUPINFOW, PROCESS_INFORMATION) =
      unsafe { (zeroed(), zeroed()) };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTF_USESTDHANDLES;
    startup.hStdInput = standard[0];
    startup.hStdOutput = standard[1];
    startup.hStdError = standard[2];
    // SAFETY: every pointer is valid for the call; the environment and the working
    // folder are inherited (null); handles are inherited for the three streams.
    let started = unsafe {
      CreateProcessAsUserW(
        token,
        null(),
        command.as_mut_ptr(),
        null(),
        null(),
        1,
        0,
        null(),
        null(),
        &startup,
        &mut process,
      )
    };
    if started == 0 {
      return Err(failed("CreateProcessAsUserW"));
    }
    let mut code = 0u32;
    // SAFETY: both handles were just returned and are closed exactly once.
    let read = unsafe {
      CloseHandle(process.hThread);
      WaitForSingleObject(process.hProcess, INFINITE) == WAIT_OBJECT_0
        && GetExitCodeProcess(process.hProcess, &mut code) != 0
    };
    let error = failed("waiting for the process without administrator rights");
    // SAFETY: closed exactly once.
    unsafe { CloseHandle(process.hProcess) };
    if read {
      Ok(code)
    } else {
      Err(error)
    }
  }

  fn run_again_restricted() -> Result<u32, String> {
    let groups = Groups::new()?;
    let switched_off = groups.0.map(|sid| SID_AND_ATTRIBUTES {
      Sid: sid,
      Attributes: 0,
    });
    let mut own: HANDLE = null_mut();
    // SAFETY: the pseudo handle of this process; `own` is writable and closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own) } == 0 {
      return Err(failed("OpenProcessToken"));
    }
    let mut restricted: HANDLE = null_mut();
    // SAFETY: `own` is open; the two groups live until the call returns; no privilege
    // list and no restricting SIDs are given (null with count 0).
    let made = unsafe {
      CreateRestrictedToken(
        own,
        DISABLE_MAX_PRIVILEGE,
        switched_off.len() as u32,
        switched_off.as_ptr(),
        0,
        null(),
        0,
        null(),
        &mut restricted,
      )
    };
    let error = failed("CreateRestrictedToken");
    // SAFETY: closed exactly once.
    unsafe { CloseHandle(own) };
    if made == 0 {
      return Err(error);
    }
    let result = add_user_to_default_access(restricted).and_then(|()| run_again(restricted));
    // SAFETY: closed exactly once.
    unsafe { CloseHandle(restricted) };
    result
  }

  /// `Ok(false)`: this process has no administrator rights; the gate runs in it.
  /// `Ok(true)`: it has them, and the gate must run in a second process without them
  /// ([`run_second_process`]). `Err`: the rights could not be read, or this already is
  /// the second process and it still has them.
  pub fn second_process_needed() -> Result<bool, String> {
    if !administrator()? {
      return Ok(false);
    }
    if std::env::var_os(DROPPED).is_some() {
      return Err(
        "This process still has administrator rights after it was started with a restricted \
         token. PostgreSQL would refuse to start."
          .into(),
      );
    }
    Ok(true)
  }

  /// Run the gate in a second process with a restricted token and give back its exit
  /// code. `Err`: the rights could not be dropped, or the process could not be started.
  pub fn run_second_process() -> Result<u32, String> {
    println!(
      "This process has administrator rights, and PostgreSQL does not run with them. The \
       gate runs in a second process with a restricted token; its output follows."
    );
    std::env::set_var(DROPPED, "1");
    run_again_restricted()
  }

  /// For the evidence: whether this is that second process.
  pub fn dropped() -> bool {
    std::env::var_os(DROPPED).is_some()
  }
}

#[test]
#[ignore = "requires bundled PostgreSQL 18.6, real server binary and loopback sockets"]
fn desktop_postgres_gate() {
  // Windows, with administrator rights (a CI runner): the gate runs in a second process
  // without them, and this one only passes its result on. See `rights`.
  #[cfg(windows)]
  match rights::second_process_needed() {
    Ok(false) => {}
    Ok(true) => {
      let output = PathBuf::from(
        std::env::var_os("KOMIKA_DESKTOP_GATE_JSON")
          .expect("KOMIKA_DESKTOP_GATE_JSON output file required"),
      );
      match through_second_process(&output, cfg!(windows), rights::run_second_process) {
        Ok(()) => return,
        Err(error) => panic!(
          "The desktop gate did not pass in the process without administrator rights: {error}"
        ),
      }
    }
    Err(error) => {
      // No step ran: say so in the evidence file as well.
      if let Some(output) = std::env::var_os("KOMIKA_DESKTOP_GATE_JSON") {
        let _ = atomic_json(
          Path::new(&output),
          &json!({"status":"fail","phase":"administrator_rights","steps":[],"error":error}),
        );
      }
      panic!(
        "The desktop gate could not read its administrator rights, and PostgreSQL does not \
         run with them: {error}"
      )
    }
  }
  let output = PathBuf::from(
    std::env::var_os("KOMIKA_DESKTOP_GATE_JSON")
      .expect("KOMIKA_DESKTOP_GATE_JSON output file required"),
  );
  atomic_json(
    &output,
    &json!({"status":"running","steps":[],"phase":"preflight"}),
  )
  .expect("write preflight JSON");
  let outcome = std::panic::catch_unwind(|| run_gate(&output));
  if let Err(panic) = outcome {
    let message = panic
      .downcast_ref::<String>()
      .map(String::as_str)
      .or_else(|| panic.downcast_ref::<&str>().copied())
      .unwrap_or("Gate panicked");
    let previous = fs::read(&output)
      .ok()
      .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    atomic_json(
      &output,
      &json!({"status":"fail","phase":"panic","error":message,"previous":previous}),
    )
    .expect("write panic JSON");
    std::panic::resume_unwind(panic);
  }
  if let Err(error) = outcome.unwrap() {
    let previous = fs::read(&output)
      .ok()
      .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    atomic_json(
      &output,
      &json!({"status":"fail","error":error,"previous":previous}),
    )
    .expect("write failure JSON");
    panic!("Desktop gate failed; see {}: {error}", output.display());
  }
  // What was written must be what a reader of the evidence relies on.
  if let Err(error) = evidence_complete(&output, cfg!(windows)) {
    panic!(
      "Desktop gate ended without an error, but {} is not complete evidence: {error}",
      output.display()
    );
  }
}

/// The steps of the gate, in order, as they must stand in the evidence of a passed run:
/// eight on Unix; five on Windows, where the three earlier-database steps are not run.
/// The workflow (.github/workflows/postgres-desktop-gate.yml, its last step) holds the
/// same two lists.
fn expected_steps(windows: bool) -> Vec<&'static str> {
  const ALL: [&str; 8] = [
    "new_install_and_clean_persistence",
    "stale_lock_file_of_a_reused_process_number",
    "crash_of_app_and_database_then_recovery",
    "relocated_runtime_same_data",
    "earlier_database_still_running_after_data_folder_rename",
    "earlier_database_whose_program_was_moved_or_deleted",
    "earlier_database_after_rename_and_a_start_at_the_old_name",
    "dump_restore_retention_and_failure",
  ];
  ALL
    .into_iter()
    .filter(|name| !(windows && name.starts_with("earlier_database_")))
    .collect()
}

/// Whether the evidence file `output` shows a passed gate: status `pass`, and exactly the
/// expected steps in their order, each passed. `Err`: what is missing.
fn evidence_complete(output: &Path, windows: bool) -> Result<(), String> {
  let evidence: Value = serde_json::from_slice(&fs::read(output).map_err(|e| e.to_string())?)
    .map_err(|e| format!("it is not JSON: {e}"))?;
  if evidence["status"] != "pass" {
    return Err(format!(
      "its status is {}, not \"pass\"",
      evidence["status"]
    ));
  }
  let steps = evidence["steps"]
    .as_array()
    .ok_or("it has no list of steps")?;
  let names: Vec<&str> = steps
    .iter()
    .map(|step| step["name"].as_str().unwrap_or(""))
    .collect();
  let expected = expected_steps(windows);
  if names != expected {
    return Err(format!(
      "its steps are {names:?}, expected exactly {expected:?}"
    ));
  }
  if let Some(step) = steps.iter().find(|step| step["status"] != "pass") {
    return Err(format!(
      "step {} has status {}",
      step["name"], step["status"]
    ));
  }
  Ok(())
}

/// What the first process of Windows does around the second one (`second`: start it,
/// wait, give back its exit code), and whether the gate passed in it.
///
/// Before the second process is started, the evidence file is replaced by a text that is
/// no pass: whatever an earlier run left in it must not be taken for the result of this
/// run. If that cannot be written, nothing is started. After the second process, an exit
/// code of 0 alone proves nothing (a test binary that ran no test ends with 0 too): the
/// evidence file must say that the gate passed with every step, in the second process.
fn through_second_process(
  output: &Path,
  windows: bool,
  second: impl FnOnce() -> Result<u32, String>,
) -> Result<(), String> {
  atomic_json(
    output,
    &json!({"status":"running","steps":[],"phase":"administrator_rights"}),
  )
  .map_err(|error| {
    format!(
      "the evidence file {} could not be replaced before the second process was started \
       ({error}), so what an earlier run left there could be taken for this run; nothing \
       was started",
      output.display()
    )
  })?;
  let code = match second() {
    Ok(code) => code,
    Err(error) => {
      // No step ran: say so in the evidence file as well.
      let _ = atomic_json(
        output,
        &json!({"status":"fail","phase":"administrator_rights","steps":[],"error":error}),
      );
      return Err(format!(
        "the administrator rights could not be dropped, and PostgreSQL does not run with \
         them: {error}"
      ));
    }
  };
  if code != 0 {
    return Err(format!(
      "it ended with exit code {code}. Its output is above, and its result is in {}",
      output.display()
    ));
  }
  evidence_complete(output, windows)
    .and_then(|()| second_process(output))
    .map_err(|error| {
      format!(
        "it ended with exit code 0, but {} does not show a passed gate: {error}",
        output.display()
      )
    })
}

/// Whether the evidence file `output` was written by the second process of Windows, the
/// one without administrator rights. `Err`: what it says instead.
fn second_process(output: &Path) -> Result<(), String> {
  const KEY: &str = "windows_ran_in_a_second_process_without_administrator_rights";
  let evidence: Value = serde_json::from_slice(&fs::read(output).map_err(|e| e.to_string())?)
    .map_err(|e| format!("it is not JSON: {e}"))?;
  if evidence[KEY] == true {
    Ok(())
  } else {
    Err(format!("its field {KEY} is {}, not true", evidence[KEY]))
  }
}

/// Windows: whether the gate runs in the second process, the one with a restricted
/// token (see `rights`). `null` on other systems.
fn windows_rights_dropped() -> Value {
  #[cfg(windows)]
  {
    json!(rights::dropped())
  }
  #[cfg(not(windows))]
  {
    Value::Null
  }
}

fn run_gate(output: &Path) -> Result<(), String> {
  let required = |name| {
    std::env::var_os(name)
      .map(PathBuf::from)
      .ok_or_else(|| format!("{name} required"))
  };
  let supplied_runtime = required("KOMIKA_POSTGRES_RUNTIME")?;
  let supplied_server = required("KOMIKA_TEST_SERVER_BIN")?;
  let base = required("KOMIKA_DESKTOP_GATE_ROOT")?;
  // Atomic creation is the authority for every gate path and cleanup operation.
  fs::create_dir(&base).map_err(|e| format!("Gate root must be absent; nothing cleaned: {e}"))?;
  // Windows: the workflow gives this folder with both separators (`D:\a\_temp/komiq-…`).
  // From here on it is the plain `D:\…` path with `\` alone, the form the app itself
  // works with and prints, so that no step depends on how the folder was spelled.
  #[cfg(windows)]
  let base = postgres::gate_work_path(&base)?;
  // Every step runs the database from a copy of the runtime at a path with a space and
  // non-ASCII characters, like an app installed under such a user name. (No platform
  // rule forbids it: the staged runtime is relocatable. On Windows this is the path that
  // needs the ASCII alias of the runtime folder.)
  let runtime = base.join("PostgreSQL runtime é 漢字");
  copy_tree(&supplied_runtime, &runtime)?;
  let installed = base
    .join("Applications")
    .join("Komiq Server José 漢字.app")
    .join("Contents")
    .join("MacOS");
  fs::create_dir_all(&installed).map_err(|e| e.to_string())?;
  let server = installed.join(if cfg!(windows) {
    "komika-server.exe"
  } else {
    "komika-server"
  });
  fs::copy(supplied_server, &server).map_err(|e| e.to_string())?;
  let root = base.join("app data José 漢字");
  let backup_root = base.join("backup app data José 漢字");
  #[cfg(unix)]
  let before_rename = base.join("app data before a rename José 漢字");
  let after_rename = base.join("app data after a rename José 漢字");
  let moved_app_root = base.join("app data of a moved app José 漢字");
  // The step that starts the app at the old name after a rename: the name the app uses,
  // and the two names its folders are renamed aside to.
  let old_name = base.join("app data at the old name José 漢字");
  #[cfg(unix)]
  let renamed_aside = base.join("first library renamed aside José 漢字");
  let second_aside = base.join("second library renamed aside José 漢字");
  let mut steps = Vec::new();
  let mut worker: Option<Worker> = None;
  let mut step = |name: &str, action: &mut dyn FnMut() -> Result<Value, String>| {
    let before = Instant::now();
    let result = action();
    let row = match &result {
      Ok(facts) => {
        json!({"name":name,"status":"pass","seconds":before.elapsed().as_secs_f64(),"facts":facts})
      }
      Err(error) => {
        json!({"name":name,"status":"fail","seconds":before.elapsed().as_secs_f64(),"error":error})
      }
    };
    steps.push(row);
    atomic_json(
      output,
      &json!({"target":postgres::target_slug(), "status":"running", "steps":steps}),
    )
    .unwrap();
    result.map(|_| ())
  };
  // The paths every step on the first data folder uses. Each must have a space and a
  // non-ASCII character, or the step fails.
  let first_paths = |runtime: &Path| -> Result<Value, String> {
    Ok(json!({
      "runtime": spaced_non_ascii("runtime", runtime)?,
      "data": spaced_non_ascii("data", &root)?,
      "server_program": path_fact(&server),
    }))
  };
  let mut result = (|| -> Result<(), String> {
    step("new_install_and_clean_persistence", &mut || {
      let paths = first_paths(&runtime)?;
      // This first run stops its database the way a failed process inspection makes it
      // stop (on Windows: a slow or failed process query): pg_ctl first, no forced end.
      let mut first = Worker::start_with(
        &root,
        &runtime,
        &server,
        &[("KOMIKA_DESKTOP_GATE_BLIND_STOP", "1")],
      )?;
      first.sql("CREATE TABLE desktop_gate_row(id bigint PRIMARY KEY, value text NOT NULL); INSERT INTO desktop_gate_row VALUES(1,'committed é 漢字')")?;
      if first.sql("SELECT rolsuper FROM pg_roles WHERE rolname=current_user")? != "f" {
        return Err("Server application role is a superuser".into());
      }
      // The folders the app creates are private to this user (Unix).
      let modes = private_modes(&root)?;
      first.stop()?;
      let stop_log =
        fs::read_to_string(root.join("database-and-server.log")).map_err(|e| e.to_string())?;
      if !stop_log.contains("asking it to stop with pg_ctl")
        || !stop_log.contains("Database stopped by pg_ctl")
        || stop_log.contains("forcing")
      {
        return Err(format!(
          "A database that could not be inspected was not stopped by pg_ctl alone:\n{stop_log}"
        ));
      }
      if cluster_data(&root).join("postmaster.pid").exists() {
        return Err("A clean stop left postmaster.pid".into());
      }
      let mut second = Worker::start(&root, &runtime, &server)?;
      let row = second.sql("SELECT value FROM desktop_gate_row WHERE id=1")?;
      if row != "committed é 漢字" {
        return Err("Clean restart lost committed row".into());
      }
      let log = second.database_log()?;
      if !log.contains("database system was shut down at") || log.contains("not properly shut down")
      {
        return Err(format!(
          "The start after a pg_ctl stop was not a start after a clean shutdown:\n{log}"
        ));
      }
      worker = Some(second);
      Ok(
        json!({"paths":paths,"database_ready":true,"server_healthy":true,"committed_row_preserved":true,
        "application_superuser":false,"data_folder_modes":modes,
        "uninspectable_database_stopped_by_pg_ctl":true,"next_start_after_clean_shutdown":true}),
      )
    })?;
    step("stale_lock_file_of_a_reused_process_number", &mut || {
      let paths = first_paths(&runtime)?;
      worker.as_mut().unwrap().stop()?;
      worker.take();
      let lock = cluster_data(&root).join("postmaster.pid");
      if lock.exists() {
        return Err("A clean stop left postmaster.pid".into());
      }
      // 1. The lock file of an earlier run names a process number that an unrelated
      //    program of this user has now. PostgreSQL alone would refuse this start. The app
      //    proves that the process is not a PostgreSQL and tells PostgreSQL to disregard
      //    it (never on Windows, where PostgreSQL finds that out itself).
      let mut sleeper = Bystander::unrelated()?;
      let stale = format!("{}\n{}\n", sleeper.pid(), cluster_data(&root).display());
      fs::write(&lock, &stale).map_err(|e| e.to_string())?;
      let before = lock_state(&root);
      let mut started = Worker::start(&root, &runtime, &server)?;
      // The app left the file as it was until PostgreSQL itself started.
      let told = (!cfg!(windows)).then(|| sleeper.pid());
      let at_spawn = expect_prestart(&root, &before, told)?;
      let log = started.database_log()?;
      let line = log
        .lines()
        .find(|line| line.contains(&format!("names process {}", sleeper.pid())))
        .ok_or_else(|| format!("No log line names process {}:\n{log}", sleeper.pid()))?
        .to_string();
      if !line.contains("was not stopped") || !line.contains("was not removed") {
        return Err(format!("The log line does not say what was done: {line}"));
      }
      if !sleeper.alive() {
        return Err("The unrelated process was stopped".into());
      }
      if started.sql("SELECT value FROM desktop_gate_row WHERE id=1")? != "committed é 漢字" {
        return Err("The start over a stale lock file lost the committed row".into());
      }
      let now_named = lock_file_pid(&root)?;
      if now_named == sleeper.pid() {
        return Err("The running database did not put its own number in the lock file".into());
      }
      started.stop()?;
      drop(sleeper);

      // 2. Unix: the lock file names a live process that is called `postgres` and runs in
      //    another folder. A PostgreSQL main process always runs in its data folder, so
      //    this one is proven not to be a database on this folder, by the identity of its
      //    working folder: PostgreSQL is told to disregard it and the start goes on.
      #[cfg(unix)]
      let lookalike = {
        let image = base.join("another place é").join("bin").join("postgres");
        let mut lookalike = Bystander::named_like_postgresql(&image)?;
        let stale = format!("{}\n{}\n", lookalike.pid(), cluster_data(&root).display());
        fs::write(&lock, &stale).map_err(|e| e.to_string())?;
        let before = lock_state(&root);
        let mut started = Worker::start(&root, &runtime, &server)?;
        let at_spawn = expect_prestart(&root, &before, Some(lookalike.pid()))?;
        let log = started.database_log()?;
        let line = log
          .lines()
          .find(|line| line.contains(&format!("names process {}", lookalike.pid())))
          .ok_or_else(|| format!("No log line names process {}:\n{log}", lookalike.pid()))?
          .to_string();
        if !line.contains("a PostgreSQL of another data folder")
          || !line.contains("was not stopped")
        {
          return Err(format!(
            "The log line does not say where that process runs: {line}"
          ));
        }
        if !lookalike.alive() {
          return Err("The process named in the lock file was stopped".into());
        }
        if started.sql("SELECT value FROM desktop_gate_row WHERE id=1")? != "committed é 漢字" {
          return Err(
            "The start beside a PostgreSQL of another folder lost the committed row".into(),
          );
        }
        started.stop()?;
        json!({"log_line": line, "process_left_running": true, "at_spawn": at_spawn})
      };
      #[cfg(not(unix))]
      let lookalike = json!("not run here: the app does not read the working folder of a process on Windows and never passes PG_GRANDPARENT_PID there");

      // 3. Unix: a LIVE database of this very folder with its complete lock file, which
      //    the app cannot take for its own (it was not started with `-D`). Nothing may be
      //    passed to PostgreSQL, PostgreSQL must refuse, the start page must show its
      //    words and the process, and the running database must not be touched.
      #[cfg(unix)]
      let live = {
        let mut database = GatePostmaster::start(
          &runtime.join("bin").join("postgres"),
          &cluster_data(&root),
          false,
          &base.join("gate-database-live.log"),
        )?;
        let before = lock_state(&root);
        if before["first_line"] != json!(database.pid().to_string())
          || before["lines"].as_u64().unwrap_or(0) < 7
        {
          return Err(format!(
            "The live database has no complete lock file: {before}"
          ));
        }
        let error = Worker::start_refused(&root, &runtime, &server)?;
        for needle in [
          "PostgreSQL did not start. It said: FATAL: lock file \"postmaster.pid\" already exists",
          &format!("names process {}", database.pid()),
          "runs in this data folder",
          "was not stopped",
          "was not removed",
        ] {
          if !error.contains(needle) {
            return Err(format!(
              "The refused start does not say `{needle}`: {error}"
            ));
          }
        }
        let at_spawn = expect_prestart(&root, &before, None)?;
        if lock_state(&root) != before {
          return Err("The lock file of the live database was changed".into());
        }
        if !database.alive() {
          return Err("The live database of this folder was stopped".into());
        }
        // Its owner stops it: an ordinary shutdown, so it was whole all the time.
        let status = database.stop()?;
        if !status.success() || lock.exists() {
          return Err(format!(
            "The live database did not shut down in the ordinary way: {status}"
          ));
        }
        json!({"error_shown": error, "lock_file_lines": before["lines"], "at_spawn": at_spawn,
          "pg_grandparent_pid_passed": false, "second_server_started": false,
          "live_database_left_running": true, "lock_file_unchanged": true,
          "live_database_exit_when_its_owner_stopped_it": status.to_string()})
      };
      #[cfg(not(unix))]
      let live = json!("not run here: the gate starts its own database process only on Unix");

      // 4. An empty lock file (a start that was cut off). The app does not read it as a
      //    process, passes nothing and removes nothing; PostgreSQL decides and refuses.
      //    The start page shows PostgreSQL's words, the file and what to do.
      let empty = {
        fs::write(&lock, "").map_err(|e| e.to_string())?;
        let before = lock_state(&root);
        let error = Worker::start_refused(&root, &runtime, &server)?;
        for needle in [
          "PostgreSQL did not start. It said: FATAL: lock file \"postmaster.pid\" is empty",
          "is not a usable lock file (it is empty)",
          "did not change or remove it",
          "postmaster.pid.aside",
        ] {
          if !error.contains(needle) {
            return Err(format!(
              "The refused start does not say `{needle}`: {error}"
            ));
          }
        }
        let at_spawn = expect_prestart(&root, &before, None)?;
        if lock_state(&root) != before {
          return Err("The empty lock file was changed or removed".into());
        }
        // The owner does what the page says: no PostgreSQL runs here, the file goes aside.
        let aside = lock.with_file_name("postmaster.pid.aside");
        fs::rename(&lock, &aside).map_err(|e| e.to_string())?;
        fs::remove_file(&aside).map_err(|e| e.to_string())?;
        json!({"error_shown": error, "at_spawn": at_spawn, "lock_file_unchanged": true})
      };
      // 5. Unix: a copy of the app data folder whose database files are the same files
      //    (hard links), while the database of the first folder runs. That database runs
      //    "in another folder", and the lock file of the copy is its own lock file under a
      //    second name. Telling PostgreSQL to disregard it would start a second server on
      //    the same files. Nothing may be passed, PostgreSQL must refuse, and the running
      //    database must not be touched.
      #[cfg(unix)]
      let hard_links = {
        let mut database = GatePostmaster::start(
          &runtime.join("bin").join("postgres"),
          &cluster_data(&root),
          true,
          &base.join("gate-database-hard-links.log"),
        )?;
        let twin = base.join("app data copied with hard links José 漢字");
        copy_sharing_database_files(&root, &twin, &cluster_data(&root))?;
        let before = lock_state(&twin);
        if before != lock_state(&root) || before["first_line"] != json!(database.pid().to_string())
        {
          return Err(format!(
            "The lock file of the copy is not the lock file of the live database: {before}"
          ));
        }
        use std::os::unix::fs::MetadataExt;
        let names = fs::metadata(cluster_data(&twin).join("postmaster.pid"))
          .map_err(|e| e.to_string())?
          .nlink();
        if names != 2 {
          return Err(format!(
            "The lock file of the copy has {names} names, not 2"
          ));
        }
        let error = Worker::start_refused(&twin, &runtime, &server)?;
        for needle in [
          "PostgreSQL did not start. It said: FATAL: lock file \"postmaster.pid\" already exists",
          &format!("names process {}", database.pid()),
          "a PostgreSQL of another data folder",
          "the file postmaster.pid of this data folder has 2 names (hard links)",
          "was not stopped",
          "was not removed",
        ] {
          if !error.contains(needle) {
            return Err(format!(
              "The refused start does not say `{needle}`: {error}"
            ));
          }
        }
        let at_spawn = expect_prestart(&twin, &before, None)?;
        if lock_state(&twin) != before || lock_state(&root) != before {
          return Err("The lock file of the live database was changed".into());
        }
        if !database.alive() {
          return Err("The live database of the first folder was stopped".into());
        }
        // 5b. The same copy, and its lock file is now a file of its own: an ordinary copy
        //     of the live database's lock file (one name). Every other database file is
        //     still shared. The lock file alone would look like "a PostgreSQL of another
        //     folder, safe to disregard"; the control file of the copy has two names and
        //     says otherwise. Nothing may be passed and PostgreSQL must refuse.
        let own_lock = {
          let twin_lock = cluster_data(&twin).join("postmaster.pid");
          let content = fs::read(&twin_lock).map_err(|e| e.to_string())?;
          fs::remove_file(&twin_lock).map_err(|e| e.to_string())?;
          fs::write(&twin_lock, &content).map_err(|e| e.to_string())?;
          let links = |file: &Path| -> Result<u64, String> {
            Ok(fs::metadata(file).map_err(|e| e.to_string())?.nlink())
          };
          let lock_names = links(&twin_lock)?;
          let control_names = links(&cluster_data(&twin).join("global").join("pg_control"))?;
          if lock_names != 1 || control_names != 2 || links(&lock)? != 1 {
            return Err(format!(
              "The copy's own lock file has {lock_names} names (expected 1) and its control \
               file {control_names} (expected 2)"
            ));
          }
          let before_own = lock_state(&twin);
          if before_own["first_line"] != json!(database.pid().to_string()) {
            return Err(format!(
              "The copy's own lock file does not name the live database: {before_own}"
            ));
          }
          let error = Worker::start_refused(&twin, &runtime, &server)?;
          for needle in [
            "PostgreSQL did not start. It said: FATAL: lock file \"postmaster.pid\" already exists",
            &format!("names process {}", database.pid()),
            "a PostgreSQL of another data folder",
            // The first shared file by name: a file of `global/`.
            "the file global/",
            "of this data folder has 2 names (hard links)",
            "was not stopped",
            "was not removed",
          ] {
            if !error.contains(needle) {
              return Err(format!(
                "The refused start over the copy's own lock file does not say `{needle}`: {error}"
              ));
            }
          }
          let at_spawn = expect_prestart(&twin, &before_own, None)?;
          if lock_state(&twin) != before_own || lock_state(&root) != before {
            return Err("A lock file was changed by the refused start".into());
          }
          if !database.alive() {
            return Err("The live database of the first folder was stopped".into());
          }
          json!({"lock_file_names": lock_names, "control_file_names": control_names,
            "error_shown": error, "at_spawn": at_spawn, "pg_grandparent_pid_passed": false,
            "second_server_started": false, "live_database_left_running": true})
        };
        // 5c. The same copy once more, and its control file is now a file of its own as
        //     well (round 7, F1). The lock file and the control file each have one name;
        //     the relation files and the write-ahead log are still the files of the live
        //     database. Round 6 looked at those two files only and told PostgreSQL to
        //     disregard the live database. The other files of `global/` and of `pg_wal/`
        //     still have two names: nothing may be passed and PostgreSQL must refuse.
        let own_control = {
          let twin_data = cluster_data(&twin);
          let twin_control = twin_data.join("global").join("pg_control");
          let content = fs::read(&twin_control).map_err(|e| e.to_string())?;
          fs::remove_file(&twin_control).map_err(|e| e.to_string())?;
          fs::write(&twin_control, &content).map_err(|e| e.to_string())?;
          let links = |file: &Path| -> Result<u64, String> {
            Ok(fs::metadata(file).map_err(|e| e.to_string())?.nlink())
          };
          let lock_names = links(&twin_data.join("postmaster.pid"))?;
          let control_names = links(&twin_control)?;
          let shared = |folder: &str| -> Result<usize, String> {
            let mut count = 0;
            for entry in fs::read_dir(twin_data.join(folder)).map_err(|e| e.to_string())? {
              let meta = entry
                .map_err(|e| e.to_string())?
                .metadata()
                .map_err(|e| e.to_string())?;
              if meta.is_file() && meta.nlink() > 1 {
                count += 1;
              }
            }
            Ok(count)
          };
          let (shared_global, shared_wal) = (shared("global")?, shared("pg_wal")?);
          if lock_names != 1 || control_names != 1 || shared_global == 0 || shared_wal == 0 {
            return Err(format!(
              "The copy is not the case to test: its lock file has {lock_names} names and its \
               control file {control_names} (expected 1 each), and {shared_global} files of \
               global and {shared_wal} of pg_wal are shared (expected some)"
            ));
          }
          let before_copy = lock_state(&twin);
          let error = Worker::start_refused(&twin, &runtime, &server)?;
          for needle in [
            "PostgreSQL did not start. It said: FATAL: lock file \"postmaster.pid\" already exists",
            &format!("names process {}", database.pid()),
            "a PostgreSQL of another data folder",
            "the file global/",
            "of this data folder has 2 names (hard links)",
            "was not stopped",
            "was not removed",
          ] {
            if !error.contains(needle) {
              return Err(format!(
                "The refused start over ordinary copies of the lock and control file does \
                 not say `{needle}`: {error}"
              ));
            }
          }
          let at_spawn = expect_prestart(&twin, &before_copy, None)?;
          if lock_state(&twin) != before_copy || lock_state(&root) != before {
            return Err("A lock file was changed by the refused start".into());
          }
          if !database.alive() {
            return Err("The live database of the first folder was stopped".into());
          }
          json!({"lock_file_names": lock_names, "control_file_names": control_names,
            "shared_files_in_global": shared_global, "shared_files_in_pg_wal": shared_wal,
            "error_shown": error, "at_spawn": at_spawn, "pg_grandparent_pid_passed": false,
            "second_server_started": false, "live_database_left_running": true})
        };
        let status = database.stop()?;
        if !status.success() || lock.exists() {
          return Err(format!(
            "The live database did not shut down in the ordinary way: {status}"
          ));
        }
        json!({"data_of_the_copy": spaced_non_ascii("data", &twin)?, "lock_file_names": names,
          "error_shown": error, "at_spawn": at_spawn, "pg_grandparent_pid_passed": false,
          "second_server_started": false, "live_database_left_running": true,
          "lock_file_unchanged": true,
          "lock_file_of_its_own_and_shared_control_file_refused": own_lock,
          "ordinary_copies_of_lock_and_control_file_refused": own_control,
          "live_database_exit_when_its_owner_stopped_it": status.to_string()})
      };
      #[cfg(not(unix))]
      let hard_links = json!("not run here: the app never passes PG_GRANDPARENT_PID on Windows, so PostgreSQL always decides alone");

      let mut after = Worker::start(&root, &runtime, &server)?;
      if after.sql("SELECT value FROM desktop_gate_row WHERE id=1")? != "committed é 漢字" {
        return Err("The start after the refused ones lost the committed row".into());
      }
      worker = Some(after);
      Ok(
        json!({"paths":paths,"lock_file":path_fact(&lock),"log_line":line,"unrelated_process_left_running":true,
        "at_spawn":at_spawn,"normal_start":true,"committed_row_preserved":true,
        "lock_file_replaced_by_postgresql":true,
        "postgresql_of_another_folder_disregarded":lookalike,
        "live_database_of_this_folder_refused":live,
        "empty_lock_file_refused":empty,
        "copy_with_hard_linked_database_files_refused":hard_links}),
      )
    })?;
    step("crash_of_app_and_database_then_recovery", &mut || {
      let paths = first_paths(&runtime)?;
      let running = worker.as_mut().unwrap();
      let program = PathBuf::from(running.request(json!({"action":"postgres_program"}))?);
      // A row written just before the crash: it is in the write-ahead log only.
      running
        .sql("INSERT INTO desktop_gate_row VALUES(2,'written just before the crash é 漢字')")?;
      let (postmaster, old) = running.database_pids()?;
      if !runs_program(postmaster, &program) {
        return Err(format!(
          "Process {postmaster} of the lock file is not {}",
          program.display()
        ));
      }
      running.crash(postmaster)?;
      let mut crashed = worker.take().unwrap();
      // The database's other processes end by themselves once its main process is gone.
      let deadline = Instant::now() + Duration::from_secs(30);
      while old.iter().any(|pid| runs_program(*pid, &program)) {
        if Instant::now() >= deadline {
          return Err("Processes of the killed database did not end".into());
        }
        std::thread::sleep(Duration::from_millis(100));
      }
      // The lock file of the killed database is still there and names the dead process.
      if lock_file_pid(&root)? != postmaster {
        return Err("The crash did not leave the lock file of the killed database".into());
      }
      // The app must leave that file to PostgreSQL: at the moment the new database is
      // spawned it is still the same file with the same content, and nothing is passed.
      let left_over = lock_state(&root);
      let mut recovered = Worker::start(&root, &runtime, &server)?;
      crashed.cleanup_pending = false;
      let at_spawn = expect_prestart(&root, &left_over, None)?;
      for (id, value) in [
        (1, "committed é 漢字"),
        (2, "written just before the crash é 漢字"),
      ] {
        if recovered.sql(&format!("SELECT value FROM desktop_gate_row WHERE id={id}"))? != value {
          return Err(format!("Crash recovery lost row {id}"));
        }
      }
      let log = recovered.database_log()?;
      let recovery: Vec<&str> = log
        .lines()
        .filter(|line| {
          line.contains("was not properly shut down")
            || line.contains("automatic recovery in progress")
            || line.contains("redo ")
        })
        .collect();
      if !log.contains("database system was not properly shut down")
        || !log.contains("automatic recovery in progress")
      {
        return Err(format!(
          "The Database log does not show crash recovery:\n{log}"
        ));
      }
      // No process of the killed database is left: every old number is free, or belongs
      // to the new database (a number given out again), or is another program.
      let (new_postmaster, new) = recovered.database_pids()?;
      let orphans: Vec<u32> = old
        .iter()
        .copied()
        .filter(|pid| !new.contains(pid) && runs_program(*pid, &program))
        .collect();
      if !orphans.is_empty() || new_postmaster == postmaster {
        return Err(format!(
          "Processes of the killed database are still running: {orphans:?}"
        ));
      }
      let recovery = json!(recovery);
      worker = Some(recovered);
      Ok(
        json!({"paths":paths,"supervisor_killed":true,"database_main_process_killed":cfg!(unix),
        "killed_database_process":postmaster,"killed_database_processes":old.len(),
        "stale_lock_file_named_the_killed_process":true,
        "lock_file_untouched_by_the_app_until_postgresql_started":true,"at_spawn":at_spawn,
        "row_written_before_crash_present":true,
        "committed_row_preserved":true,"recovery_log_lines":recovery,"orphan_database_processes":0}),
      )
    })?;
    // Not a reinstall: no installer runs in this gate. The runtime tree is copied to a
    // second place and the same data is opened from there.
    step("relocated_runtime_same_data", &mut || {
      use sha2::{Digest, Sha256};
      worker.as_mut().unwrap().stop()?;
      worker.take();
      let credential_file = postgres::cluster_dir(&root.join("server")).join("credentials");
      let before = Sha256::digest(fs::read(&credential_file).map_err(|e| e.to_string())?);
      let moved = base.join("moved runtime é 漢字");
      copy_tree(&runtime, &moved)?;
      let paths = first_paths(&moved)?;
      let mut relocated = Worker::start(&root, &moved, &server)?;
      if relocated.sql("SELECT count(*) FROM desktop_gate_row")? != "2" {
        return Err("The relocated runtime lost data".into());
      }
      if Sha256::digest(fs::read(credential_file).map_err(|e| e.to_string())?) != before {
        return Err("The relocated runtime replaced the credentials".into());
      }
      let program = PathBuf::from(relocated.request(json!({"action":"postgres_program"}))?);
      let resolved = |path: &Path| path.canonicalize().map_err(|e| e.to_string());
      if !resolved(&program)?.starts_with(resolved(&moved)?) {
        return Err(format!(
          "The database does not run from the relocated runtime: {}",
          program.display()
        ));
      }
      relocated.stop()?;
      Ok(
        json!({"paths":paths,"previous_runtime":path_fact(&runtime),"database_program":program,
        "same_credentials":true,"same_data":true,"installer_run":false}),
      )
    })?;
    // The two steps below need a database that runs without its app. Unix only: on
    // Windows the Job Object ends the database with the app, and the app does not read
    // the working folder of a process there (it never passes PG_GRANDPARENT_PID).
    #[cfg(unix)]
    step(
      "earlier_database_still_running_after_data_folder_rename",
      &mut || {
        let paths = json!({
          "runtime": spaced_non_ascii("runtime", &runtime)?,
          "data_before_the_rename": spaced_non_ascii("data", &before_rename)?,
          "data_after_the_rename": spaced_non_ascii("data", &after_rename)?,
        });
        let mut first = Worker::start(&before_rename, &runtime, &server)?;
        first.sql("CREATE TABLE desktop_gate_row(id bigint PRIMARY KEY, value text NOT NULL); INSERT INTO desktop_gate_row VALUES(1,'before the rename é 漢字')")?;
        let program = PathBuf::from(first.request(json!({"action":"postgres_program"}))?);
        // The app is force-quit and its database keeps running on the folder.
        let mut left = LeftRunning::after_force_quit(
          &mut first,
          &program,
          &base.join("gate-database-rename.log"),
        )?;
        let lock_before = lock_state(&before_rename);
        if lock_before["first_line"] != json!(left.pid.to_string())
          || lock_before["lines"].as_u64().unwrap_or(0) < 7
        {
          return Err(format!(
            "The database left running has no complete lock file: {lock_before}"
          ));
        }
        // The owner renames the app data folder, and something makes an empty folder at the
        // old name. The arguments of the running database and the second line of its lock
        // file now name that empty folder: by their text it is "another data folder".
        fs::rename(&before_rename, &after_rename).map_err(|e| e.to_string())?;
        first.root = after_rename.clone();
        let decoy = cluster_data(&before_rename);
        fs::create_dir_all(&decoy).map_err(|e| e.to_string())?;
        if !left.running() || lock_state(&after_rename) != lock_before {
          return Err("The database left running did not survive the rename untouched".into());
        }
        // A row committed AFTER the rename, through the database that still runs.
        sql_without_app(
          &runtime,
          &after_rename,
          "INSERT INTO desktop_gate_row VALUES(2,'after the rename é 漢字')",
        )?;
        let mut second = Worker::start(&after_rename, &runtime, &server)?;
        first.cleanup_pending = false;
        // By the identity of its working folder it is this folder's database: named so,
        // nothing passed to PostgreSQL, stopped with pg_ctl, then a start that recovers
        // what was committed (a database whose folder was renamed cannot shut down whole).
        let adopted = expect_adopted(&mut second, &mut left, false, true)?;
        if second.sql("SELECT string_agg(value, '|' ORDER BY id) FROM desktop_gate_row")?
          != "before the rename é 漢字|after the rename é 漢字"
        {
          return Err("The start after the rename lost a committed row".into());
        }
        if fs::read_dir(&decoy).map_err(|e| e.to_string())?.count() != 0 {
          return Err("The empty folder at the old name was written to".into());
        }
        second.stop()?;
        Ok(json!({"paths":paths,"lock_file_lines":lock_before["lines"],
        "earlier_database_alive_and_lock_file_unchanged_before_the_start":true,
        "seen_as_this_folders_database":adopted,
        "rows_committed_before_and_after_the_rename_preserved":true,
        "empty_folder_at_the_old_name_untouched":true}))
      },
    )?;
    #[cfg(unix)]
    step(
      "earlier_database_whose_program_was_moved_or_deleted",
      &mut || {
        let paths = json!({
          "runtime_of_this_start": spaced_non_ascii("runtime", &runtime)?,
          "data": spaced_non_ascii("data", &moved_app_root)?,
        });
        // An install whose runtime is at one place starts the database and is force-quit.
        let install = base.join("first install é 漢字");
        fs::create_dir(&install).map_err(|e| e.to_string())?;
        copy_tree(&runtime, &install.join("PostgreSQL runtime"))?;
        let mut first = Worker::start(
          &moved_app_root,
          &install.join("PostgreSQL runtime"),
          &server,
        )?;
        first.sql("CREATE TABLE desktop_gate_row(id bigint PRIMARY KEY, value text NOT NULL); INSERT INTO desktop_gate_row VALUES(1,'before the app was moved é 漢字')")?;
        let program = PathBuf::from(first.request(json!({"action":"postgres_program"}))?);
        let mut left = LeftRunning::after_force_quit(
          &mut first,
          &program,
          &base.join("gate-database-moved.log"),
        )?;
        // 1. The app is moved while its database runs; the app at another place starts.
        let moved = base.join("moved away é 漢字");
        fs::rename(&install, &moved).map_err(|e| e.to_string())?;
        first.runtime = runtime.clone();
        if !left.running() || program.exists() {
          return Err(
            "The database of the moved app is not running without its program path".into(),
          );
        }
        let mut second = Worker::start(&moved_app_root, &runtime, &server)?;
        first.cleanup_pending = false;
        let after_move = expect_adopted(&mut second, &mut left, true, false)?;
        if second.sql("SELECT value FROM desktop_gate_row WHERE id=1")?
          != "before the app was moved é 漢字"
        {
          return Err("The start after the move lost the committed row".into());
        }
        second.stop()?;
        // 2. The same with the program deleted while its database runs.
        let mut third = Worker::start(&moved_app_root, &moved.join("PostgreSQL runtime"), &server)?;
        third.sql("INSERT INTO desktop_gate_row VALUES(2,'before the app was deleted é 漢字')")?;
        let program = PathBuf::from(third.request(json!({"action":"postgres_program"}))?);
        let mut left = LeftRunning::after_force_quit(
          &mut third,
          &program,
          &base.join("gate-database-deleted.log"),
        )?;
        fs::remove_dir_all(&moved).map_err(|e| e.to_string())?;
        third.runtime = runtime.clone();
        if !left.running() || program.exists() {
          return Err(
            "The database of the deleted app is not running without its program file".into(),
          );
        }
        let mut fourth = Worker::start(&moved_app_root, &runtime, &server)?;
        third.cleanup_pending = false;
        let after_delete = expect_adopted(&mut fourth, &mut left, true, false)?;
        if fourth.sql("SELECT string_agg(value, '|' ORDER BY id) FROM desktop_gate_row")?
          != "before the app was moved é 漢字|before the app was deleted é 漢字"
        {
          return Err("The start after the deletion lost a committed row".into());
        }
        fourth.stop()?;
        Ok(
          json!({"paths":paths,"program_folder_moved_away":after_move,"program_deleted":after_delete,
        "stopped_with_pg_ctl_of_the_current_runtime":true,"committed_rows_preserved":true}),
        )
      },
    )?;
    // Round 5, M1. The app is force-quit, the owner renames the app data folder aside,
    // and the app is started again at the old name. The database of the renamed folder
    // is still alive there, and PostgreSQL writes its control file by the path it was
    // started with: into whatever database lies at the old name. Both directions are
    // run: a new library is made at the old name while the first one's database runs,
    // and later the first library is put back at the old name while the new one's
    // database runs. Unix only: see the comment above, and Windows does not let a folder
    // be renamed while a process runs in it.
    #[cfg(unix)]
    step(
      "earlier_database_after_rename_and_a_start_at_the_old_name",
      &mut || {
        let paths = json!({
          "runtime": spaced_non_ascii("runtime", &runtime)?,
          "data_at_the_name_the_app_uses": spaced_non_ascii("data", &old_name)?,
          "first_library_renamed_aside": spaced_non_ascii("data", &renamed_aside)?,
          "second_library_renamed_aside": spaced_non_ascii("data", &second_aside)?,
        });
        let rows = |worker: &mut Worker, table: &str| {
          worker.sql(&format!(
            "SELECT string_agg(value, '|' ORDER BY id) FROM {table}"
          ))
        };
        let running_system = |worker: &mut Worker| -> Result<u64, String> {
          worker
            .sql("SELECT system_identifier FROM pg_control_system()")?
            .parse()
            .map_err(|e| format!("system identifier: {e}"))
        };
        let recovered = |log: &str| {
          log.contains("database system was not properly shut down")
            && log.contains("automatic recovery in progress")
        };
        let clean = |log: &str| {
          log.contains("database system was shut down at")
            && !recovered(log)
            && !log.contains("PANIC")
            && !log.contains("different database system")
        };

        // The first library, and its app force-quit.
        let mut first = Worker::start(&old_name, &runtime, &server)?;
        first.sql("CREATE TABLE desktop_gate_row(id bigint PRIMARY KEY, value text NOT NULL); INSERT INTO desktop_gate_row VALUES(1,'first library, before the rename é 漢字')")?;
        let program = PathBuf::from(first.request(json!({"action":"postgres_program"}))?);
        let first_system = system_identifier(&cluster_data(&old_name))?;
        let mut left = LeftRunning::after_force_quit(
          &mut first,
          &program,
          &base.join("gate-database-old-name-1.log"),
        )?;
        // The owner renames the folder aside. Its database runs on, and takes a row.
        fs::rename(&old_name, &renamed_aside).map_err(|e| e.to_string())?;
        first.root = renamed_aside.clone();
        sql_without_app(
          &runtime,
          &renamed_aside,
          "INSERT INTO desktop_gate_row VALUES(2,'first library, after the rename é 漢字')",
        )?;
        if !left.running() || cluster_data(&old_name).exists() {
          return Err(
            "The database of the renamed folder is not running, or the old name is taken".into(),
          );
        }

        // 1. The app starts at the old name: a new install. The earlier database must be
        //    ended before the new cluster is made, so that it cannot write into it.
        let mut second = Worker::start(&old_name, &runtime, &server)?;
        // Nothing lies at the path yet, so it is asked to shut down (the interrupt).
        let new_install = expect_ended_before_the_start(&old_name, left.pid, &["interrupt"], true)?;
        let exit = left.expect_stopped(false)?;
        let log = second.database_log()?;
        for needle in [
          format!("Process {} (", left.pid),
          "is a PostgreSQL that was started on this data path".into(),
          format!(
            "now runs in another folder ({})",
            cluster_data(&renamed_aside).display()
          ),
          "has ended (signals sent: interrupt)".into(),
        ] {
          if !log.contains(&needle) {
            return Err(format!("The Database log does not say `{needle}`:\n{log}"));
          }
        }
        let at_spawn = prestart(&old_name)?;
        if at_spawn["lock_file"]["exists"] != false || !at_spawn["grandparent_pid"].is_null() {
          return Err(format!(
            "The new cluster was not started over a free data folder with nothing to disregard: {at_spawn}"
          ));
        }
        let second_system = system_identifier(&cluster_data(&old_name))?;
        if second_system == first_system || running_system(&mut second)? != second_system {
          return Err("The start at the old name does not run a new cluster of its own".into());
        }
        if second.sql("SELECT to_regclass('desktop_gate_row') IS NULL")? != "t" {
          return Err("The new cluster has a table of the first library".into());
        }
        second.sql("CREATE TABLE gate_new_row(id bigint PRIMARY KEY, value text NOT NULL); INSERT INTO gate_new_row VALUES(1,'second library é 漢字')")?;
        second.stop()?;

        // 2. The renamed folder is opened later: every row committed before and after the
        //    rename is there, recovered from the write-ahead log.
        let before = lock_state(&renamed_aside);
        let mut third = Worker::start(&renamed_aside, &runtime, &server)?;
        first.cleanup_pending = false;
        expect_prestart(&renamed_aside, &before, None)?;
        let log = third.database_log()?;
        if !recovered(&log) {
          return Err(format!(
            "The renamed folder did not start by recovery:\n{log}"
          ));
        }
        if rows(&mut third, "desktop_gate_row")?
          != "first library, before the rename é 漢字|first library, after the rename é 漢字"
          || running_system(&mut third)? != first_system
        {
          return Err("The renamed folder lost a committed row".into());
        }
        third.stop()?;

        // 3. The new cluster at the old name starts again, in the ordinary way: its
        //    control file is its own. (When the earlier database had written its control
        //    file here, this start failed: `WAL file is from different database system`.)
        let mut fourth = Worker::start(&old_name, &runtime, &server)?;
        let log = fourth.database_log()?;
        if !clean(&log) {
          return Err(format!(
            "The new cluster did not start in the ordinary way the second time:\n{log}"
          ));
        }
        if rows(&mut fourth, "gate_new_row")? != "second library é 漢字"
          || running_system(&mut fourth)? != second_system
        {
          return Err("The new cluster lost its row".into());
        }

        // 4. The other direction. The app is force-quit again; the owner renames the new
        //    library aside and puts the first library back at the old name. Now a database
        //    LIES at the old name while the database of the renamed folder runs: an
        //    ordinary shutdown of that one would write its control file over the first
        //    library's. It must be told to quit, never asked to shut down.
        let program = PathBuf::from(fourth.request(json!({"action":"postgres_program"}))?);
        let mut left = LeftRunning::after_force_quit(
          &mut fourth,
          &program,
          &base.join("gate-database-old-name-2.log"),
        )?;
        fs::rename(&old_name, &second_aside).map_err(|e| e.to_string())?;
        fourth.root = second_aside.clone();
        fs::rename(&renamed_aside, &old_name).map_err(|e| e.to_string())?;
        sql_without_app(
          &runtime,
          &second_aside,
          "INSERT INTO gate_new_row VALUES(2,'second library, after its rename é 漢字')",
        )?;
        let control_before = postgres::gate_control_file(&cluster_data(&old_name))
          .ok_or("The first library has no control file")?;
        if !left.running() || system_identifier(&cluster_data(&old_name))? != first_system {
          return Err(
            "The first library is not back at the old name beside a running database".into(),
          );
        }
        let mut fifth = Worker::start(&old_name, &runtime, &server)?;
        let put_back = expect_ended_before_the_start(&old_name, left.pid, &["quit"], false)?;
        let exit_2 = left.expect_stopped(false)?;
        let at_spawn_2 = prestart(&old_name)?;
        if at_spawn_2["control_file_sha256"] != json!(control_before) {
          return Err(format!(
            "The control file of the first library was written before its own database started. Before: {control_before}. At the spawn: {}",
            at_spawn_2["control_file_sha256"]
          ));
        }
        let log = fifth.database_log()?;
        if !log.contains("it is told to quit at once (the quit signal) and not asked to shut down")
          || !log.contains("has ended (signals sent: quit)")
        {
          return Err(format!(
            "The Database log does not say that the earlier database was told to quit:\n{log}"
          ));
        }
        if !clean(&log) {
          return Err(format!(
            "The first library did not start in the ordinary way at the old name:\n{log}"
          ));
        }
        if rows(&mut fifth, "desktop_gate_row")?
          != "first library, before the rename é 漢字|first library, after the rename é 漢字"
          || running_system(&mut fifth)? != first_system
        {
          return Err("The first library lost a committed row at the old name".into());
        }
        fifth.stop()?;

        // 5. The second library, renamed aside, opens later with its rows from before and
        //    after its rename.
        let before = lock_state(&second_aside);
        let mut sixth = Worker::start(&second_aside, &runtime, &server)?;
        fourth.cleanup_pending = false;
        expect_prestart(&second_aside, &before, None)?;
        let log = sixth.database_log()?;
        if !recovered(&log) {
          return Err(format!(
            "The second renamed folder did not start by recovery:\n{log}"
          ));
        }
        if rows(&mut sixth, "gate_new_row")?
          != "second library é 漢字|second library, after its rename é 漢字"
          || running_system(&mut sixth)? != second_system
        {
          return Err("The second renamed folder lost a committed row".into());
        }
        sixth.stop()?;
        Ok(json!({"paths":paths,
          "new_install_at_the_old_name":{"start":new_install,"earlier_process_exit":exit,
            "left_running_by":left.left_by(),"lock_file_at_spawn":at_spawn["lock_file"],
            "pg_grandparent_pid_passed":false,"new_cluster_of_its_own":true,
            "second_start_of_the_new_cluster_in_the_ordinary_way":true},
          "first_library_opened_later":{"started_by_recovery":true,
            "rows_committed_before_and_after_the_rename_preserved":true},
          "first_library_put_back_at_the_old_name":{"start":put_back,"earlier_process_exit":exit_2,
            "control_file_unchanged_until_its_own_database_started":true,
            "control_file_sha256":control_before,"started_in_the_ordinary_way":true,
            "committed_rows_preserved":true},
          "second_library_opened_later":{"started_by_recovery":true,
            "rows_committed_before_and_after_the_rename_preserved":true},
          "second_server_on_shared_files":false}))
      },
    )?;
    step("dump_restore_retention_and_failure", &mut || {
      let mut seeded = Worker::start(&backup_root, &runtime, &server)?;
      seeded.sql(
        "INSERT INTO users(id, username, email, password_hash, created_at) VALUES \
        ('00000000-0000-4000-8000-000000000900', 'owner', 'owner@example.invalid', \
        'fixture-secret', '2020-01-01T00:00:00Z')",
      )?;
      seeded.sql(
        "INSERT INTO instance_setting(key, value, updated_at) VALUES \
        ('instance_name', 'Gate fixture é 漢字', '2021-02-03T04:05:06.789Z') \
        ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
      )?;
      let counts = seeded.counts(None)?;
      let fixture_settings = seeded.settings(None)?;
      worker = Some(seeded);
      let backups = backup_root.join("server").join("backups");
      // The app's own dump task makes a dump by itself when the install has none. The
      // gate has not asked for a dump on this install so far. The dumps that the task
      // made in the earlier launches are set aside, and the app is started once more:
      // this launch must make its dump within the wait, and the gate sends nothing while
      // it waits (a request would end and restart the task).
      worker.as_mut().unwrap().stop()?;
      worker.take();
      let earlier = postgres::dumps(&backups).unwrap_or_default();
      let set_aside = backup_root.join("dumps of earlier launches");
      fs::create_dir(&set_aside).map_err(|e| e.to_string())?;
      for dump in &earlier {
        fs::rename(dump, set_aside.join(dump.file_name().ok_or("dump name")?))
          .map_err(|e| e.to_string())?;
      }
      worker = Some(Worker::start(&backup_root, &runtime, &server)?);
      let running = worker.as_mut().unwrap();
      let waited = Instant::now();
      let scheduled = loop {
        if let Some(dump) = postgres::dumps(&backups)
          .ok()
          .and_then(|dumps| dumps.into_iter().next())
        {
          break dump;
        }
        if waited.elapsed() > Duration::from_secs(180) {
          return Err("The dump task of the running app made no dump within 180 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(200));
      };
      let scheduled_waited = waited.elapsed().as_secs_f64();
      // The log names the file by the path the app works with. That is the same file,
      // and on Windows it need not be the same text (separators, the form of the drive
      // path): the paths are compared as the file system resolves them, not as text.
      let scheduled_file = scheduled.canonicalize().map_err(|e| e.to_string())?;
      let log = running.database_log()?;
      let reported = log
        .lines()
        .filter_map(|line| line.split_once("Nightly dump saved: "))
        .any(|(_, path)| {
          Path::new(path.trim()).canonicalize().ok().as_ref() == Some(&scheduled_file)
        });
      if !reported {
        return Err(format!(
          "The Database log does not report the scheduled dump {}:\n{log}",
          scheduled.display()
        ));
      }
      if !running
        .request(json!({"action":"inspect_dump","value":scheduled}))?
        .contains("user_library")
      {
        return Err("The scheduled dump does not list an application table".into());
      }
      for i in 0..9 {
        let path = backups.join(format!("komika-20000101T00000{i}Z.dump"));
        fs::write(&path, "older fake successful dump").map_err(|e| e.to_string())?;
      }
      // The content of the live database right before and right after the dump is made
      // (see `dump_differences`).
      let hashes_before_dump = running.hashes(None)?;
      let dump = PathBuf::from(running.request(json!({"action":"dump"}))?);
      let live_hashes = running.hashes(None)?;
      let changed_by_the_server = hash_differences(&hashes_before_dump, &live_hashes);
      let paths = json!({
        "runtime": spaced_non_ascii("runtime", &runtime)?,
        "data": spaced_non_ascii("data", &backup_root)?,
        "dump": spaced_non_ascii("dump", &dump)?,
      });
      let listing = running.request(json!({"action":"inspect_dump","value":dump}))?;
      if !listing.contains("user_library") {
        return Err("pg_restore --list did not read an application table".into());
      }
      // A real restore: the dump into a new empty database of the same cluster, then the
      // same count of every table there, in the live database, and in the fixture.
      let restored_into = running.request(json!({"action":"restore_dump","value":dump}))?;
      let restored = running.counts(Some(&restored_into))?;
      let live = running.counts(None)?;
      let mut wrong = differences(&counts, &restored);
      wrong.extend(differences(&counts, &live));
      if !wrong.is_empty() {
        return Err(format!(
          "The restored dump or the live database differs from the fixture: {wrong:?}"
        ));
      }
      if let Some(missing) = counts.keys().find(|table| !restored.contains_key(*table)) {
        return Err(format!("The restored database has no table {missing}"));
      }
      // `instance_setting` by content, in both (its row count is not the fixture's).
      let settings_restored = settings_check(
        "The restored dump",
        &fixture_settings,
        &running.settings(Some(&restored_into))?,
      )?;
      let settings_live = settings_check(
        "The live database",
        &fixture_settings,
        &running.settings(None)?,
      )?;
      // The content, not only the number of rows: one hash of every row of each table,
      // the same in the restored database as in the live one when the dump was made, for
      // every compared table.
      let restored_hashes = running.hashes(Some(&restored_into))?;
      let unequal = dump_differences(&hashes_before_dump, &live_hashes, &restored_hashes);
      if !unequal.is_empty() {
        return Err(format!(
          "The content of the restored dump differs from the live database, as it was \
           right before and right after the dump was made, in: {unequal:?} (tables the \
           server changed in that time: {changed_by_the_server:?})"
        ));
      }
      if let Some(missing) = counts
        .keys()
        .find(|table| !live_hashes.contains_key(*table) || !restored_hashes.contains_key(*table))
      {
        return Err(format!("Table {missing} was not hashed"));
      }
      // The hash does see a changed row: one value changed in the restored copy, the
      // number of rows the same.
      running.request(json!({"action":"sql_in","value":[restored_into,
        "UPDATE users SET username = username || ' changed' WHERE email = 'owner@example.invalid'"]}))?;
      let altered = running.hashes(Some(&restored_into))?;
      if dump_differences(&hashes_before_dump, &live_hashes, &altered) != ["users"]
        || running.counts(Some(&restored_into))? != restored
      {
        return Err("The content hash did not single out a changed row".into());
      }
      let before = postgres::dumps(&backups)?;
      if !before.contains(&scheduled) {
        return Err("Retention removed the scheduled dump of today".into());
      }
      if before.len() != 7 || !before.contains(&dump) {
        return Err(format!(
          "Expected seven dumps with the new one after success; got {}",
          before.len()
        ));
      }
      let failure = running.request_raw(json!({"action":"dump_failure"}))?;
      if failure["status"] != "fail" || postgres::dumps(&backups)? != before {
        return Err("Failed dump removed earlier backups or unexpectedly succeeded".into());
      }
      running.stop()?;
      worker.take();
      Ok(
        json!({"paths":paths,"pg_restore_list_readable":true,"restored_into_database":restored_into,
        "restored_tables_counted":restored.len(),
        "instance_setting_compared_by_content":{"restored":settings_restored,"live":settings_live},
        "compared_tables_equal_in_fixture_live_and_restored":counts.len() - NOT_COMPARED.len(),
        "compared_tables_with_equal_content_hash_in_live_and_restored":counts.len() - NOT_COMPARED.len(),
        "tables_hashed_in_live_and_restored":live_hashes.len(),
        "content_hash":"md5 of every row as text, rows ordered, per table",
        "live_content_taken":"right before and right after the dump was made; a table the server changed in between must equal one of the two in the dump",
        "tables_the_server_changed_while_the_dump_was_made":changed_by_the_server,
        "content_hash_sees_one_changed_row":true,
        "scheduled_dump":{"path":path_fact(&scheduled),"made_by":"the app's dump task, not a gate request",
          "dumps_of_earlier_launches_set_aside":earlier.len(),
          "seconds_waited":scheduled_waited,"reported_in_database_log":true,"pg_restore_list_readable":true,
          "kept_by_retention":true},
        "restored_counts":restored,"restored_content_hashes":restored_hashes,
        "retained":7,"failure_kept_every_backup":true}),
      )
    })?;
    Ok(())
  })();
  drop(worker); // every normal error path requests a complete clean stop.
  let mut cleanup = Vec::new();
  for data in [
    &root,
    &backup_root,
    &after_rename,
    &moved_app_root,
    &old_name,
    &second_aside,
  ] {
    if let Ok(bytes) = fs::read(data.join("cleanup-status.json")) {
      let record: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
      if record["status"] != "pass" {
        result = Err(format!("Gate cleanup failed: {record}"));
      }
      cleanup.push(json!({"data":data,"result":record}));
    }
  }
  atomic_json(
    output,
    &json!({"target":postgres::target_slug(),"status":if result.is_ok(){"pass"}else{"fail"},"steps":steps,
      "error":result.as_ref().err(),"data_retained":base,"cleanup":cleanup,
      "windows_ascii_aliases_exercised":cfg!(windows),
      "windows_ran_in_a_second_process_without_administrator_rights":windows_rights_dropped(),
      "steps_not_run_on_this_system": if cfg!(unix) { json!([]) } else { json!([
        {"name":"earlier_database_still_running_after_data_folder_rename",
         "reason":"Windows ends the database with the app (Job Object), and the app does not read the working folder of a process there"},
        {"name":"earlier_database_whose_program_was_moved_or_deleted",
         "reason":"Windows ends the database with the app (Job Object), and a running program file cannot be moved or deleted there"},
        {"name":"earlier_database_after_rename_and_a_start_at_the_old_name",
         "reason":"Windows ends the database with the app (Job Object), and a folder in which a process runs cannot be renamed there"}]) },
      "installer_run":false}),
  )?;
  result
}

/// Round 8: the restored dump was compared with the live database as it was some seconds
/// after the dump, and the server had written a row of `series_scan_state` in that time
/// (gate run of 2026-10-12 on aarch64 macOS). A dump has to equal the database at the
/// moment it was made.
#[test]
fn a_dump_is_compared_with_the_live_database_at_the_time_it_was_made() {
  let hashes = |rows: &[(&str, &str)]| -> Hashes {
    rows
      .iter()
      .map(|(table, hash)| (table.to_string(), hash.to_string()))
      .collect()
  };
  let before = hashes(&[("users", "a"), ("series_scan_state", "b"), ("chapter", "c")]);
  // The server wrote one table while the dump was made.
  let after = hashes(&[("users", "a"), ("series_scan_state", "B"), ("chapter", "c")]);
  // What the old comparison did: the live database afterwards against a dump that was
  // taken before the write. It called a correct dump wrong.
  assert_eq!(hash_differences(&after, &before), ["series_scan_state"]);
  // The dump holds the table as it was before the write, or as it was after: both are
  // the database at a moment in which the dump can have been made.
  assert!(dump_differences(&before, &after, &before).is_empty());
  assert!(dump_differences(&before, &after, &after).is_empty());
  // Neither: the dump is wrong in that table.
  let neither = hashes(&[("users", "a"), ("series_scan_state", "x"), ("chapter", "c")]);
  assert_eq!(
    dump_differences(&before, &after, &neither),
    ["series_scan_state"]
  );
  // A table the server did not change must be equal, whatever else changed.
  let wrong = hashes(&[("users", "A"), ("series_scan_state", "B"), ("chapter", "c")]);
  assert_eq!(dump_differences(&before, &after, &wrong), ["users"]);
  // A table that is missing in the dump.
  let missing = hashes(&[("users", "a"), ("series_scan_state", "b")]);
  assert_eq!(dump_differences(&before, &after, &missing), ["chapter"]);
  // A table whose count the server changes by itself is not compared at all.
  let (name, _) = NOT_COMPARED[0];
  let with = |hash: &str| hashes(&[("users", "a"), (name, hash)]);
  assert!(dump_differences(&with("1"), &with("2"), &with("3")).is_empty());
  // Round 9: the lists of tables are compared too. A table that is in only one or two
  // of the three is a difference, whatever the dump holds for it.
  let base = [("users", "a"), ("chapter", "c")];
  let plus = |table: &'static str| {
    let mut rows = base.to_vec();
    rows.push((table, "n"));
    hashes(&rows)
  };
  let plain = hashes(&base);
  // Only in the live database after the dump (and the dump may or may not have it).
  assert_eq!(dump_differences(&plain, &plus("new"), &plain), ["new"]);
  assert_eq!(
    dump_differences(&plain, &plus("new"), &plus("new")),
    ["new"]
  );
  // Only in the live database before the dump.
  assert_eq!(dump_differences(&plus("old"), &plain, &plain), ["old"]);
  assert_eq!(
    dump_differences(&plus("old"), &plain, &plus("old")),
    ["old"]
  );
  // Only in the dump: a table the live database never had.
  assert_eq!(dump_differences(&plain, &plain, &plus("extra")), ["extra"]);
  // In all three: no difference.
  assert!(dump_differences(&plus("t"), &plus("t"), &plus("t")).is_empty());
}

/// Round 8: the first process of Windows took an evidence file that an earlier run had
/// left for the result of this run, when the second process ended with 0 and wrote
/// nothing. Runs on every system: the second process is passed in.
#[test]
fn the_first_process_takes_only_evidence_that_the_second_wrote_in_this_run() {
  let dir = std::env::temp_dir().join(format!(
    "komiq-gate-second-{}-{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  fs::create_dir(&dir).unwrap();
  let file = dir.join("evidence.json");
  const SECOND: &str = "windows_ran_in_a_second_process_without_administrator_rights";
  let passed = |second: bool| {
    let steps: Vec<Value> = expected_steps(true)
      .iter()
      .map(|name| json!({"name": name, "status": "pass"}))
      .collect();
    json!({"status":"pass", "steps": steps, SECOND: second})
  };
  let write = |evidence: &Value| fs::write(&file, evidence.to_string()).unwrap();
  let read = || serde_json::from_slice::<Value>(&fs::read(&file).unwrap()).unwrap();
  // Control: this file, written by the second process of this run, is a pass.
  write(&passed(true));
  assert_eq!(evidence_complete(&file, true), Ok(()));
  assert_eq!(second_process(&file), Ok(()));

  // The finding: an earlier pass is in the file, and the second process ends with 0
  // without writing anything (no test ran in it).
  let stale = through_second_process(&file, true, || Ok(0)).unwrap_err();
  assert!(
    stale.contains("ended with exit code 0") && stale.contains("its status is \"running\""),
    "{stale}"
  );
  assert_eq!(read()["status"], "running");
  assert_eq!(read()["phase"], "administrator_rights");
  // What the second process sees when it starts is already not a pass.
  write(&passed(true));
  let mut seen_by_second = Value::Null;
  let fresh = through_second_process(&file, true, || {
    seen_by_second = read();
    write(&passed(true));
    Ok(0)
  });
  assert_eq!(fresh, Ok(()), "a pass that was written in this run");
  assert_eq!(seen_by_second["status"], "running");
  // Written in this run, but not by a second process without the rights.
  let other = through_second_process(&file, true, || {
    write(&passed(false));
    Ok(0)
  })
  .unwrap_err();
  assert!(other.contains("not true"), "{other}");
  // The steps of another system.
  let unix = through_second_process(&file, false, || {
    write(&passed(true));
    Ok(0)
  })
  .unwrap_err();
  assert!(unix.contains("expected exactly"), "{unix}");
  // A failed gate: its own evidence stays.
  let failed = through_second_process(&file, true, || {
    write(&json!({"status":"fail","error":"step 3"}));
    Ok(101)
  })
  .unwrap_err();
  assert!(failed.contains("exit code 101"), "{failed}");
  assert_eq!(read()["error"], "step 3");
  // The rights could not be dropped: no step ran, and the file says so.
  write(&passed(true));
  let undropped =
    through_second_process(&file, true, || Err("CreateRestrictedToken: denied".into()))
      .unwrap_err();
  assert!(
    undropped.contains("CreateRestrictedToken: denied"),
    "{undropped}"
  );
  assert_eq!(read()["status"], "fail");
  assert_eq!(read()["phase"], "administrator_rights");
  // The earlier evidence cannot be replaced (here: its folder is gone): nothing is started.
  let nowhere = dir.join("gone").join("evidence.json");
  let mut started = false;
  let unwritten = through_second_process(&nowhere, true, || {
    started = true;
    Ok(0)
  })
  .unwrap_err();
  assert!(
    unwritten.contains("could not be replaced before the second process was started"),
    "{unwritten}"
  );
  assert!(!started, "the second process is not started");
  fs::remove_dir_all(&dir).unwrap();
}

/// The evidence rule that the Windows parent process and the end of the driver apply (the
/// workflow's last step applies the same rule in its own words): a passed status alone,
/// a missing step, a step too many, steps of the other system and a failed step are all
/// refused.
#[test]
fn gate_evidence_must_be_a_pass_with_exactly_the_expected_steps() {
  let dir = std::env::temp_dir().join(format!(
    "komiq-gate-evidence-{}-{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  fs::create_dir(&dir).unwrap();
  let file = dir.join("evidence.json");
  let check = |windows: bool, evidence: Value| {
    fs::write(&file, evidence.to_string()).unwrap();
    evidence_complete(&file, windows)
  };
  let steps = |names: &[&str]| -> Vec<Value> {
    names
      .iter()
      .map(|name| json!({"name": name, "status": "pass"}))
      .collect()
  };
  let (unix, windows) = (expected_steps(false), expected_steps(true));
  assert_eq!((unix.len(), windows.len()), (8, 5));
  assert!(windows.iter().all(|name| unix.contains(name)));
  assert_eq!(
    check(false, json!({"status":"pass","steps":steps(&unix)})),
    Ok(())
  );
  assert_eq!(
    check(true, json!({"status":"pass","steps":steps(&windows)})),
    Ok(())
  );
  // The steps of the other system.
  assert!(check(true, json!({"status":"pass","steps":steps(&unix)})).is_err());
  assert!(check(false, json!({"status":"pass","steps":steps(&windows)})).is_err());
  // What the workflow writes before anything ran, and what a run that ran no test leaves.
  let blocked = check(false, json!({"status":"blocked","steps":[]})).unwrap_err();
  assert!(blocked.contains("its status is \"blocked\""), "{blocked}");
  assert!(check(false, json!({"status":"running","steps":steps(&unix)})).is_err());
  assert!(check(false, json!({"status":"pass","steps":[]})).is_err());
  assert!(check(false, json!({"status":"pass"})).is_err());
  // One step missing, one too many, two swapped, one not passed.
  assert!(check(
    false,
    json!({"status":"pass","steps":steps(&unix[..unix.len() - 1])})
  )
  .is_err());
  let mut more = unix.clone();
  more.push("one_more");
  assert!(check(false, json!({"status":"pass","steps":steps(&more)})).is_err());
  let mut swapped = unix.clone();
  swapped.swap(0, 1);
  assert!(check(false, json!({"status":"pass","steps":steps(&swapped)})).is_err());
  let mut failed = steps(&unix);
  failed[4]["status"] = json!("fail");
  let error = check(false, json!({"status":"pass","steps":failed})).unwrap_err();
  assert!(error.contains("earlier_database_still_running"), "{error}");
  // Windows: the result that the first process passes on must come from the second one.
  const SECOND: &str = "windows_ran_in_a_second_process_without_administrator_rights";
  fs::write(&file, json!({"status":"pass", SECOND: true}).to_string()).unwrap();
  assert_eq!(second_process(&file), Ok(()));
  for other in [
    json!({"status":"pass", SECOND: false}),
    json!({"status":"pass", SECOND: null}),
    json!({"status":"pass"}),
  ] {
    fs::write(&file, other.to_string()).unwrap();
    let error = second_process(&file).unwrap_err();
    assert!(error.contains("not true"), "{error}");
  }
  fs::write(&file, "not json").unwrap();
  assert!(evidence_complete(&file, false).is_err());
  assert!(second_process(&file).is_err());
  fs::remove_file(&file).unwrap();
  assert!(evidence_complete(&file, false).is_err());
  assert!(second_process(&file).is_err());
  fs::remove_dir(&dir).unwrap();
}

/// The rule for `instance_setting`: the fixture's rows with equal content, and an extra
/// row only when the server is known to write it by itself.
#[test]
fn instance_setting_rule_accepts_only_the_fixture_and_known_server_rows() {
  let row = |key: &str, value: &str, at: &str| [key.to_string(), value.to_string(), at.to_string()];
  let fixture = vec![row(
    "instance_name",
    "Gate fixture é 漢字",
    "2021-02-03T04:05:06.789Z",
  )];
  // As PostgreSQL gives the same row back: the same instant, written with microseconds.
  let kept = row(
    "instance_name",
    "Gate fixture é 漢字",
    "2021-02-03T04:05:06.789000Z",
  );
  let uuid = "0b6f7c1e-5a52-4d0b-9f0e-3c2a1d4e5f60";
  let stamp = "2026-10-11T10:00:00.000000Z";
  let check = |now: &[[String; 3]]| settings_check("test", &fixture, now);
  // What the gate meets: the fixture's row and the server's own.
  let met = check(&[row("direct_tls_generation", uuid, stamp), kept.clone()]).unwrap();
  assert_eq!(met["fixture_rows_present_with_equal_content"], 1);
  assert_eq!(
    met["extra_rows_written_by_the_server"][0]["key"],
    "direct_tls_generation"
  );
  assert!(check(std::slice::from_ref(&kept)).is_ok());
  assert!(check(&[
    kept.clone(),
    row("catalogue_sync_generation:mangadex", "3", stamp)
  ])
  .is_ok());
  // A fixture row that is gone, or changed in its value or its time.
  for wrong in [
    vec![row("direct_tls_generation", uuid, stamp)],
    vec![row(
      "instance_name",
      "Gate fixture",
      "2021-02-03T04:05:06.789000Z",
    )],
    vec![row(
      "instance_name",
      "Gate fixture é 漢字",
      "2021-02-03T04:05:06.790000Z",
    )],
    vec![],
  ] {
    assert!(check(&wrong).is_err(), "{wrong:?}");
  }
  // A row that neither the fixture nor a known server start writes, and the known keys
  // with a value of another shape.
  for (key, value) in [
    ("instance_description", "x"),
    ("direct_tls_generation", "not a uuid"),
    ("direct_tls_generation", ""),
    ("direct_tls_generation2", uuid),
    ("catalogue_sync_generation:", "3"),
    ("catalogue_sync_generation:mangadex", "three"),
    ("catalogue_sync_generation", "3"),
  ] {
    let error = check(&[kept.clone(), row(key, value, stamp)]).unwrap_err();
    assert!(
      error.contains("neither the fixture nor a known server start"),
      "{error}"
    );
  }
  // The count rule beside it: more rows than the fixture pass the count, fewer do not.
  let counts = |rows: u64| Counts::from([(SETTINGS_TABLE.to_string(), rows), ("users".into(), 3)]);
  assert!(differences(&counts(1), &counts(2)).is_empty());
  assert_eq!(differences(&counts(2), &counts(1)).len(), 1);
  let mut users = counts(1);
  users.insert("users".into(), 4);
  assert_eq!(
    differences(&counts(1), &users).len(),
    1,
    "other tables stay exact"
  );
  assert!(parse_settings("[[\"a\",\"b\"]]").is_err());
}

#[test]
fn gate_preflight_failures_leave_json_and_existing_data_untouched() {
  let base = std::env::temp_dir().join(format!("komiq-gate-preflight-{}", std::process::id()));
  fs::create_dir(&base).unwrap();
  fs::write(base.join("precious"), "foreign data").unwrap();
  let output = base.with_extension("json");
  for missing_runtime in [true, false] {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
      .args(["--ignored", "--exact", "desktop_postgres_gate"])
      .env("KOMIKA_DESKTOP_GATE_JSON", &output)
      .env("KOMIKA_DESKTOP_GATE_ROOT", &base)
      .env("KOMIKA_TEST_SERVER_BIN", "not-used");
    if missing_runtime {
      command.env_remove("KOMIKA_POSTGRES_RUNTIME");
    } else {
      command.env("KOMIKA_POSTGRES_RUNTIME", "not-used");
    }
    assert!(!command.output().unwrap().status.success());
    let record: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(record["status"], "fail");
    assert_eq!(
      fs::read_to_string(base.join("precious")).unwrap(),
      "foreign data"
    );
    assert_eq!(fs::read_dir(&base).unwrap().count(), 1);
  }
  fs::remove_dir_all(base).unwrap();
  fs::remove_file(output).unwrap();
}

// =======================================================================================
// Installer gate (fix round 12, 2026-10-12): "reinstall with data kept" with a real
// installer. Everything above this line is the lifecycle gate as it was accepted; this
// part only adds to the file, and no line above was changed for it.
//
// The lifecycle gate copies the runtime to another path and opens the same data
// (`relocated_runtime_same_data`); no installer runs there. This test takes a package
// built by scripts/installer-gate-build.sh (the product's `tauri build`: real identifier,
// real installer settings, stock templates, unsigned) and does what a user's machine
// does with it: install, start the database and the server from the installed files with
// the data folder where the app itself puts it, install again over it, uninstall with the
// installer's default choices, install again. After each of these the data folder, the
// credentials file, the cluster and the rows must be the ones of the first start.
//
// The app's window does not run here. A first launch needs a click on "Run komiq on this
// computer" before the app starts its server (apps/admin/src/routes/start/+page.svelte:
// the choice is kept in the web view's local storage), and nothing exists today that
// starts it without one. So the worker of the lifecycle gate drives `LocalServer`, with
// the paths worked out the way `lib.rs` does:
//   * data folder: Tauri's `app_data_dir()` is `dirs::data_dir()` joined with the
//     identifier (tauri 2.11.6, src/path/desktop.rs:247-251); this test calls the same
//     `dirs::data_dir()` (the one copy of `dirs` in Cargo.lock) and joins the identifier
//     of tauri.conf.json; `lib.rs:213` appends `server`, as the worker does;
//   * runtime: the product's own `postgres::resolve(resource_dir)` (`lib.rs:214`), with
//     `KOMIKA_POSTGRES_RUNTIME` taken out of the environment;
//   * `resource_dir` and the server program: Tauri's rule for an installed app
//     (tauri-utils 2.9.3, src/platform.rs:281-342: next to the program on Windows,
//     `../Resources` on macOS, `../lib/<productName>` on Linux) and `local::server_bin`
//     (`komika-server` next to the app's program), written out again in `installed`
//     below, because both read the path of the running program, which here is the test.
// The server gets no web folder (`web_dir` is `None` in the worker).
//
// Isolation. Nothing is touched before these hold, and the run refuses otherwise:
//   * the data folder that was worked out must not exist (an existing one is never used);
//   * macOS: the home folder is a new folder below the scratch root, the data folder
//     must come out below that root, and the app is copied into a scratch
//     `Applications` folder, never into /Applications;
//   * Linux: `dpkg` changes the system, so KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE=1 is
//     required (a CI runner, a container); the home folder is a scratch one unless
//     KOMIKA_INSTALLER_GATE_REAL_PROFILE=1;
//   * Windows: the data folder is a known folder of the user profile and cannot be moved
//     by the environment, and the installer writes the registry: both variables above
//     are required.
// Environment: KOMIKA_INSTALLER_GATE_JSON (evidence file), KOMIKA_INSTALLER_GATE_ROOT (a
// folder that does not exist yet), KOMIKA_INSTALLER_GATE_PACKAGE or
// KOMIKA_INSTALLER_GATE_BUILD_JSON (the facts file of the build script, which names the
// package), KOMIKA_INSTALLER_GATE_SUDO (Linux, default `sudo -n`).
// =======================================================================================

use std::ffi::OsString;

const INSTALLER_MINUTE: Duration = Duration::from_secs(60);

#[cfg(target_os = "linux")]
const INSTALLER_KIND: &str = "deb";
#[cfg(target_os = "macos")]
const INSTALLER_KIND: &str = "macos_app_copy";
#[cfg(windows)]
const INSTALLER_KIND: &str = "nsis";

/// The steps of the installer gate, in order, on the system `os`
/// (`std::env::consts::OS`). The workflow holds the same lists.
fn installer_expected_steps(os: &str) -> Vec<&'static str> {
  let mut steps = vec![
    "package_identity",
    "install",
    "first_start_from_the_installed_files",
    "install_again_over_the_installed_one",
    "uninstall_with_the_installers_defaults",
    "install_again_after_the_uninstall",
  ];
  // Only dpkg knows a second, stronger removal.
  if os == "linux" {
    steps.push("purge_recorded_only");
  }
  steps
}

/// Whether the evidence file `output` shows a passed installer gate on `os`: status
/// `pass`, an installer that ran, and exactly the expected steps in order, each passed.
fn installer_evidence_complete(output: &Path, os: &str) -> Result<(), String> {
  let evidence: Value = serde_json::from_slice(&fs::read(output).map_err(|e| e.to_string())?)
    .map_err(|e| format!("it is not JSON: {e}"))?;
  if evidence["status"] != "pass" {
    return Err(format!(
      "its status is {}, not \"pass\"",
      evidence["status"]
    ));
  }
  if evidence["installer_run"] != true {
    return Err(format!(
      "its field installer_run is {}, not true",
      evidence["installer_run"]
    ));
  }
  let steps = evidence["steps"]
    .as_array()
    .ok_or("it has no list of steps")?;
  let names: Vec<&str> = steps
    .iter()
    .map(|step| step["name"].as_str().unwrap_or(""))
    .collect();
  let expected = installer_expected_steps(os);
  if names != expected {
    return Err(format!(
      "its steps are {names:?}, expected exactly {expected:?}"
    ));
  }
  if let Some(step) = steps.iter().find(|step| step["status"] != "pass") {
    return Err(format!(
      "step {} has status {}",
      step["name"], step["status"]
    ));
  }
  Ok(())
}

fn installer_hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn installer_file_sha256(path: &Path) -> Result<String, String> {
  use sha2::{Digest, Sha256};
  let mut file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
  let mut hash = Sha256::new();
  std::io::copy(&mut file, &mut hash).map_err(|e| format!("{}: {e}", path.display()))?;
  Ok(installer_hex(&hash.finalize()))
}

/// The files below `root`: how many, their bytes, and one SHA-256 over a line per file
/// (its path below `root` with `/`, a tab, the SHA-256 of its content), in the order of
/// the names in each folder. A link is followed, as a program that opens it does.
fn installer_tree(root: &Path) -> Result<Value, String> {
  use sha2::{Digest, Sha256};
  fn walk(
    dir: &Path,
    prefix: &str,
    lines: &mut Vec<String>,
    bytes: &mut u64,
  ) -> Result<(), String> {
    let mut entries = fs::read_dir(dir)
      .map_err(|e| format!("{}: {e}", dir.display()))?
      .collect::<Result<Vec<_>, _>>()
      .map_err(|e| format!("{}: {e}", dir.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
      let path = entry.path();
      let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
      if path.is_dir() {
        walk(&path, &format!("{name}/"), lines, bytes)?;
      } else if let Ok(metadata) = fs::metadata(&path) {
        *bytes += metadata.len();
        lines.push(format!("{name}\t{}", installer_file_sha256(&path)?));
      } else {
        lines.push(format!("{name}\ta link to nothing"));
      }
    }
    Ok(())
  }
  let (mut lines, mut bytes) = (Vec::new(), 0);
  walk(root, "", &mut lines, &mut bytes)?;
  Ok(json!({
    "files": lines.len(),
    "bytes": bytes,
    "sha256": installer_hex(&Sha256::digest(lines.join("\n").as_bytes())),
  }))
}

/// One installed file: whether it is there, its size and content hash, and on Unix whose
/// it is, its mode, and which file it is (a new inode: the installer wrote a new file).
fn installer_file_fact(path: &Path) -> Value {
  let Ok(metadata) = fs::metadata(path) else {
    return json!({"path": path, "exists": false});
  };
  #[allow(unused_mut)]
  let mut fact = json!({
    "path": path,
    "exists": true,
    "bytes": metadata.len(),
    "sha256": installer_file_sha256(path).unwrap_or_else(|error| error),
  });
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    fact["owner_uid"] = json!(metadata.uid());
    fact["mode"] = json!(format!("{:o}", metadata.mode() & 0o7777));
    fact["inode"] = json!(metadata.ino());
    fact["status_changed_unix_seconds"] = json!(metadata.ctime());
  }
  fact
}

/// The last `max` characters of `text`.
fn installer_tail(text: &str, max: usize) -> String {
  let skip = text.chars().count().saturating_sub(max);
  text.chars().skip(skip).collect()
}

fn installer_names(dir: &Path) -> Vec<String> {
  let mut names: Vec<String> = fs::read_dir(dir)
    .map(|entries| {
      entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
    })
    .unwrap_or_default();
  names.sort();
  names
}

/// What identifies the data of one install while its database is stopped: the content
/// of the credentials file, the cluster's system identifier (the first eight bytes of
/// its control file; `initdb` gives every new cluster another one) and the version file.
/// `Err`: the data folder, the credentials or the cluster is not there.
fn installer_data_facts(app_data: &Path) -> Result<Value, String> {
  let server = app_data.join("server");
  let cluster = postgres::cluster_dir(&server);
  let data = cluster.join("data");
  let read = |what: &str, path: PathBuf| {
    fs::read(&path).map_err(|e| format!("The {what} {} cannot be read: {e}", path.display()))
  };
  let credentials = read("credentials file", cluster.join("credentials"))?;
  let control = read("control file", data.join("global").join("pg_control"))?;
  let identifier = control
    .get(..8)
    .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
    .map(u64::from_le_bytes)
    .ok_or("The control file is shorter than eight bytes")?;
  let version = read("version file", data.join("PG_VERSION"))?;
  // Files and bytes below the app data folder; a link or junction is counted, not entered.
  fn count(dir: &Path, files: &mut u64, bytes: &mut u64) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
      match entry.file_type() {
        Ok(kind) if kind.is_dir() => count(&entry.path(), files, bytes),
        Ok(kind) if kind.is_file() => {
          *files += 1;
          *bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
        _ => *files += 1,
      }
    }
  }
  let (mut files, mut bytes) = (0, 0);
  count(app_data, &mut files, &mut bytes);
  let credentials_sha256 = {
    use sha2::{Digest, Sha256};
    installer_hex(&Sha256::digest(&credentials))
  };
  Ok(json!({
    "app_data_dir": app_data,
    "server_data_dir": server,
    "credentials_sha256": credentials_sha256,
    "cluster_system_identifier": identifier.to_string(),
    "postgresql_version_of_the_data": String::from_utf8_lossy(&version).trim(),
    "lock_file_present": data.join("postmaster.pid").exists(),
    "names_in_the_app_data_dir": installer_names(app_data),
    "names_in_the_server_data_dir": installer_names(&server),
    "files": files,
    "bytes": bytes,
  }))
}

/// The data that is there `now` must be the data that was `kept`: same credentials, same
/// cluster, same version file.
fn installer_same_data(kept: &Value, now: &Value, when: &str) -> Result<(), String> {
  for key in [
    "credentials_sha256",
    "cluster_system_identifier",
    "postgresql_version_of_the_data",
  ] {
    if kept[key].is_null() || kept[key] != now[key] {
      return Err(format!(
        "{when}: {key} was {} after the first start and is {} now",
        kept[key], now[key]
      ));
    }
  }
  Ok(())
}

/// Where an installed app has its files, by the rules named at the top of this part.
struct Installed {
  /// What the installer creates and the uninstaller removes.
  location: PathBuf,
  main_binary: PathBuf,
  server: PathBuf,
  resource_dir: PathBuf,
}

/// One run of the installer gate. The evidence file is written again after every step.
struct InstallerGate {
  output: PathBuf,
  root: PathBuf,
  package: PathBuf,
  product_name: String,
  /// Checked against the package where the package holds it (not in a `.deb`).
  #[cfg(not(target_os = "linux"))]
  identifier: String,
  version: String,
  app_data: PathBuf,
  /// Linux: the name of the package inside the `.deb`.
  #[cfg(target_os = "linux")]
  package_name: String,
  header: serde_json::Map<String, Value>,
  steps: Vec<Value>,
  commands: Vec<Value>,
  /// `id:value` of every row written so far.
  rows: Vec<String>,
  /// The installed files as the last install left them.
  installed_now: Value,
  installer_ran: bool,
}

impl InstallerGate {
  /// Read the request, refuse what must be refused, and only then create the scratch
  /// root. Nothing is installed here.
  fn prepare(output: &Path) -> Result<Self, String> {
    let variable = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    let flag = |name: &str| std::env::var(name).as_deref() == Ok("1");
    let build: Value = match variable("KOMIKA_INSTALLER_GATE_BUILD_JSON") {
      Some(path) => {
        let path = PathBuf::from(path);
        serde_json::from_slice(&fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?)
          .map_err(|e| format!("{}: {e}", path.display()))?
      }
      None => Value::Null,
    };
    let package = variable("KOMIKA_INSTALLER_GATE_PACKAGE")
      .map(PathBuf::from)
      .or_else(|| build["package"].as_str().map(PathBuf::from))
      .ok_or(
        "KOMIKA_INSTALLER_GATE_PACKAGE, or KOMIKA_INSTALLER_GATE_BUILD_JSON with a package, \
         required",
      )?;
    if !package.exists() {
      return Err(format!("The package {} does not exist", package.display()));
    }
    let root = variable("KOMIKA_INSTALLER_GATE_ROOT")
      .map(PathBuf::from)
      .ok_or("KOMIKA_INSTALLER_GATE_ROOT required")?;
    let conf_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    let conf: Value = serde_json::from_slice(&fs::read(&conf_path).map_err(|e| e.to_string())?)
      .map_err(|e| format!("{}: {e}", conf_path.display()))?;
    let text = |key: &str| {
      conf[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{} has no {key}", conf_path.display()))
    };
    let (product_name, identifier, version) =
      (text("productName")?, text("identifier")?, text("version")?);
    let throwaway = flag("KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE");
    let real_profile = flag("KOMIKA_INSTALLER_GATE_REAL_PROFILE");
    if cfg!(target_os = "macos") && real_profile {
      return Err(
        "On macOS this gate never uses the real user profile or /Applications: \
         KOMIKA_INSTALLER_GATE_REAL_PROFILE is refused. Nothing was installed."
          .into(),
      );
    }
    if cfg!(target_os = "linux") && !throwaway {
      return Err(
        "This gate installs and removes a system package with dpkg. Run it only on a CI \
         runner or in a container, with KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE=1. Nothing \
         was installed."
          .into(),
      );
    }
    if cfg!(windows) && !(throwaway && real_profile) {
      return Err(
        "On Windows the app's data folder is a known folder of the user profile, which the \
         environment cannot move, and the installer writes the registry. Run this gate only \
         on a CI runner, with KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE=1 and \
         KOMIKA_INSTALLER_GATE_REAL_PROFILE=1. Nothing was installed."
          .into(),
      );
    }
    if real_profile && !throwaway {
      return Err(
        "KOMIKA_INSTALLER_GATE_REAL_PROFILE=1 needs KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE=1. \
         Nothing was installed."
          .into(),
      );
    }
    #[cfg(unix)]
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
      return Err(
        "PostgreSQL does not run as root. Run this gate as an ordinary user; it asks for \
         root only for dpkg (KOMIKA_INSTALLER_GATE_SUDO). Nothing was installed."
          .into(),
      );
    }
    // From here on a folder exists. Atomic creation is the authority, as in the gate above.
    fs::create_dir(&root).map_err(|e| {
      format!(
        "The scratch root {} must be absent; nothing was created or installed: {e}",
        root.display()
      )
    })?;
    #[cfg(windows)]
    let root = postgres::gate_work_path(&root)?;
    #[cfg(not(windows))]
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let shown = |name: &str| {
      std::env::var_os(name)
        .map(|value| json!(value.to_string_lossy()))
        .unwrap_or(Value::Null)
    };
    let (home_before, xdg_before) = (shown("HOME"), shown("XDG_DATA_HOME"));
    if !real_profile {
      let home = root.join("home");
      fs::create_dir(&home).map_err(|e| e.to_string())?;
      std::env::set_var("HOME", &home);
      std::env::remove_var("XDG_DATA_HOME");
    }
    // Where the runtime is must follow from the installed files, not from the variable
    // with which the lifecycle gate (and a developer) points the app at a runtime.
    let runtime_variable_was_set = variable("KOMIKA_POSTGRES_RUNTIME").is_some();
    std::env::remove_var("KOMIKA_POSTGRES_RUNTIME");
    let user_data = dirs::data_dir().ok_or("This system gives no data folder for the user")?;
    let app_data = user_data.join(&identifier);
    let below = app_data.starts_with(&root);
    if !real_profile && !below {
      return Err(format!(
        "The app's data folder comes out at {}, which is not below the scratch root {}. \
         Nothing was installed.",
        app_data.display(),
        root.display()
      ));
    }
    if fs::symlink_metadata(&app_data).is_ok() {
      return Err(format!(
        "The app's data folder {} exists already. This gate never uses or changes an \
         existing data folder. Nothing was installed.",
        app_data.display()
      ));
    }
    let mut header = serde_json::Map::new();
    let mut put = |key: &str, value: Value| header.insert(key.to_string(), value);
    put("gate", json!("desktop_installer"));
    put("target", json!(postgres::target_slug()));
    put("installer_kind", json!(INSTALLER_KIND));
    put(
      "app_shell",
      json!({"ran": false,
        "why": "A first launch starts the local server only after a click on the start page, \
          and nothing exists today that starts it without one. The worker of the lifecycle \
          gate drove LocalServer with the installed files and the app's data folder; the \
          Tauri setup of lib.rs and the window did not run.",
        "server_web_folder_given": false}),
    );
    put(
      "product",
      json!({"product_name": product_name, "identifier": identifier, "version": version,
        "read_from": conf_path, "app_program_name": env!("CARGO_PKG_NAME")}),
    );
    put(
      "isolation",
      json!({
        "profile": if real_profile { "the real user profile of a throwaway machine" } else { "a scratch home folder below the scratch root" },
        "throwaway_machine_stated": throwaway,
        "HOME_before": home_before,
        "XDG_DATA_HOME_before": xdg_before,
        "HOME_of_this_run": shown("HOME"),
        "XDG_DATA_HOME_of_this_run": shown("XDG_DATA_HOME"),
        "resolved_by": "dirs::data_dir() of dirs 6.0.0, the call inside Tauri's app_data_dir(), joined with the identifier",
        "user_data_dir": path_fact(&user_data),
        "app_data_dir": path_fact(&app_data),
        "server_data_dir": path_fact(&app_data.join("server")),
        "app_data_dir_is_below_the_scratch_root": below,
        "app_data_dir_existed_before": false,
        "KOMIKA_POSTGRES_RUNTIME_was_set_and_removed": runtime_variable_was_set,
        "files_of_the_test_in_the_app_data_dir": "the worker of the lifecycle gate keeps its small files there (ready.json, request.json, response.json, stop, worker.log, prestart.jsonl, gate-last-port, database-and-server.log, cleanup-status.json); the app does not make them",
      }),
    );
    put("scratch_root", json!(path_fact(&root)));
    put("build", build);
    Ok(Self {
      output: output.to_path_buf(),
      root,
      package,
      product_name,
      #[cfg(not(target_os = "linux"))]
      identifier,
      version,
      app_data,
      #[cfg(target_os = "linux")]
      package_name: String::new(),
      header,
      steps: Vec::new(),
      commands: Vec::new(),
      rows: Vec::new(),
      installed_now: Value::Null,
      installer_ran: false,
    })
  }

  fn write(&self, status: &str, error: Option<&str>) -> Result<(), String> {
    let mut evidence = self.header.clone();
    let mut put = |key: &str, value: Value| evidence.insert(key.to_string(), value);
    put("status", json!(status));
    put("installer_run", json!(self.installer_ran));
    put("steps", json!(self.steps));
    put("commands", json!(self.commands));
    put("error", json!(error));
    put(
      "windows_ran_in_a_second_process_without_administrator_rights",
      windows_rights_dropped(),
    );
    atomic_json(&self.output, &Value::Object(evidence))
  }

  fn step(
    &mut self,
    name: &str,
    action: impl FnOnce(&mut Self) -> Result<Value, String>,
  ) -> Result<(), String> {
    let before = Instant::now();
    let result = action(self);
    let seconds = before.elapsed().as_secs_f64();
    self.steps.push(match &result {
      Ok(facts) => json!({"name":name,"status":"pass","seconds":seconds,"facts":facts}),
      Err(error) => json!({"name":name,"status":"fail","seconds":seconds,"error":error}),
    });
    self.write("running", None)?;
    result.map(|_| ())
  }

  /// Run one command to its end and keep it in the evidence: its words, its exit status,
  /// its time and the end of what it printed. `raw_tail`: text put at the end of the
  /// command line as it is (Windows; NSIS wants `/D=` last and without quotes). Gives the
  /// exit status and the standard output. `Err`: it could not be started, or did not end
  /// within `bound` (it is killed then).
  fn command(
    &mut self,
    step: &str,
    program: &Path,
    arguments: Vec<OsString>,
    raw_tail: Option<&str>,
    bound: Duration,
  ) -> Result<(Option<i32>, String), String> {
    let folder = self.root.join("command output");
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let number = self.commands.len() + 1;
    let file = |stream: &str| folder.join(format!("{number:02} {step} {stream}.txt"));
    let (out_path, err_path) = (file("stdout"), file("stderr"));
    let create =
      |path: &Path| fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()));
    let mut command = Command::new(program);
    command
      .args(&arguments)
      .stdin(Stdio::null())
      .stdout(create(&out_path)?)
      .stderr(create(&err_path)?);
    #[cfg(windows)]
    if let Some(tail) = raw_tail {
      use std::os::windows::process::CommandExt;
      command.raw_arg(tail);
    }
    #[cfg(not(windows))]
    if let Some(tail) = raw_tail {
      command.arg(tail);
    }
    let mut words: Vec<String> = arguments
      .iter()
      .map(|word| word.to_string_lossy().into_owned())
      .collect();
    words.extend(raw_tail.map(str::to_string));
    let started = Instant::now();
    let mut child = command
      .spawn()
      .map_err(|e| format!("{} could not be started: {e}", program.display()))?;
    let status = loop {
      if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
        break Some(status);
      }
      if started.elapsed() >= bound {
        let _ = child.kill();
        let _ = child.wait();
        break None;
      }
      std::thread::sleep(Duration::from_millis(100));
    };
    let printed =
      |path: &Path| String::from_utf8_lossy(&fs::read(path).unwrap_or_default()).into_owned();
    let (stdout, stderr) = (printed(&out_path), printed(&err_path));
    self.commands.push(json!({
      "step": step,
      "program": program,
      "arguments": words,
      "exit_status": status.and_then(|status| status.code()),
      "ended_within_its_bound": status.is_some(),
      "seconds": started.elapsed().as_secs_f64(),
      "stdout_end": installer_tail(&stdout, 1500),
      "stderr_end": installer_tail(&stderr, 1500),
    }));
    match status {
      Some(status) => Ok((status.code(), stdout)),
      None => Err(format!(
        "{} did not end within {} seconds and was killed",
        program.display(),
        bound.as_secs()
      )),
    }
  }

  /// [`Self::command`], and the exit status must be 0.
  fn expect_ok(
    &mut self,
    step: &str,
    program: &Path,
    arguments: Vec<OsString>,
    raw_tail: Option<&str>,
    bound: Duration,
  ) -> Result<String, String> {
    let (code, stdout) = self.command(step, program, arguments, raw_tail, bound)?;
    if code != Some(0) {
      let stderr = self
        .commands
        .last()
        .map(|command| command["stderr_end"].clone());
      return Err(format!(
        "{} ended with exit status {code:?}: {}",
        program.display(),
        stderr.unwrap_or_default()
      ));
    }
    Ok(stdout)
  }

  /// The package file (macOS: the `.app` folder): its name, size and content hash.
  fn package_file(&self) -> Result<Value, String> {
    let name = self
      .package
      .file_name()
      .map(|name| name.to_string_lossy().into_owned());
    if self.package.is_dir() {
      let tree = installer_tree(&self.package)?;
      Ok(
        json!({"path": self.package, "file_name": name, "a_folder": true,
        "bytes": tree["bytes"], "files": tree["files"], "sha256_of_the_tree": tree["sha256"],
        "sha256_of_the_tree_is": "over one line per file, in the order of the names in each folder: its path below the folder, a tab, the SHA-256 of its content",
        "version": self.version}),
      )
    } else {
      let bytes = fs::metadata(&self.package)
        .map_err(|e| e.to_string())?
        .len();
      Ok(
        json!({"path": self.package, "file_name": name, "a_folder": false, "bytes": bytes,
        "sha256": installer_file_sha256(&self.package)?, "version": self.version}),
      )
    }
  }

  /// The installed files, which must be there: the app's program, the server program and
  /// the database program of the runtime that the product's own rule finds below the
  /// resource folder.
  fn installed_facts(&mut self) -> Result<Value, String> {
    let installed = self.installed();
    let runtime = postgres::resolve(&installed.resource_dir);
    let database_program = runtime.join("bin").join(if cfg!(windows) {
      "postgres.exe"
    } else {
      "postgres"
    });
    for (what, path) in [
      ("app's program", &installed.main_binary),
      ("server program", &installed.server),
      ("database program", &database_program),
    ] {
      if !path.is_file() {
        return Err(format!(
          "After the install the {what} is not at {}. Names in the resource folder {}: {:?}",
          path.display(),
          installed.resource_dir.display(),
          installer_names(&installed.resource_dir)
        ));
      }
    }
    #[allow(unused_mut)]
    let mut facts = json!({
      "location": path_fact(&installed.location),
      "app_program": installer_file_fact(&installed.main_binary),
      "server_program": installer_file_fact(&installed.server),
      "resource_dir": path_fact(&installed.resource_dir),
      "names_in_the_resource_dir": installer_names(&installed.resource_dir),
      "postgresql_runtime": {
        "path": path_fact(&runtime),
        "found_by": "postgres::resolve(resource_dir) of the product, KOMIKA_POSTGRES_RUNTIME not set",
        "database_program": installer_file_fact(&database_program),
        "tree": installer_tree(&runtime)?,
      },
    });
    #[cfg(unix)]
    {
      use std::os::unix::ffi::OsStrExt;
      let path = std::ffi::CString::new(database_program.as_os_str().as_bytes())
        .map_err(|e| e.to_string())?;
      // SAFETY: `path` is a NUL-terminated string that lives until the call returns.
      let writable = unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0;
      facts["this_user_can_write_the_database_program"] = json!(writable);
    }
    self.installed_now = facts.clone();
    Ok(facts)
  }

  /// Start the database and the server from the installed files on the app's data
  /// folder, require every row written so far, write one more, and stop cleanly.
  fn start_and_check(&mut self, new_row: &str) -> Result<Value, String> {
    const ROWS: &str =
      "SELECT coalesce(string_agg(id::text || ':' || value, ';' ORDER BY id), '') \
       FROM installer_gate_row";
    let installed = self.installed();
    let runtime = postgres::resolve(&installed.resource_dir);
    let mut worker = Worker::start(&self.app_data, &runtime, &installed.server)?;
    let program = PathBuf::from(worker.request(json!({"action":"postgres_program"}))?);
    let resolved = |path: &Path| {
      path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))
    };
    if !resolved(&program)?.starts_with(resolved(&installed.resource_dir)?) {
      return Err(format!(
        "The database does not run from the installed files: {}",
        program.display()
      ));
    }
    let first = self.rows.is_empty();
    if first {
      worker.sql("CREATE TABLE installer_gate_row(id bigint PRIMARY KEY, value text NOT NULL)")?;
    } else {
      let found = worker.sql(ROWS)?;
      if found != self.rows.join(";") {
        return Err(format!(
          "The rows written before are not all there: found {found:?}, expected {:?}",
          self.rows.join(";")
        ));
      }
    }
    let id = self.rows.len() + 1;
    worker.sql(&format!(
      "INSERT INTO installer_gate_row VALUES({id}, '{new_row}')"
    ))?;
    self.rows.push(format!("{id}:{new_row}"));
    let found = worker.sql(ROWS)?;
    if found != self.rows.join(";") {
      return Err(format!(
        "The new row was not kept with the earlier ones: {found:?}"
      ));
    }
    if worker.sql("SELECT rolsuper FROM pg_roles WHERE rolname=current_user")? != "f" {
      return Err("Server application role is a superuser".into());
    }
    let version = worker.sql("SHOW server_version")?;
    let log = worker.database_log()?;
    let clean =
      log.contains("database system was shut down at") && !log.contains("not properly shut down");
    if !first && !clean {
      return Err(format!(
        "The start was not a start after a clean shutdown:\n{log}"
      ));
    }
    let address = worker.ready["address"].clone();
    let modes = private_modes(&self.app_data)?;
    worker.stop()?;
    if cluster_data(&self.app_data).join("postmaster.pid").exists() {
      return Err("A clean stop left postmaster.pid".into());
    }
    Ok(json!({
      "database_program": program,
      "database_program_is_below_the_installed_resource_dir": true,
      "server_program": installed.server,
      "server_ready_at": address,
      "postgresql_version": version,
      "application_superuser": false,
      "start_after_a_clean_shutdown": clean,
      "rows_before_this_start_all_present": true,
      "rows_now": self.rows,
      "data_folder_modes": modes,
      "stopped_cleanly": true,
    }))
  }

  fn run_steps(&mut self) -> Result<(), String> {
    self.step("package_identity", |gate| {
      let file = gate.package_file()?;
      let described = gate.describe_package()?;
      Ok(json!({"package": file, "described": described}))
    })?;
    self.step("install", |gate| {
      let installer = gate.install("install", false)?;
      Ok(json!({"installer": installer, "installed": gate.installed_facts()?}))
    })?;
    let mut kept = Value::Null;
    self.step("first_start_from_the_installed_files", |gate| {
      let start = gate.start_and_check("written by the first install é 漢字")?;
      kept = installer_data_facts(&gate.app_data)?;
      Ok(json!({"start": start, "data_after_the_clean_stop": kept}))
    })?;
    // The update case: the same package over the installed one, the data in place.
    self.step("install_again_over_the_installed_one", |gate| {
      let before = gate.installed_now.clone();
      let installer = gate.install("install_again_over_the_installed_one", true)?;
      let installed = gate.installed_facts()?;
      let tree = |facts: &Value| facts["postgresql_runtime"]["tree"]["sha256"].clone();
      if tree(&before).is_null() || tree(&before) != tree(&installed) {
        return Err(format!(
          "The runtime of the second install is not that of the first: {} and {}",
          tree(&before),
          tree(&installed)
        ));
      }
      let data = installer_data_facts(&gate.app_data)?;
      installer_same_data(&kept, &data, "After the install over the installed app")?;
      let start =
        gate.start_and_check("written after the install over the installed app é 漢字")?;
      let after = installer_data_facts(&gate.app_data)?;
      installer_same_data(
        &kept,
        &after,
        "After the start that followed the second install",
      )?;
      Ok(json!({
        "installer": installer,
        "installed": installed,
        "database_program_file_before": before["postgresql_runtime"]["database_program"],
        "database_program_file_now": installed["postgresql_runtime"]["database_program"],
        "data_before_the_start": data,
        "same_credentials": true, "same_cluster": true,
        "start": start,
        "data_after_the_clean_stop": after,
      }))
    })?;
    self.step("uninstall_with_the_installers_defaults", |gate| {
      let uninstaller = gate.uninstall("uninstall_with_the_installers_defaults")?;
      let installed = gate.installed();
      let database_program =
        postgres::resolve(&installed.resource_dir)
          .join("bin")
          .join(if cfg!(windows) {
            "postgres.exe"
          } else {
            "postgres"
          });
      for path in [&installed.main_binary, &installed.server, &database_program] {
        if path.exists() {
          return Err(format!(
            "After the uninstall {} is still there",
            path.display()
          ));
        }
      }
      // `Err` when the data folder, the credentials or the cluster is gone.
      let data = installer_data_facts(&gate.app_data)?;
      installer_same_data(&kept, &data, "After the uninstall")?;
      Ok(json!({
        "uninstaller": uninstaller,
        "program_files_gone": true,
        "left_at_the_install_location": gate.left_behind(),
        "data_folder_kept": true, "same_credentials": true, "same_cluster": true,
        "data_after_the_uninstall": data,
      }))
    })?;
    self.step("install_again_after_the_uninstall", |gate| {
      let installer = gate.install("install_again_after_the_uninstall", false)?;
      let installed = gate.installed_facts()?;
      let data = installer_data_facts(&gate.app_data)?;
      installer_same_data(
        &kept,
        &data,
        "After the install that followed the uninstall",
      )?;
      let start = gate.start_and_check("written after the uninstall and the new install é 漢字")?;
      let after = installer_data_facts(&gate.app_data)?;
      installer_same_data(
        &kept,
        &after,
        "After the start that followed the new install",
      )?;
      Ok(json!({
        "installer": installer,
        "installed": installed,
        "data_before_the_start": data,
        "same_credentials": true, "same_cluster": true,
        "start": start,
        "data_after_the_clean_stop": after,
      }))
    })?;
    #[cfg(target_os = "linux")]
    self.step("purge_recorded_only", |gate| gate.purge())?;
    Ok(())
  }
}

#[cfg(target_os = "linux")]
impl InstallerGate {
  /// Tauri's `.deb`: the programs in /usr/bin, the resources in /usr/lib/<productName>
  /// (the app finds them as `<folder of its program>/../lib/<productName>`).
  fn installed(&self) -> Installed {
    let resources = Path::new("/usr/lib").join(&self.product_name);
    Installed {
      location: resources.clone(),
      main_binary: Path::new("/usr/bin").join(env!("CARGO_PKG_NAME")),
      server: Path::new("/usr/bin").join("komika-server"),
      resource_dir: resources,
    }
  }

  /// `words`, run as root: behind the words of KOMIKA_INSTALLER_GATE_SUDO (`sudo -n`).
  fn as_root(&self, words: Vec<OsString>) -> (PathBuf, Vec<OsString>) {
    let prefix = std::env::var("KOMIKA_INSTALLER_GATE_SUDO").unwrap_or_else(|_| "sudo -n".into());
    let mut all: Vec<OsString> = prefix.split_whitespace().map(OsString::from).collect();
    all.extend(words);
    let program = PathBuf::from(all.remove(0));
    (program, all)
  }

  /// What dpkg says about the package now: the exit status of the query and its text.
  fn dpkg_status(&mut self, step: &str) -> Result<Value, String> {
    let (code, text) = self.command(
      step,
      Path::new("dpkg-query"),
      vec![
        "-W".into(),
        "-f=${Status} ${Version}".into(),
        self.package_name.clone().into(),
      ],
      None,
      INSTALLER_MINUTE,
    )?;
    Ok(json!({"dpkg_query_exit_status": code, "status": text.trim()}))
  }

  fn describe_package(&mut self) -> Result<Value, String> {
    let step = "package_identity";
    let mut fields = serde_json::Map::new();
    for field in ["Package", "Version", "Architecture", "Depends"] {
      let value = self.expect_ok(
        step,
        Path::new("dpkg-deb"),
        vec!["-f".into(), self.package.clone().into(), field.into()],
        None,
        INSTALLER_MINUTE,
      )?;
      fields.insert(field.into(), json!(value.trim()));
    }
    self.package_name = fields["Package"].as_str().unwrap_or_default().to_string();
    if self.package_name.is_empty() {
      return Err("The .deb names no package".into());
    }
    if fields["Version"] != json!(self.version) {
      return Err(format!(
        "The .deb has version {}, tauri.conf.json has {}",
        fields["Version"], self.version
      ));
    }
    // The scripts dpkg would run on install, removal and purge: the stock Tauri .deb has
    // none, and none of them could know the user's home folder anyway.
    let control = self.root.join("deb control files");
    self.expect_ok(
      step,
      Path::new("dpkg-deb"),
      vec![
        "-e".into(),
        self.package.clone().into(),
        control.clone().into(),
      ],
      None,
      INSTALLER_MINUTE,
    )?;
    let names = installer_names(&control);
    let mut scripts = serde_json::Map::new();
    for name in &names {
      if ["preinst", "postinst", "prerm", "postrm", "config"].contains(&name.as_str()) {
        let text = fs::read_to_string(control.join(name)).unwrap_or_default();
        scripts.insert(name.clone(), json!(installer_tail(&text, 4000)));
      }
    }
    let before = self.dpkg_status(step)?;
    if before["status"]
      .as_str()
      .is_some_and(|status| status.starts_with("install ok installed"))
    {
      return Err(format!(
        "The package {} is installed on this machine already. This gate does not touch an \
         existing install. Nothing was installed.",
        self.package_name
      ));
    }
    Ok(
      json!({"deb_fields": fields, "control_files": names, "maintainer_scripts": scripts,
      "state_before_the_first_install": before}),
    )
  }

  fn install(&mut self, step: &str, over_existing: bool) -> Result<Value, String> {
    let (program, arguments) = self.as_root(vec![
      "env".into(),
      "DEBIAN_FRONTEND=noninteractive".into(),
      "dpkg".into(),
      "-i".into(),
      self.package.clone().into(),
    ]);
    self.expect_ok(step, &program, arguments, None, 15 * INSTALLER_MINUTE)?;
    self.installer_ran = true;
    let state = self.dpkg_status(step)?;
    if !state["status"]
      .as_str()
      .is_some_and(|status| status.starts_with("install ok installed"))
    {
      return Err(format!(
        "dpkg does not list the package as installed: {state}"
      ));
    }
    let listed = self.expect_ok(
      step,
      Path::new("dpkg"),
      vec!["-L".into(), self.package_name.clone().into()],
      None,
      INSTALLER_MINUTE,
    )?;
    Ok(
      json!({"how": "dpkg -i, as root", "over_an_installed_app": over_existing,
      "state": state, "paths_listed_by_dpkg": listed.lines().count()}),
    )
  }

  fn uninstall(&mut self, step: &str) -> Result<Value, String> {
    let (program, arguments) = self.as_root(vec![
      "dpkg".into(),
      "-r".into(),
      self.package_name.clone().into(),
    ]);
    self.expect_ok(step, &program, arguments, None, 15 * INSTALLER_MINUTE)?;
    Ok(
      json!({"how": "dpkg -r, as root (remove; the default, not purge)",
      "state": self.dpkg_status(step)?}),
    )
  }

  fn left_behind(&self) -> Value {
    let resources = self.installed().resource_dir;
    json!({"resource_dir": resources, "resource_dir_exists": resources.exists(),
      "names_in_it": installer_names(&resources)})
  }

  /// Recorded, not required: what `dpkg -P` does to the data folder.
  fn purge(&mut self) -> Result<Value, String> {
    let step = "purge_recorded_only";
    let (program, arguments) = self.as_root(vec![
      "dpkg".into(),
      "-P".into(),
      self.package_name.clone().into(),
    ]);
    let (code, _) = self.command(step, &program, arguments, None, 15 * INSTALLER_MINUTE)?;
    let state = self.dpkg_status(step)?;
    let data = installer_data_facts(&self.app_data);
    Ok(json!({
      "how": "dpkg -P, as root, on the installed package",
      "exit_status": code,
      "state": state,
      "program_files_gone": !self.installed().main_binary.exists(),
      "data_folder_present": self.app_data.is_dir(),
      "data_after_the_purge": match data { Ok(facts) => facts, Err(error) => json!({"error": error}) },
      "required": "nothing; this step records what purge does",
    }))
  }
}

#[cfg(target_os = "macos")]
impl InstallerGate {
  /// The app in the scratch `Applications` folder (never /Applications).
  fn app(&self) -> PathBuf {
    self
      .root
      .join("Applications")
      .join(self.package.file_name().unwrap_or_default())
  }

  /// A macOS bundle: the programs in Contents/MacOS, the resources in Contents/Resources
  /// (the app finds them as `<folder of its program>/../Resources`).
  fn installed(&self) -> Installed {
    let contents = self.app().join("Contents");
    Installed {
      location: self.app(),
      main_binary: contents.join("MacOS").join(env!("CARGO_PKG_NAME")),
      server: contents.join("MacOS").join("komika-server"),
      resource_dir: contents.join("Resources"),
    }
  }

  fn describe_package(&mut self) -> Result<Value, String> {
    let step = "package_identity";
    let plist = self.package.join("Contents").join("Info.plist");
    let mut read = serde_json::Map::new();
    for (key, expected) in [
      ("CFBundleIdentifier", self.identifier.clone()),
      ("CFBundleShortVersionString", self.version.clone()),
      ("CFBundleExecutable", env!("CARGO_PKG_NAME").to_string()),
      ("CFBundleName", self.product_name.clone()),
    ] {
      let value = self.expect_ok(
        step,
        Path::new("/usr/bin/plutil"),
        vec![
          "-extract".into(),
          key.into(),
          "raw".into(),
          "-o".into(),
          "-".into(),
          plist.clone().into(),
        ],
        None,
        INSTALLER_MINUTE,
      )?;
      if value.trim() != expected {
        return Err(format!(
          "Info.plist has {key} {:?}, expected {expected:?}",
          value.trim()
        ));
      }
      read.insert(key.into(), json!(value.trim()));
    }
    // Recorded only: an unsigned bundle (codesign then ends with a status that is not 0).
    let (code, _) = self.command(
      step,
      Path::new("/usr/bin/codesign"),
      vec![
        "-dv".into(),
        "--verbose=2".into(),
        self.package.clone().into(),
      ],
      None,
      INSTALLER_MINUTE,
    )?;
    Ok(
      json!({"info_plist": read, "codesign_display_exit_status": code,
      "codesign_text": self.commands.last().map(|command| command["stderr_end"].clone())}),
    )
  }

  fn install(&mut self, step: &str, over_existing: bool) -> Result<Value, String> {
    let app = self.app();
    let applications = self.root.join("Applications");
    fs::create_dir_all(&applications).map_err(|e| e.to_string())?;
    if app.exists() != over_existing {
      return Err(format!(
        "{} {} before this install",
        app.display(),
        if over_existing {
          "is not there"
        } else {
          "is there"
        }
      ));
    }
    self.expect_ok(
      step,
      Path::new("/usr/bin/ditto"),
      vec![self.package.clone().into(), app.into()],
      None,
      15 * INSTALLER_MINUTE,
    )?;
    self.installer_ran = true;
    Ok(json!({
      "how": if over_existing {
        "ditto of the .app over the installed one: every file is written again in place, the old bundle is not deleted first"
      } else {
        "ditto of the .app into the scratch Applications folder (a macOS install is a copy of the app; there is no installer script)"
      },
      "over_an_installed_app": over_existing,
      "applications_folder": path_fact(&applications),
    }))
  }

  fn uninstall(&mut self, step: &str) -> Result<Value, String> {
    let app = self.app();
    if !app.starts_with(&self.root) {
      return Err(format!("{} is not below the scratch root", app.display()));
    }
    let started = Instant::now();
    fs::remove_dir_all(&app).map_err(|e| format!("{}: {e}", app.display()))?;
    self.commands.push(json!({
      "step": step,
      "program": "(the test itself, std::fs::remove_dir_all)",
      "arguments": [app],
      "exit_status": 0,
      "ended_within_its_bound": true,
      "seconds": started.elapsed().as_secs_f64(),
    }));
    Ok(
      json!({"how": "the .app folder was deleted (a macOS uninstall is the removal of the app; there is no uninstaller)"}),
    )
  }

  fn left_behind(&self) -> Value {
    let applications = self.root.join("Applications");
    json!({"applications_folder": applications, "names_in_it": installer_names(&applications)})
  }
}

#[cfg(windows)]
impl InstallerGate {
  /// The folder given to the installer with `/D=`: a space and non-ASCII characters.
  fn install_dir(&self) -> PathBuf {
    self.root.join(format!("{} é 漢字", self.product_name))
  }

  /// Tauri's NSIS installer: the programs and the resources all in the install folder
  /// (the app takes the folder of its program as the resource folder).
  fn installed(&self) -> Installed {
    let dir = self.install_dir();
    Installed {
      main_binary: dir.join(concat!(env!("CARGO_PKG_NAME"), ".exe")),
      server: dir.join("komika-server.exe"),
      resource_dir: dir.clone(),
      location: dir,
    }
  }

  /// The key the installer writes for "Apps and features" and the uninstaller deletes
  /// (`UNINSTKEY` of the template; `currentUser` install mode, so below HKCU).
  fn uninstall_key(&self) -> String {
    format!(
      r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\{}",
      self.product_name
    )
  }

  /// Whether that key is there, asked without a line in the evidence.
  fn registered(&self) -> bool {
    Command::new("reg")
      .args(["query", &self.uninstall_key()])
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok_and(|status| status.success())
  }

  /// The lines of the installer script that Tauri generated for this package which
  /// decide what happens to the app's data on uninstall, each with its line number.
  fn nsis_lines(&self) -> Value {
    let Some(path) = self.header["build"]["generated_nsis_script"].as_str() else {
      return json!({"read": false, "why": "the build facts name no generated installer script"});
    };
    let bytes = match fs::read(path) {
      Ok(bytes) => bytes,
      Err(error) => return json!({"read": false, "path": path, "why": error.to_string()}),
    };
    // Tauri writes it as UTF-8 or as UTF-16 with a byte order mark.
    let text = if bytes.starts_with(&[0xFF, 0xFE]) {
      let units: Vec<u16> = bytes[2..]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
      String::from_utf16_lossy(&units)
    } else {
      String::from_utf8_lossy(&bytes).into_owned()
    };
    let wanted = [
      "!define BUNDLEID",
      "!define PRODUCTNAME",
      "!define MAINBINARYNAME",
      "!define INSTALLMODE",
      "!define UNINSTKEY",
      "DeleteAppDataCheckboxState",
      "RmDir /r \"$APPDATA",
      "RmDir /r \"$LOCALAPPDATA",
      "Function un.ConfirmLeave",
      "NSIS_HOOK_",
    ];
    let lines: Vec<Value> = text
      .lines()
      .enumerate()
      .filter(|(_, line)| wanted.iter().any(|word| line.contains(word)))
      .map(|(index, line)| json!({"line": index + 1, "text": line.trim()}))
      .collect();
    json!({"read": true, "path": path, "lines_in_the_script": text.lines().count(), "lines": lines})
  }

  fn describe_package(&mut self) -> Result<Value, String> {
    let name = self
      .package
      .file_name()
      .map(|name| name.to_string_lossy().into_owned())
      .unwrap_or_default();
    if !name.ends_with("-setup.exe") || !name.contains(&self.version) {
      return Err(format!(
        "{name} is not an NSIS installer of version {}",
        self.version
      ));
    }
    if self.registered() {
      return Err(format!(
        "{} is installed for this user already ({}). This gate does not touch an existing \
         install. Nothing was installed.",
        self.product_name,
        self.uninstall_key()
      ));
    }
    // The folder that the uninstaller's "Delete app data" choice removes is named by
    // BUNDLEID. It must be the identifier from which the app's data folder was worked out.
    let script = self.nsis_lines();
    let bundle_id = script["lines"]
      .as_array()
      .into_iter()
      .flatten()
      .filter_map(|line| line["text"].as_str())
      .find(|text| text.starts_with("!define BUNDLEID"));
    if bundle_id.is_some_and(|text| !text.contains(&format!("\"{}\"", self.identifier))) {
      return Err(format!(
        "The generated installer script has {bundle_id:?}, tauri.conf.json has the identifier {}",
        self.identifier
      ));
    }
    Ok(json!({
      "uninstall_registry_key": self.uninstall_key(),
      "uninstall_registry_key_present_before": false,
      "bundle_id_of_the_installer_is_the_identifier": bundle_id.map(|_| true),
      "generated_installer_script": script,
      "delete_app_data_in_silent_mode": "The uninstaller removes %APPDATA%\\<identifier> and %LOCALAPPDATA%\\<identifier> only when $DeleteAppDataCheckboxState is 1 and it is not an update. That variable is set in one place, un.ConfirmLeave, when the confirm page of the uninstaller is left. A silent uninstall (/S) shows no page, so the variable keeps its empty start value and the data is not removed. The lines are in generated_installer_script.",
    }))
  }

  fn install(&mut self, step: &str, over_existing: bool) -> Result<Value, String> {
    let dir = self.install_dir();
    let package = self.package.clone();
    // NSIS: `/D=` must be the last word and must not be quoted, even with spaces.
    self.expect_ok(
      step,
      &package,
      vec!["/S".into()],
      Some(&format!("/D={}", dir.display())),
      20 * INSTALLER_MINUTE,
    )?;
    self.installer_ran = true;
    if !self.registered() {
      return Err(format!(
        "After the install the registry has no {}",
        self.uninstall_key()
      ));
    }
    let key = self.uninstall_key();
    let (_, version) = self.command(
      step,
      Path::new("reg"),
      vec![
        "query".into(),
        key.into(),
        "/v".into(),
        "DisplayVersion".into(),
      ],
      None,
      INSTALLER_MINUTE,
    )?;
    Ok(json!({
      "how": "the NSIS installer in silent mode: /S /D=<folder>, the folder last and not quoted",
      "over_an_installed_app": over_existing,
      "install_folder": path_fact(&dir),
      "uninstall_registry_key_present": true,
      "display_version": version.lines().map(str::trim).find(|line| line.starts_with("DisplayVersion")),
    }))
  }

  fn uninstall(&mut self, step: &str) -> Result<Value, String> {
    let installed = self.installed();
    let uninstaller = installed.location.join("uninstall.exe");
    self.expect_ok(
      step,
      &uninstaller,
      vec!["/S".into()],
      None,
      10 * INSTALLER_MINUTE,
    )?;
    // An NSIS uninstaller copies itself to the temporary folder, starts the copy and ends
    // at once; the copy does the work. Its last acts are: the files, the registry key,
    // then (only when asked) the app data. Wait for the key to go, then a little longer.
    let started = Instant::now();
    while installed.main_binary.exists() || uninstaller.exists() || self.registered() {
      if started.elapsed() >= 5 * INSTALLER_MINUTE {
        return Err(format!(
          "Five minutes after the uninstaller was started the app is still installed: \
           program {}, uninstaller {}, registry key {}",
          installed.main_binary.exists(),
          uninstaller.exists(),
          self.registered()
        ));
      }
      std::thread::sleep(Duration::from_millis(500));
    }
    let waited = started.elapsed().as_secs_f64();
    std::thread::sleep(Duration::from_secs(10));
    Ok(json!({
      "how": "uninstall.exe /S: silent mode, no other flag, so every choice is the default",
      "seconds_until_program_uninstaller_and_registry_key_were_gone": waited,
      "seconds_waited_after_that": 10,
      "uninstall_registry_key_present": false,
    }))
  }

  fn left_behind(&self) -> Value {
    let dir = self.install_dir();
    json!({"install_folder": dir, "install_folder_exists": dir.exists(),
      "names_in_it": installer_names(&dir)})
  }
}

fn installer_run(output: &Path) -> Result<(), String> {
  let mut gate = InstallerGate::prepare(output)?;
  gate.write("running", None)?;
  let result = gate.run_steps();
  gate.write(
    if result.is_ok() { "pass" } else { "fail" },
    result.as_ref().err().map(String::as_str),
  )?;
  result
}

/// The evidence file on standard output: an upload of it can fail, the log stays.
fn installer_print(output: &Path) {
  println!("=== INSTALLER GATE EVIDENCE (JSON) BEGIN ===");
  println!(
    "{}",
    fs::read_to_string(output)
      .unwrap_or_else(|error| json!({"error": error.to_string()}).to_string())
  );
  println!("=== INSTALLER GATE EVIDENCE (JSON) END ===");
}

#[test]
#[ignore = "installs, reinstalls and removes a built package: a throwaway machine, or macOS with a scratch home"]
fn desktop_installer_gate() {
  let output = PathBuf::from(
    std::env::var_os("KOMIKA_INSTALLER_GATE_JSON")
      .expect("KOMIKA_INSTALLER_GATE_JSON output file required"),
  );
  // Windows, with administrator rights (a CI runner): as in the lifecycle gate, the run
  // is made by a second process without them, installer included, the way a person
  // without administrator rights installs for the current user.
  #[cfg(windows)]
  match rights::second_process_needed() {
    Ok(false) => {}
    Ok(true) => {
      atomic_json(
        &output,
        &json!({"gate":"desktop_installer","status":"running","steps":[],"phase":"administrator_rights"}),
      )
      .expect("replace the evidence file before the second process");
      match rights::run_second_process() {
        Ok(0) => {
          if let Err(error) = installer_evidence_complete(&output, std::env::consts::OS)
            .and_then(|()| second_process(&output))
          {
            panic!(
              "The second process ended with exit code 0, but {} does not show a passed \
               installer gate: {error}",
              output.display()
            );
          }
          return;
        }
        Ok(code) => panic!(
          "The installer gate ended with exit code {code} in the process without \
           administrator rights. Its output is above."
        ),
        Err(error) => {
          let _ = atomic_json(
            &output,
            &json!({"gate":"desktop_installer","status":"fail","phase":"administrator_rights","steps":[],"error":error}),
          );
          installer_print(&output);
          panic!("The administrator rights could not be dropped: {error}");
        }
      }
    }
    Err(error) => {
      let _ = atomic_json(
        &output,
        &json!({"gate":"desktop_installer","status":"fail","phase":"administrator_rights","steps":[],"error":error}),
      );
      installer_print(&output);
      panic!("The installer gate could not read its administrator rights: {error}");
    }
  }
  atomic_json(
    &output,
    &json!({"gate":"desktop_installer","status":"running","steps":[],"phase":"preflight"}),
  )
  .expect("write preflight JSON");
  let error = match std::panic::catch_unwind(|| installer_run(&output)) {
    Ok(Ok(())) => None,
    Ok(Err(error)) => Some(error),
    Err(panic) => Some(
      panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
        .unwrap_or_else(|| "The installer gate panicked".into()),
    ),
  };
  if let Some(error) = &error {
    // What the run wrote stays; a run that ended before it wrote its failure gets one.
    let previous = fs::read(&output)
      .ok()
      .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    if previous.as_ref().map(|record| &record["status"]) != Some(&json!("fail")) {
      atomic_json(
        &output,
        &json!({"gate":"desktop_installer","status":"fail","error":error,"previous":previous}),
      )
      .expect("write failure JSON");
    }
  }
  installer_print(&output);
  if let Some(error) = error {
    panic!("Installer gate failed; see {}: {error}", output.display());
  }
  if let Err(error) = installer_evidence_complete(&output, std::env::consts::OS) {
    panic!(
      "Installer gate ended without an error, but {} is not complete evidence: {error}",
      output.display()
    );
  }
}

/// The evidence rule of the installer gate (the workflow applies the same rule in its
/// own words): a pass without an installer run, a missing step, a step too many, the
/// steps of another system and a failed step are all refused.
#[test]
fn installer_evidence_must_be_a_pass_with_an_installer_run_and_the_expected_steps() {
  let dir = std::env::temp_dir().join(format!(
    "komiq-installer-evidence-{}-{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  fs::create_dir(&dir).unwrap();
  let file = dir.join("evidence.json");
  let check = |os: &str, evidence: Value| {
    fs::write(&file, evidence.to_string()).unwrap();
    installer_evidence_complete(&file, os)
  };
  let steps = |names: &[&str]| -> Vec<Value> {
    names
      .iter()
      .map(|name| json!({"name": name, "status": "pass"}))
      .collect()
  };
  let (linux, other) = (
    installer_expected_steps("linux"),
    installer_expected_steps("macos"),
  );
  assert_eq!((linux.len(), other.len()), (7, 6));
  assert_eq!(other, installer_expected_steps("windows"));
  let pass = |names: &[&str]| json!({"status":"pass","installer_run":true,"steps":steps(names)});
  assert_eq!(check("linux", pass(&linux)), Ok(()));
  assert_eq!(check("macos", pass(&other)), Ok(()));
  assert_eq!(check("windows", pass(&other)), Ok(()));
  // The steps of another system.
  assert!(check("linux", pass(&other)).is_err());
  assert!(check("windows", pass(&linux)).is_err());
  // No installer ran: what the lifecycle gate writes, and a pass that leaves the field out.
  let no_installer = check(
    "macos",
    json!({"status":"pass","installer_run":false,"steps":steps(&other)}),
  )
  .unwrap_err();
  assert!(no_installer.contains("installer_run"), "{no_installer}");
  assert!(check("macos", json!({"status":"pass","steps":steps(&other)})).is_err());
  // What the workflow writes before anything ran, and a run that did not finish.
  assert!(check("macos", json!({"status":"blocked","steps":[]})).is_err());
  assert!(check(
    "macos",
    json!({"status":"running","installer_run":true,"steps":steps(&other)})
  )
  .is_err());
  // One step missing, one too many, two swapped, one not passed.
  assert!(check("macos", pass(&other[..5])).is_err());
  let mut swapped = other.clone();
  swapped.swap(3, 4);
  assert!(check("macos", pass(&swapped)).is_err());
  let mut more = other.clone();
  more.push("one_more");
  assert!(check("macos", pass(&more)).is_err());
  let mut failed = steps(&other);
  failed[4]["status"] = json!("fail");
  let error = check(
    "macos",
    json!({"status":"pass","installer_run":true,"steps":failed}),
  )
  .unwrap_err();
  assert!(
    error.contains("uninstall_with_the_installers_defaults"),
    "{error}"
  );
  fs::remove_file(&file).unwrap();
  assert!(installer_evidence_complete(&file, "macos").is_err());
  fs::remove_dir(&dir).unwrap();
}

/// What "the data was kept" means in the installer gate, on a made-up data folder: the
/// same credentials, the same cluster and the same version file pass; a folder that is
/// gone, new credentials and a new cluster (what a fresh `initdb` would leave) do not.
#[test]
fn installer_data_is_kept_only_with_the_same_credentials_and_the_same_cluster() {
  let dir = std::env::temp_dir().join(format!(
    "komiq-installer-data-{}-{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  let app_data = dir.join("app.komika.desktop");
  let cluster = postgres::cluster_dir(&app_data.join("server"));
  let global = cluster.join("data").join("global");
  let make = |credentials: &str, identifier: u64| {
    fs::create_dir_all(&global).unwrap();
    fs::write(cluster.join("credentials"), credentials).unwrap();
    let mut control = identifier.to_le_bytes().to_vec();
    control.extend_from_slice(b"the rest of the control file changes at every checkpoint");
    fs::write(global.join("pg_control"), control).unwrap();
    fs::write(cluster.join("data").join("PG_VERSION"), "18\n").unwrap();
  };
  assert!(installer_data_facts(&app_data).is_err(), "no folder yet");
  make("{\"application\":\"one\"}", 7_563_219_876_543_210_987);
  let kept = installer_data_facts(&app_data).unwrap();
  assert_eq!(kept["cluster_system_identifier"], "7563219876543210987");
  assert_eq!(kept["postgresql_version_of_the_data"], "18");
  assert_eq!(kept["files"], 3);
  // The rest of the control file may change; the data is the same.
  let mut control = fs::read(global.join("pg_control")).unwrap();
  control.extend_from_slice(b" and more");
  fs::write(global.join("pg_control"), control).unwrap();
  let same = installer_data_facts(&app_data).unwrap();
  assert_eq!(installer_same_data(&kept, &same, "later"), Ok(()));
  make("{\"application\":\"two\"}", 7_563_219_876_543_210_987);
  let other_credentials = installer_data_facts(&app_data).unwrap();
  let error = installer_same_data(&kept, &other_credentials, "later").unwrap_err();
  assert!(error.contains("credentials_sha256"), "{error}");
  make("{\"application\":\"one\"}", 7_563_219_876_543_210_988);
  let other_cluster = installer_data_facts(&app_data).unwrap();
  let error = installer_same_data(&kept, &other_cluster, "later").unwrap_err();
  assert!(error.contains("cluster_system_identifier"), "{error}");
  assert!(installer_same_data(&Value::Null, &kept, "later").is_err());
  // The tree hash sees a changed file and a renamed one, and not the order of creation.
  let tree = installer_tree(&app_data).unwrap();
  assert_eq!(tree["files"], 3);
  assert_eq!(installer_tree(&app_data).unwrap(), tree);
  fs::write(cluster.join("credentials"), "{\"application\":\"0ne\"}").unwrap();
  assert_ne!(installer_tree(&app_data).unwrap()["sha256"], tree["sha256"]);
  fs::write(cluster.join("credentials"), "{\"application\":\"one\"}").unwrap();
  assert_eq!(installer_tree(&app_data).unwrap(), tree);
  fs::rename(cluster.join("credentials"), cluster.join("credentials2")).unwrap();
  assert_ne!(installer_tree(&app_data).unwrap()["sha256"], tree["sha256"]);
  fs::remove_dir_all(&dir).unwrap();
  assert!(
    installer_data_facts(&app_data).is_err(),
    "the folder is gone"
  );
}

/// The installer gate refuses, before it creates or installs anything, a scratch root
/// that exists, and on every system the request that does not fit it: the real profile
/// on macOS, a system install on Linux without the statement that the machine is a
/// throwaway one.
#[test]
fn installer_gate_refuses_before_it_creates_or_installs_anything() {
  let base = std::env::temp_dir().join(format!("komiq-installer-preflight-{}", std::process::id()));
  fs::create_dir(&base).unwrap();
  fs::write(base.join("precious"), "foreign data").unwrap();
  let output = base.with_extension("json");
  let absent = base.join("absent root");
  // (root, throwaway, real profile): each must be refused on this system.
  let mut cases = vec![(&base, cfg!(not(target_os = "macos")), cfg!(windows))];
  if cfg!(target_os = "macos") {
    cases.push((&absent, true, true));
  }
  if cfg!(not(target_os = "macos")) {
    cases.push((&absent, false, false));
  }
  for (root, throwaway, real_profile) in cases {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
      .args(["--ignored", "--exact", "desktop_installer_gate"])
      .env("KOMIKA_INSTALLER_GATE_JSON", &output)
      .env("KOMIKA_INSTALLER_GATE_ROOT", root)
      // Any file that exists: the run must end before it looks into the package.
      .env(
        "KOMIKA_INSTALLER_GATE_PACKAGE",
        std::env::current_exe().unwrap(),
      )
      .env_remove("KOMIKA_INSTALLER_GATE_BUILD_JSON")
      .env(
        "KOMIKA_INSTALLER_GATE_THROWAWAY_MACHINE",
        if throwaway { "1" } else { "0" },
      )
      .env(
        "KOMIKA_INSTALLER_GATE_REAL_PROFILE",
        if real_profile { "1" } else { "0" },
      );
    assert!(!command.output().unwrap().status.success());
    let record: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(record["status"], "fail", "{record}");
    assert_eq!(record["previous"]["phase"], "preflight", "{record}");
    assert!(!absent.exists(), "{record}");
    assert_eq!(
      fs::read_to_string(base.join("precious")).unwrap(),
      "foreign data"
    );
    assert_eq!(fs::read_dir(&base).unwrap().count(), 1);
  }
  fs::remove_dir_all(base).unwrap();
  fs::remove_file(output).unwrap();
}
