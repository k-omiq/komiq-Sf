//! The database, engine and server must not outlive the app, even when it is force-killed
//! (no `RunEvent::Exit`, no `kill_on_drop`). Each OS gets its closest reliable tool:
//!
//! * **Linux**: `PR_SET_PDEATHSIG`. The kernel sends the child SIGTERM (both the server
//!   and the JVM shut down cleanly on it) when the thread that spawned it ends. Children
//!   are spawned from the async runtime's worker threads, which live as long as the app.
//! * **Windows**: every child joins one Job Object created with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The app holds the only handle; when the app
//!   dies, the OS closes it and ends every process in the job.
//! * **macOS** (and other Unix): no kernel mechanism exists without a helper process, so
//!   children are recorded in a pid file and the next launch stops any that were left
//!   behind ([`reap_leftovers`]), before it starts new ones on the same data.
//!
//! The engine is spawned by the reader's supervisor; these hooks reach it through
//! `suwayomi::set_child_hooks`. The server is spawned in src/local.rs, PostgreSQL in
//! src/postgres.rs. PostgreSQL records are retained but delegated to Database::reconcile, which also
//! proves the data directory before stopping it. Transient tools are never recorded.
//!
//! A record that cannot be verified never authorizes a signal, and is never thrown away
//! either: its file is moved aside (`children.pid.unverified-<UTC time>`) and the owner is
//! told which process was left running and where the record is ([`init`]'s result, shown
//! in the log view and the start status by src/local.rs).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The pid file, set once by [`init`].
static PID_FILE: OnceLock<PathBuf> = OnceLock::new();

/// Names narrow the identity-checked server/engine records; they never prove ownership.
/// Only Unix reaps at the next launch (Windows ends children through the Job Object).
#[cfg(unix)]
pub const CHILD_NAMES: [&str; 3] = ["komika-server", "java", "java.exe"];

/// Set up for this run: stop children a killed previous run left behind (Unix), then
/// start recording this run's. Returns what the owner must be told: every record that
/// could not be verified (its process was left alone and the record was kept).
pub fn init(data_dir: &Path) -> Vec<String> {
  let (file, warnings) = prepare(data_dir);
  let _ = PID_FILE.set(file);
  warnings
}

/// [`init`] without installing the process-wide record file.
fn prepare(data_dir: &Path) -> (PathBuf, Vec<String>) {
  let file = data_dir.join("children.pid");
  let _ = crate::postgres::private_dir(data_dir);
  #[cfg(unix)]
  let settled = settle_leftovers(
    &file,
    &CHILD_NAMES,
    &chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
  );
  #[cfg(not(unix))]
  let settled = Settled::default();
  // Evidence that could not be moved aside stays where it is: never truncate it.
  if !settled.keep_current {
    let _ = std::fs::write(&file, "");
  }
  (file, settled.warnings)
}

/// Before `spawn`, for every child of the app.
pub fn before_spawn(cmd: &mut tokio::process::Command) {
  #[cfg(target_os = "linux")]
  linux::die_with_parent(cmd);
  #[cfg(not(target_os = "linux"))]
  let _ = cmd;
}

/// Right after `spawn`, for the children this app starts itself (the database, the
/// server, the PostgreSQL tools): tie the child to the life of the app, or end it.
///
/// Windows: the tie is the Job Object, and it is all there is. No record of a child is
/// kept there, so a child outside the job would outlive a force-quit app and nothing
/// would find it at the next launch. If the job cannot be made, set up or given this
/// child, the child is ended and collected, and the error says so: the caller starts
/// nothing further. `lasting`: also write the Unix record for the next launch (not for
/// a short-lived tool). `what`: the child, for the message ("PostgreSQL").
pub async fn after_spawn_tied(
  child: &mut tokio::process::Child,
  what: &str,
  lasting: bool,
) -> Result<(), String> {
  tied_or_ended(child, what, tie).await?;
  if lasting {
    record_spawned(child);
  }
  Ok(())
}

/// The tie of this system: the Job Object on Windows. Linux ties a child before it is
/// spawned ([`before_spawn`]); macOS has no tie (the next launch looks for what was left).
fn tie(child: &tokio::process::Child) -> Result<(), String> {
  #[cfg(windows)]
  {
    windows::adopt(child)
  }
  #[cfg(not(windows))]
  {
    let _ = child;
    Ok(())
  }
}

/// How long a child that could not be tied to the app may take to end after it was killed.
const UNTIED_END_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// [`after_spawn_tied`] with the tie passed in, so that the failure is tested on every
/// system: when `tie` fails, the child is killed and waited for, and the error names it.
async fn tied_or_ended(
  child: &mut tokio::process::Child,
  what: &str,
  tie: impl FnOnce(&tokio::process::Child) -> Result<(), String>,
) -> Result<(), String> {
  let Err(error) = tie(child) else {
    return Ok(());
  };
  // A child that has already ended by itself cannot outlive the app, and Windows does
  // not put an ended process into a job: a short tool (`postgres --version`) can be
  // done before the tie is tried. That is no failure; its exit status stays with the
  // child for whoever waits for it next.
  if matches!(child.try_wait(), Ok(Some(_))) {
    return Ok(());
  }
  let pid = child.id();
  let killed = child.start_kill();
  let ended = tokio::time::timeout(UNTIED_END_WAIT, child.wait()).await;
  let outcome = match (killed, ended) {
    (_, Ok(Ok(_))) => "It was ended again, and nothing else was started".to_string(),
    (kill, _) => format!(
      "The app tried to end it again and could not confirm that it ended{}{}; look for it \
       in Task Manager",
      pid
        .map(|pid| format!(" (process {pid})"))
        .unwrap_or_default(),
      kill.err().map(|e| format!(": {e}")).unwrap_or_default()
    ),
  };
  Err(format!(
    "{what} was started, but could not be tied to this app ({error}). Without that tie it \
     would keep running after the app is closed or force-quit. {outcome}. Retry; if this \
     happens again, restart this computer."
  ))
}

/// Right after `spawn`, for a child that another part starts through a hook and that
/// cannot be refused here (the source engine, started by the reader's code). A tie that
/// fails is a warning in the log view and the start status, and the child keeps running.
/// The app's own children go through [`after_spawn_tied`].
///
/// Unix also writes the child's record for the next launch. Windows does not: the Job
/// Object ends every child with the app, nothing ever reads a record there, and reading
/// a process's identity on Windows starts a helper program (up to 5 seconds) per child.
pub fn after_spawn(child: &tokio::process::Child) {
  if let Err(error) = tie(child) {
    warn(format!(
      "A helper process of the app{} could not be tied to it ({error}). If the app is \
       force-quit, that process keeps running until it is ended in Task Manager.",
      child
        .id()
        .map(|pid| format!(" (process {pid})"))
        .unwrap_or_default()
    ));
  }
  record_spawned(child);
}

/// Unix: the record of a lasting child for the next launch. Nothing on Windows.
fn record_spawned(child: &tokio::process::Child) {
  #[cfg(not(unix))]
  let _ = child;
  #[cfg(unix)]
  if let (Some(file), Some(pid)) = (PID_FILE.get(), child.id()) {
    match identity_after_spawn(pid) {
      Ok(Some(spawned)) => {
        if spawned.unsettled {
          // The log only: the record below still lets the next launch find the child.
          log::warn!(
            "Process {pid} still showed the program of this app {} ms after it was started, \
             so which program it runs was not recorded. Its record holds its process number \
             and creation time; the next launch looks at what it runs then.",
            SETTLE_BOUND.as_millis()
          );
        }
        record(file, &Record::from(&spawned))
      }
      other => warn(format!(
        "Could not record which program process {pid} is ({}). If the app is force-quit, \
         that process is not stopped at the next launch.",
        match other {
          Err(error) => error,
          _ => "it is already gone".into(),
        }
      )),
    }
  }
}

/// The identity of a child right after `spawn`. On Linux `spawn` can return a moment
/// before the kernel shows the child's new program: for that moment the child still
/// shows the program of this app, its parent (seen on Linux arm64, about one start in
/// ten under load). A record with that image names no child of the app and would be
/// passed over at the next launch, so the look is repeated until the image is another
/// one. An exec takes far less than the bound. A child that still shows the app's
/// program at the bound is given back as `unsettled`.
#[cfg(unix)]
pub(crate) fn identity_after_spawn(pid: u32) -> Result<Option<Spawned>, String> {
  let own = std::env::current_exe().ok();
  settle(own.as_deref(), SETTLE_BOUND, || {
    crate::postgres::process::identity(pid)
  })
}

/// How long [`identity_after_spawn`] waits for a child to show its own program.
#[cfg(unix)]
const SETTLE_BOUND: std::time::Duration = std::time::Duration::from_millis(250);

/// What the look right after `spawn` showed of a child.
#[cfg(any(unix, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spawned {
  pub identity: crate::postgres::process::Identity,
  /// The child still showed the program of this app when the wait ended. Its image is
  /// then not the program it is going to run, and must not be recorded as that.
  pub unsettled: bool,
}

/// Repeat `read` while it shows a live process whose image is `own`, at most for `bound`.
#[cfg(any(unix, test))]
fn settle(
  own: Option<&Path>,
  bound: std::time::Duration,
  mut read: impl FnMut() -> Result<Option<crate::postgres::process::Identity>, String>,
) -> Result<Option<Spawned>, String> {
  let deadline = std::time::Instant::now() + bound;
  loop {
    let seen = read()?;
    let unsettled = matches!(&seen, Some(identity) if own == Some(identity.image.as_path()));
    if !unsettled || std::time::Instant::now() >= deadline {
      return Ok(seen.map(|identity| Spawned {
        identity,
        unsettled,
      }));
    }
    std::thread::sleep(std::time::Duration::from_millis(1));
  }
}

/// Clean exit: every child was stopped, so nothing is left to reap next time.
pub fn forget_all() {
  if let Some(file) = PID_FILE.get() {
    let _ = std::fs::write(file, "");
  }
}

/// Warnings raised outside [`init`] (a child whose identity could not be recorded).
/// The local server (src/local.rs) takes them into the log view and the start status.
static WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub(crate) fn warn(text: String) {
  log::warn!("{text}");
  let mut pending = WARNINGS.lock().unwrap_or_else(|e| e.into_inner());
  if !pending.contains(&text) && pending.len() < 50 {
    pending.push(text);
  }
}

/// Serializes the tests (here and in src/local/tests.rs) that read the queue back.
#[cfg(test)]
pub(crate) static WARNING_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Hand over the warnings raised since the last call.
pub fn take_warnings() -> Vec<String> {
  std::mem::take(&mut *WARNINGS.lock().unwrap_or_else(|e| e.into_inner()))
}

// Never persist argv: engine/server arguments may contain secrets.
#[cfg(any(unix, test))]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Record {
  pid: u32,
  /// The program the child runs. Empty in an `unsettled` record.
  image: PathBuf,
  created: String,
  /// The child had not shown its own program yet when the record was written (see
  /// [`identity_after_spawn`]): which program it runs is not in the record. The next
  /// launch takes such a record by process number and creation time, and looks at the
  /// program the process runs then. Left out of the line when false, so that a settled
  /// record reads as it always did.
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  unsettled: bool,
}
#[cfg(any(unix, test))]
impl From<&crate::postgres::process::Identity> for Record {
  fn from(value: &crate::postgres::process::Identity) -> Self {
    Self {
      pid: value.pid,
      image: value.image.clone(),
      created: value.created.clone(),
      unsettled: false,
    }
  }
}
#[cfg(any(unix, test))]
impl From<&Spawned> for Record {
  fn from(value: &Spawned) -> Self {
    Self {
      // What an unsettled child showed is the program of this app, not its own.
      image: if value.unsettled {
        PathBuf::new()
      } else {
        value.identity.image.clone()
      },
      unsettled: value.unsettled,
      ..Self::from(&value.identity)
    }
  }
}
#[cfg(any(unix, test))]
impl Record {
  fn matches(&self, current: &crate::postgres::process::Identity) -> bool {
    self.pid > 1
      && self.pid == current.pid
      && self.image == current.image
      && self.created == current.created
      && !self.created.is_empty()
      && self.image.is_absolute()
  }
}

#[cfg(any(unix, test))]
fn record(file: &Path, value: &Record) {
  use std::io::Write;
  if let Ok(mut f) = std::fs::OpenOptions::new()
    .create(true)
    .append(true)
    .open(file)
  {
    if let Ok(line) = serde_json::to_string(value) {
      let _ = writeln!(f, "{line}");
    }
  }
}

/// A record whose process is (or may be) still running and could not be proven to be
/// this app's orphan. Its process was not signaled.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
  /// The record exactly as stored.
  pub line: String,
  pub pid: Option<u32>,
  pub image: Option<PathBuf>,
  pub reason: String,
}

/// What one pass over a record file did.
#[cfg(unix)]
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reaped {
  pub stopped: Vec<u32>,
  pub unresolved: Vec<Unresolved>,
}

/// What [`init`] must act on after the previous run's records were examined.
#[derive(Debug, Default)]
struct Settled {
  warnings: Vec<String>,
  /// The current record file still holds unverified records (it could not be moved).
  keep_current: bool,
  #[cfg(unix)]
  stopped: Vec<u32>,
  #[cfg(unix)]
  aside: Option<PathBuf>,
}

/// Examine the previous run's records, and every record file an earlier launch moved
/// aside. A file with a record that cannot be verified is never emptied or deleted: the
/// current one is renamed to `children.pid.unverified-<UTC time>` and kept until a later
/// launch can verify every record in it.
#[cfg(unix)]
fn settle_leftovers(file: &Path, names: &[&str], stamp: &str) -> Settled {
  let mut settled = Settled::default();
  let dir = file.parent().unwrap_or(Path::new("."));
  let mut earlier: Vec<PathBuf> = std::fs::read_dir(dir)
    .map(|entries| {
      entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
          path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(ASIDE_PREFIX))
        })
        .collect()
    })
    .unwrap_or_default();
  earlier.sort();
  for aside in earlier {
    let result = reap_leftovers(&aside, names);
    settled.stopped.extend(result.stopped);
    if result.unresolved.is_empty() {
      // Every record in it is verified now (stopped, gone, or its pid reused).
      let _ = std::fs::remove_file(&aside);
    } else {
      for item in &result.unresolved {
        settled.warnings.push(describe(item, &aside));
      }
    }
  }
  let result = reap_leftovers(file, names);
  settled.stopped.extend(result.stopped);
  if !result.unresolved.is_empty() {
    let mut aside = dir.join(format!("{ASIDE_PREFIX}{stamp}"));
    let mut n = 1;
    while aside.exists() {
      aside = dir.join(format!("{ASIDE_PREFIX}{stamp}-{n}"));
      n += 1;
    }
    let kept = match std::fs::rename(file, &aside) {
      Ok(()) => {
        settled.aside = Some(aside.clone());
        aside
      }
      Err(error) => {
        settled.keep_current = true;
        settled.warnings.push(format!(
          "The record of the last run's processes could not be moved aside ({error}); it is \
           kept in place at {}.",
          file.display()
        ));
        file.to_path_buf()
      }
    };
    for item in &result.unresolved {
      settled.warnings.push(describe(item, &kept));
    }
  }
  for warning in &settled.warnings {
    log::warn!("{warning}");
  }
  settled
}

#[cfg(unix)]
const ASIDE_PREFIX: &str = "children.pid.unverified-";

#[cfg(unix)]
fn describe(item: &Unresolved, kept: &Path) -> String {
  let process = match (&item.pid, &item.image) {
    (Some(pid), Some(image)) => format!("process {pid} ({})", image.display()),
    (Some(pid), None) => format!("process {pid}"),
    _ => "a process".into(),
  };
  format!(
    "A program from an earlier run may still be running and was left alone: {process}. \
     {}. If it is an old komiq server or source engine, quit it with your system's \
     process monitor{}, then start komiq again. Its record is kept at {}.",
    item.reason,
    item
      .pid
      .map(|pid| format!(" (or `kill {pid}`)"))
      .unwrap_or_default(),
    kept.display()
  )
}

/// Stop the identity-verified orphans recorded in `file`; report every record that could
/// not be verified. Legacy PID-only or unverifiable records never authorize a signal.
/// PostgreSQL is handled separately by Database::reconcile with the additional
/// data-directory proof, so records of other program names are passed over.
#[cfg(unix)]
pub fn reap_leftovers(file: &Path, names: &[&str]) -> Reaped {
  reap_leftovers_with(
    file,
    names,
    &Look {
      identity: &crate::postgres::process::identity,
      parent: &parent_of,
      signal: &signal,
      grace: std::time::Duration::from_secs(5),
    },
  )
}

/// How the look at the last run's children reads and signals a process: the system, or
/// made-up answers in the tests.
#[cfg(unix)]
struct Look<'a> {
  identity: &'a dyn Fn(u32) -> Result<Option<crate::postgres::process::Identity>, String>,
  parent: &'a dyn Fn(u32) -> Option<u32>,
  signal: &'a dyn Fn(u32, &str),
  /// How long a process gets to end after a signal.
  grace: std::time::Duration,
}

/// What a recorded orphan is at this moment.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum Standing {
  /// Provably the recorded process, and a child of init: it may be signaled.
  Ours,
  /// No process has its number any more, or one that was created at another time.
  Ended,
  /// It may still run, and it is not proven to be the recorded orphan: why, as a clause.
  Unknown(String),
}

/// The one rule for what a recorded orphan is now. Every look of `reap_leftovers_with`
/// goes through it: the first one, the one right before a signal, and the wait after it.
#[cfg(unix)]
fn standing(expected: &Record, look: &Look) -> Standing {
  if expected.pid <= 1 || expected.created.is_empty() || !expected.image.is_absolute() {
    return Standing::Unknown("its record is incomplete".into());
  }
  let read = || match (look.identity)(expected.pid) {
    Ok(None) => Err(Standing::Ended),
    Ok(Some(current)) if current.created != expected.created => Err(Standing::Ended),
    Ok(Some(current)) => Ok(current),
    Err(error) => Err(Standing::Unknown(format!(
      "the process could not be inspected ({error})"
    ))),
  };
  let current = match read() {
    Ok(current) => current,
    Err(standing) => return standing,
  };
  if !expected.matches(&current) {
    // Same process, other program file: e.g. the app was updated while it ran.
    return Standing::Unknown(format!(
      "its program file is now reported as {}, not the one that was started",
      current.image.display()
    ));
  }
  match (look.parent)(expected.pid) {
    Some(1) => Standing::Ours,
    Some(parent) => Standing::Unknown(format!(
      "it is still attached to another process ({parent}), so it is not provably left over"
    )),
    // No answer: it ended between the two readings, or its parent could not be read.
    // Only the first is an end, and only a second reading of the process shows it.
    None => match read() {
      Err(standing) => standing,
      Ok(_) => Standing::Unknown("its parent process could not be read".into()),
    },
  }
}

/// A reason clause of `Standing::Unknown` as a sentence of its own.
#[cfg(unix)]
fn sentence(clause: &str) -> String {
  let mut letters = clause.chars();
  match letters.next() {
    Some(first) => first.to_uppercase().chain(letters).collect(),
    None => String::new(),
  }
}

/// Wait until the recorded orphan has ended, for at most the grace of `look`, and give
/// back what it is then. Only `Ended` is a proven end: a process that can no longer be
/// read has not been shown to be gone, so the wait goes on, and the last answer says so.
#[cfg(unix)]
fn wait_for_end(expected: &Record, look: &Look) -> Standing {
  let deadline = std::time::Instant::now() + look.grace;
  loop {
    let now = standing(expected, look);
    if now == Standing::Ended || std::time::Instant::now() >= deadline {
      return now;
    }
    std::thread::sleep(std::time::Duration::from_millis(100).min(look.grace));
  }
}

#[cfg(unix)]
fn reap_leftovers_with(file: &Path, names: &[&str], look: &Look) -> Reaped {
  let mut reaped = Reaped::default();
  let Ok(text) = std::fs::read_to_string(file) else {
    return reaped;
  };
  for line in text.lines().filter(|line| !line.trim().is_empty()) {
    let mut unresolved = |pid: Option<u32>, image: Option<PathBuf>, reason: String| {
      reaped.unresolved.push(Unresolved {
        line: line.to_string(),
        pid,
        image,
        reason,
      })
    };
    let expected = match serde_json::from_str::<Record>(line) {
      Ok(expected) => expected,
      Err(_) => {
        // An older app wrote bare pids. A pid alone proves nothing about a live process.
        match line.trim().parse::<u32>() {
          Ok(pid) => match (look.identity)(pid) {
            Ok(None) => {} // gone: nothing to stop, nothing to keep
            Ok(Some(current)) => unresolved(
              Some(pid),
              Some(current.image),
              "Its record comes from an older app version and cannot prove whose process this is"
                .into(),
            ),
            Err(error) => unresolved(
              Some(pid),
              None,
              format!("Its record comes from an older app version and the process could not be inspected ({error})"),
            ),
          },
          Err(_) => unresolved(None, None, "Its record could not be read".into()),
        }
        continue;
      }
    };
    let named = |image: &Path| {
      image
        .file_name()
        .is_some_and(|base| names.iter().any(|n| base == *n))
    };
    let expected = if expected.unsettled {
      // The record does not say which program the child runs. The process number and the
      // creation time say whether it is still that process; the program it runs now
      // decides the rest. Such a record is never passed over without a word.
      const UNSETTLED: &str =
        "Its record was written before the system showed which program it runs";
      if expected.pid <= 1 || expected.created.is_empty() {
        unresolved(Some(expected.pid), None, "Its record is incomplete".into());
        continue;
      }
      match (look.identity)(expected.pid) {
        Ok(None) => continue,                                                 // gone
        Ok(Some(current)) if current.created != expected.created => continue, // pid reused
        Ok(Some(current)) if named(&current.image) => Record::from(&current),
        // PostgreSQL: handled by Database::reconcile, as for a settled record.
        Ok(Some(current)) if crate::postgres::process::postgres_like(&current.image) => continue,
        Ok(Some(current)) => {
          unresolved(
            Some(expected.pid),
            Some(current.image),
            format!("{UNSETTLED}, and what it runs now is not a program this app stops"),
          );
          continue;
        }
        Err(error) => {
          unresolved(
            Some(expected.pid),
            None,
            format!("{UNSETTLED}, and the process could not be inspected ({error})"),
          );
          continue;
        }
      }
    } else if named(&expected.image) {
      expected
    } else {
      continue;
    };
    match standing(&expected, look) {
      Standing::Ours => {}
      Standing::Ended => continue,
      // Possibly this app's child, but not provably: left alone, record kept.
      Standing::Unknown(why) => {
        unresolved(
          Some(expected.pid),
          Some(expected.image.clone()),
          sentence(&why),
        );
        continue;
      }
    }
    // Right before the signal, once more: only a process that is proven to be this
    // orphan now is signaled.
    match standing(&expected, look) {
      Standing::Ours => {}
      Standing::Ended => continue, // it ended between the two looks
      Standing::Unknown(why) => {
        unresolved(
          Some(expected.pid),
          Some(expected.image.clone()),
          format!("When it was to be asked to stop, {why}; nothing was sent to it"),
        );
        continue;
      }
    }
    log::warn!(
      "stopping an identity-verified child left by the last run (pid {})",
      expected.pid
    );
    (look.signal)(expected.pid, "-TERM");
    let mut now = wait_for_end(&expected, look);
    if now == Standing::Ours {
      (look.signal)(expected.pid, "-KILL");
      now = wait_for_end(&expected, look);
    }
    // Only a proven end is "stopped". A process that can no longer be read after the
    // signal may still run: its record is kept and it is reported.
    match now {
      Standing::Ended => reaped.stopped.push(expected.pid),
      Standing::Ours => unresolved(
        Some(expected.pid),
        Some(expected.image.clone()),
        "It did not stop when asked".into(),
      ),
      Standing::Unknown(why) => unresolved(
        Some(expected.pid),
        Some(expected.image.clone()),
        format!("It was asked to stop, and it is not known whether it stopped: {why}"),
      ),
    }
  }
  reaped
}

/// Only the numeric parent is parsed. Image paths come from native inspection and never
/// pass through whitespace splitting (installed paths contain spaces).
#[cfg(unix)]
fn parent_of(pid: u32) -> Option<u32> {
  let out = std::process::Command::new("ps")
    .args(["-o", "ppid=", "-p", &pid.to_string()])
    .output()
    .ok()?;
  if !out.status.success() {
    return None;
  }
  String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Tests: whether the recorded process is, on this system and now, this app's orphan.
#[cfg(all(unix, test))]
fn is_owned_orphan(expected: &Record) -> bool {
  let Ok(Some(current)) = crate::postgres::process::identity(expected.pid) else {
    return false;
  };
  expected.matches(&current) && parent_of(expected.pid) == Some(1)
}

#[cfg(unix)]
fn signal(pid: u32, sig: &str) {
  let _ = std::process::Command::new("kill")
    .args([sig, &pid.to_string()])
    .stderr(std::process::Stdio::null())
    .status();
}

/// Tests: whether `pid` is a process that still runs. A process that has ended and was
/// not collected by its parent (in a container the first process often collects nothing)
/// still answers `kill -0`; it is not alive.
#[cfg(all(unix, test))]
fn alive(pid: u32) -> bool {
  use crate::postgres::process::{holder, Holder};
  !matches!(holder(pid), Holder::Free | Holder::Ended)
}

/// Tests: `pid` stops being a running process within a few seconds. A process that was
/// just told to end is, for a moment, neither running a program nor ended yet.
#[cfg(all(unix, test))]
fn ends_soon(pid: u32) -> bool {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
  while alive(pid) && std::time::Instant::now() < deadline {
    std::thread::sleep(std::time::Duration::from_millis(10));
  }
  !alive(pid)
}

/// Tests: start `command`, trying again for a moment while Linux answers "Text file
/// busy" (ETXTBSY). That happens to a program or script file that a test has just
/// written when another test thread forked at the same moment: the forked child still
/// holds the file open for writing until it runs its own program.
#[cfg(all(unix, test))]
pub(crate) fn spawn_when_ready(command: &mut std::process::Command) -> std::process::Child {
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
  loop {
    match command.spawn() {
      Ok(child) => return child,
      Err(error)
        if error.raw_os_error() == Some(libc::ETXTBSY) && std::time::Instant::now() < deadline =>
      {
        std::thread::sleep(std::time::Duration::from_millis(20));
      }
      Err(error) => panic!("could not start {command:?}: {error}"),
    }
  }
}

/// Tests: this test binary, copied to `image` (a program of the test's own at that
/// path), and proven startable. The copy is written under another name, closed and
/// renamed; then it is run once (`--list`, which only prints) with the ETXTBSY wait of
/// [`spawn_when_ready`]. After that no process holds it open for writing, so a later
/// start cannot meet "Text file busy", also one made through a shell.
#[cfg(all(unix, test))]
pub(crate) fn install_test_program(image: &Path) {
  static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
  std::fs::create_dir_all(image.parent().unwrap()).unwrap();
  let temporary = image.with_file_name(format!(
    ".installing-{}-{}",
    std::process::id(),
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
  ));
  std::fs::copy(std::env::current_exe().unwrap(), &temporary).unwrap();
  std::fs::rename(&temporary, image).unwrap();
  let listed = spawn_when_ready(
    std::process::Command::new(image)
      .arg("--list")
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null()),
  )
  .wait()
  .unwrap();
  assert!(listed.success(), "{} does not run", image.display());
}

#[cfg(target_os = "linux")]
mod linux {
  /// Ask the kernel for SIGTERM when the spawning thread ends. If the app already died
  /// between fork and this call (the parent changed), exit instead of running orphaned.
  pub fn die_with_parent(cmd: &mut tokio::process::Command) {
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe calls (prctl, getppid) between fork and exec.
    unsafe {
      cmd.pre_exec(move || {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
          return Err(std::io::Error::last_os_error());
        }
        if libc::getppid() != parent {
          return Err(std::io::Error::other(
            "the app exited while starting a child",
          ));
        }
        Ok(())
      });
    }
  }
}

#[cfg(windows)]
mod windows {
  use std::sync::OnceLock;
  use windows_sys::Win32::Foundation::HANDLE;
  use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
  };

  /// The job's handle as an integer (raw handles are not `Sync`). Never closed: the OS
  /// closes it when the app's process ends, which is what kills the children.
  static JOB: OnceLock<Result<usize, String>> = OnceLock::new();

  /// The job, or why there is none. A job that could not be made or set up is not
  /// tried again in this run of the app.
  fn job() -> Result<HANDLE, String> {
    let raw = JOB.get_or_init(|| {
      // SAFETY: plain Win32 calls with a zeroed POD struct of the right size.
      unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
          let error = std::io::Error::last_os_error();
          return Err(format!("the Job Object could not be created: {error}"));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
          job,
          JobObjectExtendedLimitInformation,
          &info as *const _ as *const core::ffi::c_void,
          std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
          let error = std::io::Error::last_os_error();
          return Err(format!("the Job Object could not be set up: {error}"));
        }
        Ok(job as usize)
      }
    });
    raw.clone().map(|h| h as HANDLE)
  }

  /// Put `child` into the job that ends with the app. `Err`: it is not in the job.
  pub fn adopt(child: &tokio::process::Child) -> Result<(), String> {
    let job = job()?;
    let process = child
      .raw_handle()
      .ok_or("the process has no handle any more (it was already collected)")?;
    // SAFETY: both handles are valid for the duration of the call.
    if unsafe { AssignProcessToJobObject(job, process as HANDLE) } == 0 {
      let error = std::io::Error::last_os_error();
      return Err(format!(
        "the process could not be put into the Job Object: {error}"
      ));
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("komiq-child-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
  }

  /// The process-wide record file, installed once for this test binary and never
  /// removed while it runs. Tests that read it back hold [`REGISTRY_LOCK`].
  fn registry() -> PathBuf {
    static FILE: OnceLock<PathBuf> = OnceLock::new();
    FILE
      .get_or_init(|| {
        init(&tmp("registry"));
        PID_FILE
          .get()
          .expect("init installs the record file")
          .clone()
      })
      .clone()
  }
  static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

  /// Not a test: the long-lived child of the reaper tests. macOS kills a copy of a
  /// system program such as `/bin/sleep`, so the tests copy this test binary to the
  /// path they need and run this function in it.
  #[test]
  #[ignore = "helper process of the reaper tests"]
  fn helper_sleep() {
    if let Ok(seconds) = std::env::var(HELPER_SLEEP) {
      std::thread::sleep(std::time::Duration::from_secs(seconds.parse().unwrap()));
    }
  }
  const HELPER_SLEEP: &str = "KOMIQ_CHILD_TEST_SLEEP";
  #[cfg(unix)]
  const HELPER_ARGS: [&str; 4] = [
    "--ignored",
    "--exact",
    "child::tests::helper_sleep",
    "--nocapture",
  ];

  /// This test binary, copied to `image` (a program of the test's own at that path).
  #[cfg(unix)]
  fn install_helper(image: &Path) {
    install_test_program(image);
  }

  /// A live child of this test process running the helper at `image`.
  #[cfg(unix)]
  fn spawn_helper(image: &Path) -> std::process::Child {
    spawn_when_ready(
      std::process::Command::new(image)
        .args(HELPER_ARGS)
        .env(HELPER_SLEEP, "60")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()),
    )
  }

  /// `program` started by a shell that has exited, i.e. re-parented like an app child
  /// after a force-kill. Returns its pid.
  #[cfg(unix)]
  fn orphan(program: &Path, args: &[&str]) -> u32 {
    let out = std::process::Command::new("sh")
      .args(["-c", "\"$0\" \"$@\" >/dev/null 2>&1 & echo $!"])
      .arg(program)
      .args(args)
      .env(HELPER_SLEEP, "60")
      .output()
      .unwrap();
    let pid = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    // Give the kernel a moment to re-parent it and to finish the exec.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
      let execed = crate::postgres::process::identity(pid)
        .ok()
        .flatten()
        .is_some_and(|i| i.image.file_name() == program.file_name());
      if execed && parent_of(pid) == Some(1) {
        break;
      }
      std::thread::sleep(std::time::Duration::from_millis(20));
    }
    pid
  }

  #[cfg(unix)]
  fn record_of(pid: u32) -> Record {
    let spawned = identity_after_spawn(pid).unwrap().unwrap();
    assert!(!spawned.unsettled, "{spawned:?}");
    Record::from(&spawned)
  }

  /// A child that was just spawned may show its parent's program for a moment (Linux).
  /// The record must wait for the child's own program, and must not wait for ever.
  #[test]
  fn a_child_record_waits_until_the_child_shows_its_own_program() {
    use crate::postgres::process::Identity;
    let shows = |image: &str| Identity {
      pid: 4242,
      image: PathBuf::from(image),
      created: "1:2".into(),
      args: vec![],
    };
    let own = Path::new("/opt/komiq/komiq-desktop");
    let bound = std::time::Duration::from_secs(5);
    // Twice the app's own program (the exec is not done), then the child's.
    let mut looks = 0;
    let seen = settle(Some(own), bound, || {
      looks += 1;
      Ok(Some(shows(if looks < 3 {
        "/opt/komiq/komiq-desktop"
      } else {
        "/opt/komiq/komika-server"
      })))
    });
    assert_eq!(looks, 3);
    assert_eq!(
      seen,
      Ok(Some(Spawned {
        identity: shows("/opt/komiq/komika-server"),
        unsettled: false
      }))
    );
    // A child that is gone, or cannot be read, is reported at once.
    let mut looks = 0;
    assert_eq!(
      settle(Some(own), bound, || {
        looks += 1;
        Ok(None)
      }),
      Ok(None)
    );
    let unreadable = settle(Some(own), bound, || {
      looks += 1;
      Err("no".into())
    });
    assert_eq!((looks, unreadable), (2, Err("no".into())));
    // A child that really runs the app's own program: the wait ends at the bound.
    let started = std::time::Instant::now();
    let same = settle(Some(own), std::time::Duration::from_millis(30), || {
      Ok(Some(shows("/opt/komiq/komiq-desktop")))
    });
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    // It is given back as unsettled, and its record does not name the app's program as
    // the child's: no image, the mark, and the process number and creation time.
    let same = same.unwrap().unwrap();
    assert!(same.unsettled, "{same:?}");
    let record = Record::from(&same);
    assert_eq!(
      (
        record.pid,
        record.image.as_os_str().is_empty(),
        record.unsettled
      ),
      (4242, true, true)
    );
    assert_eq!(record.created, "1:2");
    let line = serde_json::to_string(&record).unwrap();
    assert!(line.contains("\"unsettled\":true"), "{line}");
    assert!(!line.contains("komiq-desktop"), "{line}");
    assert!(serde_json::from_str::<Record>(&line).unwrap().unsettled);
    // A settled record is written as before, without the mark, and reads as settled.
    let settled = serde_json::to_string(&Record::from(&shows("/opt/komiq/komika-server"))).unwrap();
    assert!(!settled.contains("unsettled"), "{settled}");
    assert!(!serde_json::from_str::<Record>(&settled).unwrap().unsettled);
  }

  /// A record written before the child showed its own program is not passed over at the
  /// next launch: the process is found by its number and creation time, and what it runs
  /// then decides. (Before, such a record held the app's own program as the child's and
  /// was passed over without a word, so the orphan kept running.)
  #[cfg(unix)]
  #[test]
  fn an_unsettled_record_is_taken_by_process_number_and_creation_time() {
    let dir = tmp("unsettled");
    let file = dir.join("children.pid");
    let pid = orphan(Path::new("/bin/sleep"), &["60"]);
    let identity = crate::postgres::process::identity(pid).unwrap().unwrap();
    // As `after_spawn` records a child that still showed the app's program at the bound.
    let unsettled = Record::from(&Spawned {
      identity: crate::postgres::process::Identity {
        image: std::env::current_exe().unwrap(),
        ..identity.clone()
      },
      unsettled: true,
    });
    record(&file, &unsettled);
    // The same process number with another creation time: that process has ended.
    record(
      &file,
      &Record {
        created: format!("{}0", unsettled.created),
        ..Record::from(&Spawned {
          identity: identity.clone(),
          unsettled: true,
        })
      },
    );

    // It runs a program this app does not stop: reported, never signaled, never silent.
    let other = reap_leftovers(&file, &["komika-server"]);
    assert!(other.stopped.is_empty(), "{other:?}");
    assert_eq!(other.unresolved.len(), 1, "{other:?}");
    assert_eq!(other.unresolved[0].pid, Some(pid));
    assert_eq!(
      other.unresolved[0].image.as_deref(),
      Some(identity.image.as_path())
    );
    assert!(
      other.unresolved[0]
        .reason
        .contains("before the system showed which program it runs"),
      "{other:?}"
    );
    assert!(alive(pid));

    // It runs one of the app's children: stopped like a settled record's orphan.
    let reparented_to_init = parent_of(pid) == Some(1);
    let result = reap_leftovers(&file, &["sleep"]);
    if reparented_to_init {
      assert_eq!(result.stopped, vec![pid], "{result:?}");
      assert!(result.unresolved.is_empty(), "{result:?}");
      assert!(
        ends_soon(pid),
        "the orphan of an unsettled record was stopped"
      );
    } else {
      // A subreaper adopted it: reported, not stopped.
      assert!(result.stopped.is_empty());
      assert!(result.unresolved.iter().any(|u| u.pid == Some(pid)));
      signal(pid, "-KILL");
    }
    // Gone now: nothing to stop, nothing to keep.
    assert!(ends_soon(pid));
    assert_eq!(reap_leftovers(&file, &["sleep"]), Reaped::default());
    let _ = std::fs::remove_dir_all(dir);
  }

  /// Round 9: after the stop signal, a child that could no longer be inspected was
  /// listed as stopped and its record was dropped, although nothing showed that it had
  /// ended. Only a proven end is "stopped". The process is made up here (its readings
  /// and the signals are passed in), so nothing depends on timing.
  #[cfg(unix)]
  #[test]
  fn only_a_proven_end_counts_as_stopped_after_the_stop_signal() {
    use crate::postgres::process::Identity;
    use std::cell::{Cell, RefCell};
    const PID: u32 = 4242;
    let dir = tmp("proven-end");
    let file = dir.join("children.pid");
    let image = PathBuf::from("/opt/komiq/komika-server");
    let recorded = Identity {
      pid: PID,
      image: image.clone(),
      created: "boot-1:5000".into(),
      args: Vec::new(),
    };
    record(&file, &Record::from(&recorded));
    // `after`: what the process is after the n-th signal (0: before any). `parent`: what
    // its parent is said to be then (`None`: the parent could not be read).
    type Reading = Result<Option<Identity>, String>;
    let run_with = |after: &dyn Fn(usize) -> Reading, parent: &dyn Fn(usize) -> Option<u32>| {
      let signals = RefCell::new(Vec::<String>::new());
      let readings = Cell::new(0usize);
      let reaped = reap_leftovers_with(
        &file,
        &["komika-server"],
        &Look {
          identity: &|pid| {
            assert_eq!(pid, PID);
            readings.set(readings.get() + 1);
            after(signals.borrow().len())
          },
          parent: &|pid| {
            assert_eq!(pid, PID);
            parent(signals.borrow().len())
          },
          signal: &|pid, signal| {
            assert_eq!(pid, PID);
            signals.borrow_mut().push(signal.to_string());
          },
          grace: std::time::Duration::ZERO,
        },
      );
      (reaped, signals.into_inner(), readings.get())
    };
    let run = |after: &dyn Fn(usize) -> Reading| {
      let (reaped, signals, readings) = run_with(after, &|_| Some(1));
      assert!(readings >= 2, "looked at before and after the signal");
      (reaped, signals)
    };
    let alive = || Ok(Some(recorded.clone()));
    let one_unresolved = |reaped: &Reaped| {
      assert!(reaped.stopped.is_empty(), "{reaped:?}");
      assert_eq!(reaped.unresolved.len(), 1, "{reaped:?}");
      assert_eq!(reaped.unresolved[0].pid, Some(PID));
      assert_eq!(reaped.unresolved[0].image.as_deref(), Some(image.as_path()));
      reaped.unresolved[0].reason.clone()
    };

    // It ends after the stop signal: stopped.
    let (reaped, signals) = run(&|sent| if sent == 0 { alive() } else { Ok(None) });
    assert_eq!(reaped.stopped, vec![PID]);
    assert!(reaped.unresolved.is_empty(), "{reaped:?}");
    assert_eq!(signals, ["-TERM"]);
    // Its number belongs to a process created at another time: it ended as well.
    let (reaped, signals) = run(&|sent| {
      if sent == 0 {
        alive()
      } else {
        Ok(Some(Identity {
          created: "boot-1:9000".into(),
          ..recorded.clone()
        }))
      }
    });
    assert_eq!(reaped.stopped, vec![PID]);
    assert_eq!(signals, ["-TERM"]);
    // It ignores the stop signal and ends after the kill signal.
    let (reaped, signals) = run(&|sent| if sent < 2 { alive() } else { Ok(None) });
    assert_eq!(reaped.stopped, vec![PID]);
    assert_eq!(signals, ["-TERM", "-KILL"]);
    // It ends after neither.
    let (reaped, signals) = run(&|_| alive());
    assert_eq!(one_unresolved(&reaped), "It did not stop when asked");
    assert_eq!(signals, ["-TERM", "-KILL"]);

    // The finding: after the stop signal it can no longer be inspected. That is not an
    // end. It is reported, its record stays, and it gets no kill signal (it is not
    // proven to be this app's process any more).
    let (reaped, signals) = run(&|sent| {
      if sent == 0 {
        alive()
      } else {
        Err("Cannot inspect process 4242: Permission denied (os error 13)".into())
      }
    });
    assert_eq!(
      one_unresolved(&reaped),
      "It was asked to stop, and it is not known whether it stopped: the process could \
       not be inspected (Cannot inspect process 4242: Permission denied (os error 13))"
    );
    assert_eq!(signals, ["-TERM"]);
    // The same when it shows another program after the stop signal.
    let (reaped, signals) = run(&|sent| {
      if sent == 0 {
        alive()
      } else {
        Ok(Some(Identity {
          image: "/usr/bin/other".into(),
          ..recorded.clone()
        }))
      }
    });
    assert_eq!(
      one_unresolved(&reaped),
      "It was asked to stop, and it is not known whether it stopped: its program file is \
       now reported as /usr/bin/other, not the one that was started"
    );
    assert_eq!(signals, ["-TERM"]);
    // And when it cannot be inspected right before the first signal (the first look
    // could read it): nothing is sent, and it is reported, not passed over.
    let looks = Cell::new(0usize);
    let (reaped, signals) = run(&|_| {
      looks.set(looks.get() + 1);
      if looks.get() == 1 {
        alive()
      } else {
        Err("Operation not permitted (os error 1)".into())
      }
    });
    assert_eq!(
      one_unresolved(&reaped),
      "When it was to be asked to stop, the process could not be inspected (Operation \
       not permitted (os error 1)); nothing was sent to it"
    );
    assert!(signals.is_empty(), "{signals:?}");
    // It ended between the first look and the signal: nothing to stop, nothing to keep.
    let looks = Cell::new(0usize);
    let (reaped, signals) = run(&|_| {
      looks.set(looks.get() + 1);
      if looks.get() == 1 {
        alive()
      } else {
        Ok(None)
      }
    });
    assert_eq!(reaped, Reaped::default());
    assert!(signals.is_empty());

    // The parent. Round 10: at the first look, a parent that could not be read (the
    // `ps` call failed) counted as "the process is gone": a live orphan was dropped from
    // the record with no signal and no word. A process that can still be read is not
    // gone; its record is kept, and it is reported.
    let (reaped, signals, _) = run_with(&|_| alive(), &|_| None);
    assert_eq!(
      one_unresolved(&reaped),
      "Its parent process could not be read"
    );
    assert!(signals.is_empty(), "{signals:?}");
    // The parent cannot be read at the first asking and can be read at every later one.
    // The first look of a launch asks once, reads the process again and, as it is still
    // there, reports it: nothing is sent, the parent is not asked a second time.
    let asked = Cell::new(0usize);
    let parent_then_readable = |_: usize| {
      asked.set(asked.get() + 1);
      (asked.get() > 1).then_some(1)
    };
    let process = |sent: usize| if sent == 0 { alive() } else { Ok(None) };
    let (reaped, signals, readings) = run_with(&process, &parent_then_readable);
    assert_eq!(
      one_unresolved(&reaped),
      "Its parent process could not be read"
    );
    assert!(signals.is_empty(), "{signals:?}");
    assert_eq!(
      asked.get(),
      1,
      "one look, and it ended the matter for this launch"
    );
    assert_eq!(
      readings, 2,
      "the process was read again when its parent gave no answer"
    );
    // The record is kept, so the look of the next launch (the same record, the same
    // parent, which answers now) stops the process.
    let (reaped, signals, _) = run_with(&process, &parent_then_readable);
    assert_eq!(reaped.stopped, vec![PID]);
    assert!(reaped.unresolved.is_empty(), "{reaped:?}");
    assert_eq!(signals, ["-TERM"]);
    assert!(
      asked.get() >= 3,
      "asked at the first look and right before the signal"
    );
    // No parent because the process ended between the two readings: that is an end.
    let looks = Cell::new(0usize);
    let (reaped, signals, _) = run_with(
      &|_| {
        looks.set(looks.get() + 1);
        if looks.get() == 1 {
          alive()
        } else {
          Ok(None)
        }
      },
      &|_| None,
    );
    assert_eq!(reaped, Reaped::default());
    assert!(signals.is_empty());
    // Another parent than init at the first look: not provably left over, no signal.
    let (reaped, signals, _) = run_with(&|_| alive(), &|_| Some(77));
    assert_eq!(
      one_unresolved(&reaped),
      "It is still attached to another process (77), so it is not provably left over"
    );
    assert!(signals.is_empty(), "{signals:?}");
    // Another parent after the stop signal: not proven any more, so no kill signal.
    let (reaped, signals, _) = run_with(&|_| alive(), &|sent| Some(if sent == 0 { 1 } else { 77 }));
    assert_eq!(
      one_unresolved(&reaped),
      "It was asked to stop, and it is not known whether it stopped: it is still attached \
       to another process (77), so it is not provably left over"
    );
    assert_eq!(signals, ["-TERM"]);
    // A parent that cannot be read after the stop signal: the same.
    let (reaped, signals, _) = run_with(&|_| alive(), &|sent| (sent == 0).then_some(1));
    assert_eq!(
      one_unresolved(&reaped),
      "It was asked to stop, and it is not known whether it stopped: its parent process \
       could not be read"
    );
    assert_eq!(signals, ["-TERM"]);

    // A record that cannot prove anything (no creation time) is reported, never signaled.
    let incomplete = dir.join("incomplete.pid");
    record(
      &incomplete,
      &Record::from(&Identity {
        created: String::new(),
        ..recorded.clone()
      }),
    );
    let reaped = reap_leftovers_with(
      &incomplete,
      &["komika-server"],
      &Look {
        identity: &|_| Ok(Some(recorded.clone())),
        parent: &|_| Some(1),
        signal: &|_, _| panic!("a signal for an incomplete record"),
        grace: std::time::Duration::ZERO,
      },
    );
    assert_eq!(one_unresolved(&reaped), "Its record is incomplete");
    let _ = std::fs::remove_dir_all(dir);
  }

  #[cfg(unix)]
  #[test]
  fn leftovers_are_stopped_only_when_orphaned_and_named() {
    let dir = tmp("reap");
    let file = dir.join("children.pid");
    let orphan = orphan(Path::new("/bin/sleep"), &["60"]);
    // Our own live, non-orphaned child must never be touched, whatever its name.
    let mut own = std::process::Command::new("sleep")
      .arg("60")
      .spawn()
      .unwrap();
    let expected = record_of(orphan);
    record(&file, &expected);
    record(&file, &record_of(own.id()));
    use std::io::Write;
    writeln!(
      std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap(),
      "{orphan}\nnot-a-pid\n1"
    )
    .unwrap();

    // Name guard: a recorded pid running something else is left alone. The three
    // records of an older app (bare pids, one unreadable) authorize nothing either.
    let passed_over = reap_leftovers(&file, &["komika-server"]);
    assert!(passed_over.stopped.is_empty());
    assert_eq!(
      passed_over
        .unresolved
        .iter()
        .map(|u| u.line.as_str())
        .collect::<Vec<_>>(),
      vec![orphan.to_string().as_str(), "not-a-pid", "1"],
      "live or unreadable legacy records are reported, never used"
    );
    assert!(alive(orphan));

    let reparented_to_init = is_owned_orphan(&expected);
    eprintln!("orphan re-parented to pid 1: {reparented_to_init}");
    let result = reap_leftovers(&file, &["sleep"]);
    if reparented_to_init {
      assert_eq!(result.stopped, vec![orphan]);
      assert!(ends_soon(orphan), "left-behind process stopped");
    } else {
      // A subreaper (e.g. a service manager) adopted it: not provably ours, kept.
      assert!(result.stopped.is_empty());
      assert!(result.unresolved.iter().any(|u| u.pid == Some(orphan)));
      signal(orphan, "-KILL");
    }
    assert!(
      result
        .unresolved
        .iter()
        .any(|u| u.pid == Some(own.id()) && u.reason.contains("still attached")),
      "a live child of a live parent is reported, not signaled: {result:?}"
    );
    assert!(own.try_wait().unwrap().is_none(), "own child untouched");
    let _ = own.kill();
    let _ = own.wait();
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn init_records_and_forgets() {
    let _serial = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let file = registry();
    let value = Record {
      pid: 4242,
      image: PathBuf::from("/Applications/Komiq Server José 漢字.app/Contents/MacOS/komika-server"),
      created: "123:456".into(),
      unsettled: false,
    };
    record(&file, &value);
    // Other tests' fake servers may be recorded in the same file at the same time.
    let saved: Record = std::fs::read_to_string(&file)
      .unwrap()
      .lines()
      .filter_map(|line| serde_json::from_str::<Record>(line).ok())
      .find(|r| r.pid == 4242)
      .expect("the record was written");
    assert_eq!(saved.image, value.image);
    assert_eq!(saved.created, value.created);
    forget_all();
    assert!(!std::fs::read_to_string(&file)
      .unwrap()
      .contains("\"pid\":4242"));
  }

  /// An installed program path that is absolute on the system the test runs on. A record
  /// only matches a process whose program path is absolute, and `/Applications/…` is not
  /// absolute on Windows (it names no drive): there the same folders are put on `C:`.
  fn installed(path: &str) -> PathBuf {
    if cfg!(windows) {
      PathBuf::from(format!("C:{}", path.replace('/', "\\")))
    } else {
      PathBuf::from(path)
    }
  }

  /// The records are only written and read on Unix (on Windows the Job Object ends the
  /// children, and no record is kept). The rule itself is the same on every system, so
  /// this test runs on every system, with a path that is absolute there.
  #[test]
  fn installed_unicode_path_and_creation_identity_guard_pid_reuse() {
    let current = crate::postgres::process::Identity {
      pid: 42,
      image: installed("/Applications/Komiq Server José 漢字.app/Contents/MacOS/komika-server"),
      created: "123:456".into(),
      args: vec!["secret argument".into()],
    };
    assert!(current.image.is_absolute());
    let record = Record::from(&current);
    assert!(record.matches(&current));
    let serialized = serde_json::to_string(&record).unwrap();
    assert!(!serialized.contains("secret"));
    let decoded: Record = serde_json::from_str(&serialized).unwrap();
    assert!(decoded.matches(&current));
    let mut reused = current.clone();
    reused.created = "123:457".into();
    assert!(!decoded.matches(&reused));
    reused = current.clone();
    reused.image = installed("/other/komika-server");
    assert!(!decoded.matches(&reused));
    // A program path that is not absolute proves nothing about which file runs: such a
    // record never matches, even a process that reports the very same text.
    let mut loose = current.clone();
    loose.image = PathBuf::from("Komiq Server José 漢字/komika-server");
    assert!(!Record::from(&loose).matches(&loose));
    // Nor does a record without a creation time.
    let mut timeless = current.clone();
    timeless.created = String::new();
    assert!(!Record::from(&timeless).matches(&timeless));
    assert!(serde_json::from_str::<Record>("42").is_err());
  }

  /// The installed app's path has spaces and non-ASCII characters. The reaper must find
  /// and stop a real orphan started from such a path: this fails if the executable path
  /// is ever read through whitespace-split `ps` output again.
  #[cfg(unix)]
  #[test]
  fn reaper_stops_an_orphan_started_from_an_installed_path_with_spaces_and_unicode() {
    let dir = tmp("installed-path");
    let image = dir.join("Applications/Komiq Server José 漢字.app/Contents/MacOS/komika-server");
    install_helper(&image);
    let pid = orphan(&image, &HELPER_ARGS);
    let expected = record_of(pid);
    assert_eq!(expected.image, image.canonicalize().unwrap());
    let file = dir.join("children.pid");
    record(&file, &expected);
    let reparented_to_init = parent_of(pid) == Some(1);
    let result = reap_leftovers(&file, &CHILD_NAMES);
    if reparented_to_init {
      assert_eq!(result.stopped, vec![pid], "{result:?}");
      assert!(result.unresolved.is_empty(), "{result:?}");
      assert!(
        ends_soon(pid),
        "the orphan from the installed path was stopped"
      );
    } else {
      // A subreaper adopted it: reported, not stopped.
      assert!(result.stopped.is_empty());
      assert!(result.unresolved.iter().any(|u| u.pid == Some(pid)));
      signal(pid, "-KILL");
    }
    let _ = std::fs::remove_dir_all(dir);
  }

  /// With the record file installed, the recording hook writes the child (the control)
  /// and the transient-tool hook writes nothing for its child.
  #[cfg(unix)]
  #[test]
  fn transient_tool_hook_never_writes_reap_record() {
    let _serial = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let file = registry();
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .unwrap();
    rt.block_on(async {
      let recorded = |pid: u32| {
        std::fs::read_to_string(&file)
          .unwrap()
          .lines()
          .filter_map(|line| serde_json::from_str::<Record>(line).ok())
          .any(|r| r.pid == pid)
      };
      let mut kept = tokio::process::Command::new("sleep")
        .arg("10")
        .spawn()
        .unwrap();
      after_spawn(&kept);
      assert!(
        recorded(kept.id().unwrap()),
        "control: the record file is installed and a long-lived child is written to it"
      );
      let mut lasting = tokio::process::Command::new("sleep")
        .arg("10")
        .spawn()
        .unwrap();
      after_spawn_tied(&mut lasting, "A lasting child", true)
        .await
        .unwrap();
      assert!(
        recorded(lasting.id().unwrap()),
        "the app's own lasting children (database, server) are written to it as well"
      );
      lasting.kill().await.unwrap();
      let mut tool = tokio::process::Command::new("sleep")
        .arg("10")
        .spawn()
        .unwrap();
      after_spawn_tied(&mut tool, "A tool", false).await.unwrap();
      assert!(
        !recorded(tool.id().unwrap()),
        "a short-lived tool must never be signaled by the next launch"
      );
      kept.kill().await.unwrap();
      tool.kill().await.unwrap();
    });
  }

  /// Round 6, B3. On Windows the Job Object is all that ends a child with the app. A
  /// child that could not be put into it must not be left running: it is ended and
  /// collected, and the caller gets an error that says so (the stage fails). The failed
  /// tie is passed in, so this branch runs on every system, Windows included.
  #[test]
  fn a_child_that_cannot_be_tied_to_the_app_is_ended_and_the_stage_fails() {
    let rt = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
      .unwrap();
    rt.block_on(async {
      // This test program as an idle helper: a live child on every system.
      let helper = || {
        tokio::process::Command::new(std::env::current_exe().unwrap())
          .args([
            "--ignored",
            "--exact",
            "child::tests::helper_sleep",
            "--nocapture",
          ])
          .env(HELPER_SLEEP, "120")
          .stdin(std::process::Stdio::null())
          .stdout(std::process::Stdio::null())
          .stderr(std::process::Stdio::null())
          .spawn()
          .unwrap()
      };
      // Control: a tie that works leaves the child running and gives no error.
      let mut kept = helper();
      assert_eq!(
        tied_or_ended(&mut kept, "PostgreSQL", |_| Ok(())).await,
        Ok(())
      );
      assert_eq!(kept.try_wait().unwrap(), None, "still running");
      kept.kill().await.unwrap();

      let mut child = helper();
      let pid = child.id().expect("a live child");
      assert_eq!(child.try_wait().unwrap(), None, "running before the tie");
      let error = tied_or_ended(&mut child, "PostgreSQL", |tied| {
        assert_eq!(tied.id(), Some(pid), "the tie is tried on this child");
        Err("the process could not be put into the Job Object: Access is denied.".into())
      })
      .await
      .expect_err("the stage fails");
      for needle in [
        "PostgreSQL was started, but could not be tied to this app",
        "(the process could not be put into the Job Object: Access is denied.)",
        "would keep running after the app is closed or force-quit",
        "It was ended again, and nothing else was started.",
        "Retry",
      ] {
        assert!(error.contains(needle), "{needle}: {error}");
      }
      // A child that ended by itself before the tie was tried (a short tool): the failed
      // tie is no failure, and its own exit status is still there for the caller.
      let mut done = tokio::process::Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
      // Its output ends when it has ended.
      let mut listed = Vec::new();
      tokio::io::AsyncReadExt::read_to_end(done.stdout.as_mut().unwrap(), &mut listed)
        .await
        .unwrap();
      let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
      while done.try_wait().unwrap().is_none() {
        assert!(std::time::Instant::now() < deadline, "the short child ends");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
      }
      let outcome = tied_or_ended(&mut done, "The PostgreSQL tool initdb", |_| {
        Err("the process could not be put into the Job Object: Access is denied.".into())
      })
      .await;
      assert_eq!(outcome, Ok(()), "a child that already ended is not refused");
      assert!(done.wait().await.unwrap().success(), "its own exit status");
      assert!(!listed.is_empty());

      // Ended and collected: no live process is left behind.
      assert!(
        child
          .try_wait()
          .unwrap()
          .is_some_and(|status| !status.success()),
        "the child was ended and collected"
      );
      assert_eq!(child.id(), None, "collected");
    });
  }

  /// N3: a record that cannot be verified is reported with its pid, program and the
  /// place it is kept, its process is left running, and the record survives the launch.
  #[cfg(unix)]
  #[test]
  fn unverified_records_are_moved_aside_reported_and_never_truncated() {
    let dir = tmp("unverified");
    let file = dir.join("children.pid");
    // A live child of a live parent, recorded under an allowed name: not an orphan, so
    // not provably left over. And a record of an older app version for the same process.
    let image = dir.join("komika-server");
    install_helper(&image);
    let mut survivor = spawn_helper(&image);
    let pid = survivor.id();
    record(&file, &record_of(pid));
    // A record whose process is gone is verified (nothing to stop) and needs no warning.
    let mut ended = spawn_helper(&image);
    let gone = record_of(ended.id());
    ended.kill().unwrap();
    ended.wait().unwrap();
    record(&file, &gone);
    use std::io::Write;
    writeln!(
      std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap(),
      "{pid}"
    )
    .unwrap();
    let before = std::fs::read_to_string(&file).unwrap();

    let (current, warnings) = prepare(&dir);
    assert_eq!(current, file);
    let aside: Vec<PathBuf> = std::fs::read_dir(&dir)
      .unwrap()
      .filter_map(Result::ok)
      .map(|e| e.path())
      .filter(|p| {
        p.file_name()
          .unwrap()
          .to_string_lossy()
          .starts_with(ASIDE_PREFIX)
      })
      .collect();
    assert_eq!(aside.len(), 1, "the unverified records were moved aside");
    assert_eq!(
      std::fs::read_to_string(&aside[0]).unwrap(),
      before,
      "nothing was dropped from the evidence"
    );
    assert_eq!(
      std::fs::read_to_string(&file).unwrap(),
      "",
      "this run starts a new record file"
    );
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    for warning in &warnings {
      assert!(warning.contains(&format!("process {pid}")), "{warning}");
      assert!(
        warning.contains(&image.canonicalize().unwrap().display().to_string()),
        "{warning}"
      );
      assert!(
        warning.contains(&aside[0].display().to_string()),
        "{warning}"
      );
      assert!(warning.contains(&format!("kill {pid}")), "{warning}");
    }
    assert!(survivor.try_wait().unwrap().is_none(), "left running");

    // The next launch still cannot verify it: told again, record still kept.
    let (_, again) = prepare(&dir);
    assert_eq!(again.len(), 2, "{again:?}");
    assert_eq!(std::fs::read_to_string(&aside[0]).unwrap(), before);

    // Once the process is gone every record is verified and the kept file is removed.
    survivor.kill().unwrap();
    survivor.wait().unwrap();
    let (_, resolved) = prepare(&dir);
    assert!(resolved.is_empty(), "{resolved:?}");
    assert!(!aside[0].exists());
    let _ = std::fs::remove_dir_all(dir);
  }

  /// A child that could not be recorded is a warning the local server can take.
  #[test]
  fn warnings_are_queued_once_and_handed_over() {
    let _serial = WARNING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let text = format!("test warning {}", std::process::id());
    warn(text.clone());
    warn(text.clone());
    let taken = take_warnings();
    assert_eq!(taken.iter().filter(|w| **w == text).count(), 1);
    assert!(!take_warnings().contains(&text));
  }

  /// PR_SET_PDEATHSIG fires when the spawning THREAD ends, so a thread that spawns a
  /// child and exits stands in for a killed app.
  #[cfg(target_os = "linux")]
  #[test]
  fn linux_child_dies_with_its_spawner() {
    // One child with the hook, one without (the control), from a thread that then ends.
    let (guarded, control) = std::thread::spawn(|| {
      let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
      rt.block_on(async {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("60");
        before_spawn(&mut cmd);
        let guarded = cmd.spawn().unwrap();
        let control = tokio::process::Command::new("sleep")
          .arg("60")
          .spawn()
          .unwrap();
        let pids = (guarded.id().unwrap(), control.id().unwrap());
        // Not killed by us: forget the handles so only the kernel can end them.
        std::mem::forget(guarded);
        std::mem::forget(control);
        pids
      })
    })
    .join()
    .unwrap();
    // A dead child stays a zombie until reaped: count only non-zombie states as running.
    let running = |pid: u32| {
      std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| {
          let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
          !s.is_empty() && !s.starts_with('Z')
        })
        .unwrap_or(false)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while running(guarded) && std::time::Instant::now() < deadline {
      std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!running(guarded), "child ended with its spawner");
    assert!(running(control), "a child without the hook outlives it");
    signal(control, "-KILL");
  }
}
