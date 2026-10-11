//! What the server's automatic SQLite conversion is doing, read from the server's own
//! `sqlite import:` lines (apps/server/src/pg_import/: startup.rs, copy.rs, importer.rs,
//! source.rs, verifier.rs). The start wait (src/local.rs) uses the state to choose its
//! bound; the start screen shows it.
//!
//! Only a line that proves conversion work counts as progress. A warning, or a line this
//! file does not know, never extends a wait: an ordinary start that happens to print one
//! keeps the ordinary bound.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Every line of the server's importer starts with this.
pub const PREFIX: &str = "sqlite import:";
/// Stage names the server prints as `<stage> start` and `<stage> complete: <s>s`
/// (startup.rs `Stage::name`; `cleanup` is the finished stage and has no such lines).
const STAGES: [&str; 6] = ["copy", "migrate", "repair", "import", "verify", "rename"];
/// Warnings kept for the start screen and the log view.
pub const MAX_WARNINGS: usize = 50;
/// The two warnings that are about the owner's data: an earlier attempt's copy of the
/// database was moved aside and kept (only the owner may delete it), and the database
/// that is imported now holds fewer rows than that copy. They are kept and shown first,
/// also when the list is full of other warnings (startup.rs `kept_warning`,
/// `smaller_warning`).
const KEPT_WARNINGS: [&str; 2] = [
  "the copy of the SQLite database that an earlier import attempt made differs",
  "the database being imported is smaller than the copy kept at",
];
/// The line before the server reads whole files to see what they are. It comes before
/// the files of a conversion are hashed, and also before a scan that converts nothing
/// (startup.rs `say_scan`): a finished import with a work copy left over, and a start
/// that is refused after it compared the copies or counted the rows of kept copies (one
/// line for each file that is read).
const SCAN: &str = "source inventory start: ";
/// `repair:` lines kept to show next to a failure.
pub const MAX_REPAIR_LINES: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportState {
  /// No import line yet: an ordinary start as far as anyone can tell.
  Idle,
  /// The server is checking the old database file and the new database.
  Preparing,
  /// A stage, table or verification line was seen: data is being converted. Also while
  /// the server reads whole files to decide (the scan line), which can take as long.
  Converting,
  /// The server decided there is nothing to convert, or nothing more, and said why (a
  /// warning).
  NoOp,
  /// The server reported an error and will not serve.
  Failed,
  /// The conversion finished; the server continues with an ordinary start.
  Done,
}

impl ImportState {
  pub fn as_str(self) -> &'static str {
    match self {
      ImportState::Idle => "idle",
      ImportState::Preparing => "preparing",
      ImportState::Converting => "converting",
      ImportState::NoOp => "noop",
      ImportState::Failed => "failed",
      ImportState::Done => "done",
    }
  }
}

/// One server line, as the state machine reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
  Preparing,
  Progress,
  /// `restarting from the beginning: <reason>`: the conversion starts over. The stage
  /// lines that follow are the progress; this line itself extends no wait.
  Restart,
  Warning(String),
  Error(String),
  Done,
  /// An importer line of a shape this file does not know. Never progress.
  Other,
}

/// `None` for a line that is not the importer's.
pub fn classify(line: &str) -> Option<Line> {
  let rest = line.trim().strip_prefix(PREFIX)?.trim();
  if rest.starts_with("preparing") {
    return Some(Line::Preparing);
  }
  if let Some(text) = rest.strip_prefix("warning:") {
    return Some(Line::Warning(text.trim().to_string()));
  }
  if let Some(text) = rest.strip_prefix("error:") {
    return Some(Line::Error(text.trim().to_string()));
  }
  if rest.starts_with("restarting from the beginning: ") {
    return Some(Line::Restart);
  }
  // `complete: 1.151s total`. Stage lines read `<stage> complete: …` and do not match.
  if rest.starts_with("complete: ") {
    return Some(Line::Done);
  }
  Some(if is_progress(rest) {
    Line::Progress
  } else {
    Line::Other
  })
}

fn is_progress(rest: &str) -> bool {
  if [
    SCAN,
    "disk space: ",
    // The retained date repair's own output while the `repair` stage runs.
    "repair: ",
    "verifier PASS: ",
    "renamed original to ",
  ]
  .iter()
  .any(|shape| rest.starts_with(shape))
  {
    return true;
  }
  if STAGES.iter().any(|stage| {
    rest
      .strip_prefix(stage)
      .is_some_and(|tail| tail == " start" || tail.starts_with(" complete: "))
  }) {
    return true;
  }
  if let Some(tail) = rest.strip_prefix("table ") {
    // `table users: 5000 rows sent, 5.001s` and `table users complete: 3 rows, …`.
    return tail.contains(" rows sent, ") || tail.contains(" complete: ");
  }
  if let Some(tail) = rest.strip_prefix("precheck ") {
    // `precheck users start`, `precheck users complete: 3 rows`. Not `precheck failed: …`.
    return tail.ends_with(" start") || tail.contains(" complete: ");
  }
  if let Some(tail) = rest.strip_prefix("verify ") {
    // `verify users: SQLite=3 PG=3 checksum …`.
    return tail.contains(": SQLite=");
  }
  false
}

/// The three waits of a server start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
  /// An ordinary start; also the time the server may spend checking before it converts.
  pub normal: Duration,
  /// The longest silence between two progress lines of a running conversion.
  pub inactivity: Duration,
  /// The longest a start with a conversion may take in all, from spawn.
  pub total: Duration,
}

/// Why a start wait ended without a healthy server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overdue {
  /// No import line at all, and no answer within the normal bound from spawn.
  Start,
  /// Still checking after the normal bound from the `preparing` line.
  Preparing,
  /// Nothing to convert, yet no answer within the normal bound from that line.
  AfterNoOp,
  /// Converted, yet no answer within the normal bound from the `complete` line.
  AfterDone,
  /// A conversion printed no progress line for the inactivity bound.
  Stalled,
  /// A conversion ran past the total cap.
  TooLong,
  /// The server printed an error line.
  Failed,
}

/// The import of one server process, as far as its output has told.
#[derive(Clone, Debug)]
pub struct Tracker {
  state: ImportState,
  /// Start of the running bound: when the state was entered, or the last progress line.
  since: Option<Instant>,
  /// Last `preparing` or progress line, for the start screen.
  progress: Option<String>,
  progress_lines: u64,
  verifier_pass: Option<String>,
  renamed: Option<String>,
  error: Option<String>,
  repair: VecDeque<String>,
  warnings: Vec<String>,
}

impl Default for Tracker {
  fn default() -> Self {
    Tracker {
      state: ImportState::Idle,
      since: None,
      progress: None,
      progress_lines: 0,
      verifier_pass: None,
      renamed: None,
      error: None,
      repair: VecDeque::new(),
      warnings: Vec::new(),
    }
  }
}

impl Tracker {
  /// Feed one cleaned output line of the server. Lines of other origins are ignored.
  pub fn observe(&mut self, line: &str, now: Instant) {
    let Some(kind) = classify(line) else {
      return;
    };
    // After an error nothing the server prints makes the start good again.
    if self.state == ImportState::Failed {
      if let Line::Warning(text) = kind {
        self.warn(text);
      }
      return;
    }
    let shown = line.trim().to_string();
    match kind {
      Line::Preparing => {
        self.state = ImportState::Preparing;
        self.since = Some(now);
        self.progress = Some(shown);
      }
      Line::Progress => {
        self.state = ImportState::Converting;
        self.since = Some(now);
        self.progress_lines += 1;
        let rest = shown[PREFIX.len()..].trim();
        if rest.starts_with("verifier PASS: ") {
          self.verifier_pass = Some(shown.clone());
        } else if rest.starts_with("renamed original to ") {
          self.renamed = Some(shown.clone());
        } else if let Some(text) = rest.strip_prefix("repair: ") {
          self.repair.push_back(text.to_string());
          while self.repair.len() > MAX_REPAIR_LINES {
            self.repair.pop_front();
          }
        }
        self.progress = Some(shown);
      }
      Line::Restart => {
        // What an earlier pass proved is void. The inactivity wait begins here only when
        // no conversion was running; a running one is not given more time by this line.
        if self.state != ImportState::Converting {
          self.state = ImportState::Converting;
          self.since = Some(now);
        }
        self.verifier_pass = None;
        self.renamed = None;
        self.progress = Some(shown);
      }
      Line::Warning(text) => {
        self.warn(text);
        // A warning before any conversion work is the server saying "nothing to do".
        // During a conversion it changes nothing and, above all, extends nothing.
        if matches!(self.state, ImportState::Idle | ImportState::Preparing) {
          self.state = ImportState::NoOp;
          self.since = Some(now);
          self.progress = None;
        }
      }
      Line::Error(text) => {
        self.state = ImportState::Failed;
        self.since = Some(now);
        self.error = Some(text);
      }
      Line::Done => {
        self.state = ImportState::Done;
        self.since = Some(now);
        self.progress = None;
      }
      Line::Other => {}
    }
  }

  /// The server answers. It runs its import before it listens, so from here nothing is
  /// checked or converted any more: it is an ordinary, started server.
  ///
  /// A conversion that ended well was ended by its `complete` line before this, and one
  /// that failed never answers. What is left are the starts that end without either:
  ///
  /// * `preparing` and nothing after it: there was nothing to convert;
  /// * the scan line and no conversion (a finished import whose work copy was left over
  ///   prints `preparing`, the scan line and a warning);
  /// * a finished import whose interrupted archive rename could not be finished in this
  ///   start either: `verify` and `rename` lines, then warnings, and no `complete` line.
  ///
  /// The scan and stage lines put the tracker into `Converting`, so that the work is
  /// waited for; without this the state would stay there for as long as the server runs,
  /// and the start screen would go on saying that a conversion is running.
  ///
  /// What it becomes: `NoOp` when the server gave a warning in this start (it said why
  /// nothing more was converted), otherwise `Idle`. Lines that reach this file after the
  /// answer are still read: a `complete` line that was on its way makes it `Done`.
  pub fn ready(&mut self) {
    if matches!(self.state, ImportState::Preparing | ImportState::Converting) {
      self.state = if self.state == ImportState::Converting && !self.warnings.is_empty() {
        ImportState::NoOp
      } else {
        ImportState::Idle
      };
      self.since = None;
      self.progress = None;
    }
  }

  /// Keep a warning once. The list holds at most [`MAX_WARNINGS`] ordinary warnings. One
  /// of the two about the owner's data ([`KEPT_WARNINGS`]) is kept also when the list is
  /// full, and stands before the ordinary ones, so that fifty lines about left-over
  /// files cannot push it out or below the fold. (At most [`MAX_WARNINGS`] of those as
  /// well; the server prints one or two in a start.)
  fn warn(&mut self, text: String) {
    if self.warnings.contains(&text) {
      return;
    }
    let kept = |warning: &String| KEPT_WARNINGS.iter().any(|start| warning.starts_with(start));
    if kept(&text) {
      let before = self.warnings.iter().take_while(|w| kept(w)).count();
      if before < MAX_WARNINGS {
        self.warnings.insert(before, text);
      }
    } else if self.warnings.iter().filter(|w| !kept(w)).count() < MAX_WARNINGS {
      self.warnings.push(text);
    }
  }

  pub fn state(&self) -> ImportState {
    self.state
  }

  /// The line to show while the server checks or converts. Nothing in other states.
  pub fn progress(&self) -> Option<&str> {
    match self.state {
      ImportState::Preparing | ImportState::Converting => self.progress.as_deref(),
      _ => None,
    }
  }

  pub fn progress_lines(&self) -> u64 {
    self.progress_lines
  }

  pub fn verifier_pass(&self) -> Option<&str> {
    self.verifier_pass.as_deref()
  }

  pub fn renamed(&self) -> Option<&str> {
    self.renamed.as_deref()
  }

  pub fn warnings(&self) -> &[String] {
    &self.warnings
  }

  /// The server's own words after `error:`.
  pub fn error(&self) -> Option<&str> {
    self.error.as_deref()
  }

  /// The last `repair:` lines of this start, oldest first.
  pub fn repair_lines(&self) -> Vec<String> {
    self.repair.iter().cloned().collect()
  }

  /// `conversion` only while a conversion is running; every other state waits the
  /// normal bound.
  pub fn bound_kind(&self) -> &'static str {
    match self.state {
      ImportState::Converting => "conversion",
      ImportState::Failed => "failed",
      _ => "normal",
    }
  }

  /// Whether the wait that began at `spawned` is over at `now`, and why.
  pub fn overdue(&self, spawned: Instant, now: Instant, bounds: &Bounds) -> Option<Overdue> {
    let since = self.since.unwrap_or(spawned);
    let waited = now.saturating_duration_since(since);
    match self.state {
      ImportState::Idle => {
        (now.saturating_duration_since(spawned) >= bounds.normal).then_some(Overdue::Start)
      }
      ImportState::Preparing => (waited >= bounds.normal).then_some(Overdue::Preparing),
      ImportState::NoOp => (waited >= bounds.normal).then_some(Overdue::AfterNoOp),
      ImportState::Done => (waited >= bounds.normal).then_some(Overdue::AfterDone),
      ImportState::Converting => {
        if waited >= bounds.inactivity {
          Some(Overdue::Stalled)
        } else if now.saturating_duration_since(spawned) >= bounds.total {
          Some(Overdue::TooLong)
        } else {
          None
        }
      }
      ImportState::Failed => Some(Overdue::Failed),
    }
  }

  /// The failure sentence for the start screen: the server's error, then what its
  /// repair step printed (when it ran in this start).
  pub fn failure_text(&self) -> String {
    let mut text = format!(
      "The database conversion failed: {}",
      self.error.as_deref().unwrap_or("the log has the cause")
    );
    if !self.repair.is_empty() {
      text.push_str("\nRepair step output:");
      for line in &self.repair {
        text.push_str("\n  ");
        text.push_str(line);
      }
    }
    text
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const BOUNDS: Bounds = Bounds {
    normal: Duration::from_secs(60),
    inactivity: Duration::from_secs(1800),
    total: Duration::from_secs(6 * 3600),
  };

  fn at(start: Instant, secs: u64) -> Instant {
    start + Duration::from_secs(secs)
  }

  // Lines below are copied from a real run of the server's importer (gate log of
  // 2026-10-11), or from the `eprintln!` in the file named beside them.
  const PREPARING: &str = "sqlite import: preparing (checking source and target)";
  const NOOP: &str = "sqlite import: warning: PostgreSQL already holds data or a completed import; leaving SQLite in place";
  /// The scan line (startup.rs `say_scan` and the source inventory).
  const SCAN_LINE: &str = "sqlite import: source inventory start: 1060864 bytes";
  // The lines of the server's import fix rounds 3 to 5, made from its format strings.
  const DONE_LEFT_IN_PLACE: &str = "sqlite import: warning: PostgreSQL already holds the imported data; leaving /data/komika.sqlite3 in place (earlier archive: komika.sqlite3.imported-20261011T115920Z)";
  const KEPT_COPY: &str = "sqlite import: warning: the copy of the SQLite database that an earlier import attempt made differs from the file now at /data/komika.sqlite3; an exact copy of the database as it was then is kept at /data/komika.sqlite3.import-kept-20261012T080000Z and /data/komika.sqlite3.import-kept-20261012T080000Z-wal. The server never deletes it: you may delete it once you have checked your data";
  const SMALLER: &str = "sqlite import: warning: the database being imported is smaller than the copy kept at /data/komika.sqlite3.import-kept-20261012T080000Z (users: 2 now, 3 before; user_library: 10 now, 10 before)";
  const RESTART_CHANGED: &str = "sqlite import: restarting from the beginning: the SQLite file changed since the unfinished import began";
  /// The one error line of a refused start; the text after the stage is shortened here.
  const REFUSED: &str = "sqlite import: error: recovery stage: the unfinished import cannot go on with the files in this directory";
  /// The two warnings of a finished import whose archive rename was interrupted and
  /// could not be finished in this start either (startup.rs `settle_done`).
  const RENAME_NOT_FINISHED: &str = "sqlite import: warning: could not finish the earlier archive rename: Permission denied (os error 13)";
  /// The error line of a refused start as the server's import fix round 6 writes it
  /// (startup.rs `refusal`, a missing file with PostgreSQL holding part of its data, two
  /// kept copies): one line, several thousand characters, with paths that hold spaces
  /// and other letters, brackets and commas. The instructions stand at its end.
  fn long_refusal() -> String {
    let source =
      "/Users/José Müller/Library/Application Support/komiq desktop/server/komika.sqlite3";
    let record = format!("{source}.import-work-state");
    let files = [
      ".import-work.repair-lock",
      ".import-work",
      ".import-work-wal",
      ".import-work-shm",
      ".import-work-journal",
      ".import-work-source",
      ".import-work-source-wal",
      ".import-work-source-shm",
      ".import-work-source-journal",
      ".import-work-state.next-4f2a",
    ]
    .map(|name| format!("{source}{name}"))
    .join(", ");
    let newest = format!(
      "{source}.import-kept-20261012T080000Z (comments: 4, user_library: 12, users: 3, work: 15)"
    );
    let older = format!(
      "{source}.import-kept-20261011T070000Z (rows not counted: it has a write-ahead log or a \
       rollback journal)"
    );
    let kept = [
      format!("{source}.import-kept-20261012T080000Z"),
      format!("{source}.import-kept-20261011T070000Z"),
      format!("{source}.import-kept-20261011T070000Z-wal"),
    ]
    .join(", ");
    format!(
      "sqlite import: error: recovery stage: the SQLite file {source} named by the unfinished \
       import is missing, and PostgreSQL holds only part of its data. Nothing was changed. \
       Kept by earlier starts (each is the SQLite database as it was when an earlier import \
       attempt began): {newest}, {older}. To continue, put {source} back and start again. Or, \
       to import the kept copy {newest} instead, rename \
       {source}.import-kept-20261012T080000Z to {source} and start again; any other kept copy \
       is imported the same way. Or, to start with an empty database, empty the PostgreSQL \
       database (drop it and create it again), delete {record} and the work files {files}, \
       move {kept} out of the data directory and start again"
    )
  }
  const REAL_PROGRESS: [&str; 17] = [
    "sqlite import: source inventory start: 1060864 bytes",
    "sqlite import: copy start",
    "sqlite import: disk space: 47126335488 bytes free, 276922368 bytes required",
    "sqlite import: copy complete: 0.164s",
    "sqlite import: migrate start",
    "sqlite import: repair start",
    "sqlite import: repair: chapter: 2 rows applied",
    "sqlite import: import start",
    "sqlite import: precheck users start",
    "sqlite import: precheck users complete: 3 rows",
    "sqlite import: table chapter: 250000 rows sent, 5.004s", // copy.rs
    "sqlite import: table users complete: 3 rows, 0.001s, 553 COPY bytes",
    "sqlite import: verify users: SQLite=3 PG=3 checksum 9f", // verifier.rs
    "sqlite import: verifier PASS: 63 tables",
    "sqlite import: renamed original to /data/komika.sqlite3.imported-20261011T115920Z",
    "sqlite import: rename complete: 0.079s",
    "sqlite import: verify start",
  ];

  #[test]
  fn real_line_shapes_are_classified() {
    assert_eq!(classify("listening on http://127.0.0.1:1/graphql"), None);
    assert_eq!(classify(PREPARING), Some(Line::Preparing));
    for line in REAL_PROGRESS {
      assert_eq!(classify(line), Some(Line::Progress), "{line}");
    }
    assert_eq!(
      classify("sqlite import: complete: 1.151s total"),
      Some(Line::Done)
    );
    assert!(matches!(classify(NOOP), Some(Line::Warning(_))));
    assert_eq!(
      classify("sqlite import: error: verify stage: verifier did not pass"),
      Some(Line::Error("verify stage: verifier did not pass".into()))
    );
    // Importer lines that prove no conversion work are never progress.
    for line in [
      "sqlite import: precheck failed: 2 failing rules", // importer.rs
      "sqlite import: cleanup stage: journal published; archival reconciliation required",
      "sqlite import: something a later server version prints",
      "sqlite import: copy",
    ] {
      assert_eq!(classify(line), Some(Line::Other), "{line}");
    }
  }

  /// The server's call that runs the import, and its two calls that open a listener
  /// (apps/server/src/main.rs). Each text stands there exactly once.
  const IMPORT_CALL: &str = "pg_import::startup::run(";
  const PUBLIC_BIND: &str = "tokio::net::TcpListener::bind(addr)";
  const METRICS_BIND: &str = "tokio::net::TcpListener::bind(&metrics_addr)";

  /// Whether the server, by the text of its `main.rs`, runs the import before it opens
  /// the public listener and the metrics listener. `Tracker::ready` relies on this
  /// order: it takes an answer of the server as proof that nothing is converted any more.
  fn imports_before_it_listens(main: &str) -> Result<(), String> {
    let place = |text: &str| match main.match_indices(text).collect::<Vec<_>>()[..] {
      [(index, _)] => Ok(index),
      ref found => Err(format!(
        "apps/server/src/main.rs has `{text}` {} times, not once, so the order of the \
         import and the listeners cannot be read from it. `Tracker::ready` in \
         src/import.rs relies on this order (the import runs before the server listens): \
         find the calls again and correct this test, or change `ready`.",
        found.len()
      )),
    };
    let import = place(IMPORT_CALL)?;
    for (bind, name) in [
      (PUBLIC_BIND, "public listener"),
      (METRICS_BIND, "metrics listener"),
    ] {
      if place(bind)? < import {
        return Err(format!(
          "apps/server/src/main.rs opens the {name} (`{bind}`) before it runs the import \
           (`{IMPORT_CALL}`). `Tracker::ready` in src/import.rs relies on this order: it \
           reads an answer of the server as \"nothing is converted any more\". Change both \
           together."
        ));
      }
    }
    Ok(())
  }

  /// The contract with the server, checked against the server's own source: every line
  /// shape this file depends on must still be a format string there, and a line made
  /// from that format string must be read as the kind this file expects. A server-side
  /// rename of a line fails here, in the desktop's tests.
  #[test]
  fn the_servers_source_still_prints_the_line_shapes_this_file_reads() {
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
      .ancestors()
      .nth(3)
      .expect("apps/admin/src-tauri lies three folders below the repository");
    let folder = repository.join("apps/server/src/pg_import");
    if !folder.join("startup.rs").is_file() {
      eprintln!(
        "SKIPPED: {} is not here (the desktop crate was checked out without the server), \
         so the line shapes were not compared with the server's source",
        folder.join("startup.rs").display()
      );
      return;
    }
    let read = |file: &str| {
      std::fs::read_to_string(folder.join(file))
        .unwrap_or_else(|e| panic!("{}: {e}", folder.join(file).display()))
    };
    use Line::*;
    // (server file, the start of its format string, a line made from it, how it is read)
    let shapes: [(&str, &str, &str, Line); 21] = [
      (
        "startup.rs",
        "sqlite import: preparing",
        PREPARING,
        Preparing,
      ),
      (
        "startup.rs",
        "sqlite import: verifier PASS: ",
        "sqlite import: verifier PASS: 63 tables",
        Progress,
      ),
      (
        "startup.rs",
        "sqlite import: renamed original to ",
        "sqlite import: renamed original to /data/komika.sqlite3.imported-20261011T115920Z",
        Progress,
      ),
      (
        "startup.rs",
        "sqlite import: complete: ",
        "sqlite import: complete: 1.151s total",
        Done,
      ),
      (
        "startup.rs",
        "sqlite import: warning: ",
        NOOP,
        Warning(
          "PostgreSQL already holds data or a completed import; leaving SQLite in place".into(),
        ),
      ),
      (
        "startup.rs",
        "sqlite import: error: ",
        "sqlite import: error: verify stage: verifier did not pass",
        Error("verify stage: verifier did not pass".into()),
      ),
      (
        "startup.rs",
        "sqlite import: repair: ",
        "sqlite import: repair: chapter: 2 rows applied",
        Progress,
      ),
      (
        "startup.rs",
        "sqlite import: {} start\"",
        "sqlite import: copy start",
        Progress,
      ),
      (
        "startup.rs",
        "sqlite import: {} complete: ",
        "sqlite import: copy complete: 0.164s",
        Progress,
      ),
      (
        "startup.rs",
        "sqlite import: source inventory start: ",
        "sqlite import: source inventory start: 1060864 bytes",
        Progress,
      ),
      (
        "startup.rs",
        "sqlite import: disk space: ",
        "sqlite import: disk space: 47126335488 bytes free, 276922368 bytes required",
        Progress,
      ),
      (
        "copy.rs",
        "sqlite import: table {}: {rows} rows sent, ",
        "sqlite import: table chapter: 250000 rows sent, 5.004s",
        Progress,
      ),
      (
        "importer.rs",
        "sqlite import: table {} complete: ",
        "sqlite import: table users complete: 3 rows, 0.001s, 553 COPY bytes",
        Progress,
      ),
      (
        "source.rs",
        "sqlite import: precheck {} start\"",
        "sqlite import: precheck users start",
        Progress,
      ),
      (
        "source.rs",
        "sqlite import: precheck {} complete: ",
        "sqlite import: precheck users complete: 3 rows",
        Progress,
      ),
      (
        "verifier.rs",
        "sqlite import: verify {}: SQLite=",
        "sqlite import: verify users: SQLite=3 PG=3 checksum 9f",
        Progress,
      ),
      // Since the server's import fix rounds 3 to 5.
      (
        "startup.rs",
        "sqlite import: restarting from the beginning: {}\"",
        RESTART_CHANGED,
        Restart,
      ),
      (
        "startup.rs",
        "sqlite import: restarting from the beginning: the earlier attempt stopped before",
        "sqlite import: restarting from the beginning: the earlier attempt stopped before \
         PostgreSQL held any imported data",
        Restart,
      ),
      (
        "startup.rs",
        "sqlite import: warning: PostgreSQL already holds the imported data; leaving {} in place",
        DONE_LEFT_IN_PLACE,
        Warning(DONE_LEFT_IN_PLACE[PREFIX.len() + " warning: ".len()..].into()),
      ),
      (
        "startup.rs",
        "sqlite import: warning: the copy of the SQLite database that an earlier import attempt made differs from the file now at {}",
        KEPT_COPY,
        Warning(KEPT_COPY[PREFIX.len() + " warning: ".len()..].into()),
      ),
      (
        "startup.rs",
        "sqlite import: warning: the database being imported is smaller than the copy kept at {}",
        SMALLER,
        Warning(SMALLER[PREFIX.len() + " warning: ".len()..].into()),
      ),
    ];
    for (file, format, sample, expected) in shapes {
      assert!(
        read(file).contains(&format!("\"{format}")),
        "apps/server/src/pg_import/{file} no longer has a format string that starts with \
         `{format}`. src/import.rs reads that line: change both together."
      );
      assert_eq!(classify(sample), Some(expected), "{sample}");
    }
    // The stage names the `{} start` and `{} complete` lines are printed with.
    let startup = read("startup.rs");
    for stage in STAGES {
      assert!(
        startup.contains(&format!("=> \"{stage}\"")),
        "startup.rs no longer names a stage `{stage}`"
      );
      assert_eq!(
        classify(&format!("sqlite import: {stage} start")),
        Some(Progress)
      );
    }
    // The two warnings that are kept when the list is full are found by how they start:
    // the server's format strings must start that way.
    for start in KEPT_WARNINGS {
      assert!(
        startup.contains(&format!("\"sqlite import: warning: {start}")),
        "startup.rs no longer has a warning that starts with `{start}`. src/import.rs \
         keeps that warning by this start: change both together."
      );
    }
    // The pieces `long_refusal` is made of, so that the sample stays a text the server
    // can print.
    for piece in [
      "\"sqlite import: error: {message}\"",
      " Or, to import the kept copy {which} instead, {renames} and start again{}.",
      "; any other kept copy is imported the same way",
      " Kept by earlier starts (each is the SQLite database as it was when an earlier import attempt began): {}.",
      "is missing, and PostgreSQL holds only part of its data. Nothing was changed.{note} To continue, {back} Or, to start with an empty database, {EMPTY_DATABASE}, {forget_all} and start again",
      "rows not counted: it has a write-ahead log or a rollback journal",
      "\"sqlite import: warning: could not finish the earlier archive rename: {}\"",
    ] {
      assert!(
        startup.contains(piece),
        "startup.rs no longer has `{piece}`; a sample line of the tests in src/import.rs \
         is made of it"
      );
    }
    // The order `Tracker::ready` relies on: the import runs before the server listens.
    let main = std::fs::read_to_string(repository.join("apps/server/src/main.rs"))
      .unwrap_or_else(|e| panic!("apps/server/src/main.rs: {e}"));
    if let Err(why) = imports_before_it_listens(&main) {
      panic!("{why}");
    }
    // The same check sees the other order, in the server's own text with the import
    // call moved behind one of the listeners, and a text in which a call is not unique.
    let call_line = main
      .lines()
      .find(|line| line.contains(IMPORT_CALL))
      .expect("checked above");
    for bind in [PUBLIC_BIND, METRICS_BIND] {
      let bind_line = main
        .lines()
        .find(|line| line.contains(bind))
        .expect("checked above");
      let swapped = main
        .replace(call_line, "")
        .replace(bind_line, &format!("{bind_line}\n{call_line}"));
      let why = imports_before_it_listens(&swapped).expect_err("the import after a listener");
      assert!(
        why.contains(bind) && why.contains("`Tracker::ready`"),
        "{why}"
      );
    }
    for text in [IMPORT_CALL, PUBLIC_BIND, METRICS_BIND] {
      let why = imports_before_it_listens(&format!("{main}\n{text}")).expect_err("twice");
      assert!(
        why.contains("2 times") && why.contains("`Tracker::ready`"),
        "{why}"
      );
      let why = imports_before_it_listens(&main.replace(text, "")).expect_err("gone");
      assert!(why.contains("0 times"), "{why}");
    }
    // The scan line: the server prints the inventory line of a conversion also before a
    // scan that converts nothing (`say_scan`), with the same format string.
    let scan = format!("\"sqlite import: {SCAN}{{bytes}} bytes\"");
    assert!(
      startup.matches(&scan).count() >= 2 && startup.contains("fn say_scan("),
      "startup.rs no longer prints `{scan}` both for a conversion and before a recovery \
       scan (`say_scan`). `Tracker::ready` ends a scan that converted nothing by this line."
    );
  }

  /// A scan that converts nothing, after a finished import whose work copy was left
  /// over (startup.rs `settle_done`): `preparing`, the scan line, a warning, and then
  /// the server starts. The scan reads whole files and may take long; the line that
  /// announces it is read as progress, so it is waited for like a conversion. When the
  /// server answers, nothing was converted, and the state must not stay `Converting`.
  #[test]
  fn a_scan_that_converts_nothing_is_waited_for_and_ends_when_the_server_answers() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    tracker.observe(SCAN_LINE, at(spawned, 2));
    assert_eq!(tracker.state(), ImportState::Converting);
    assert_eq!(tracker.bound_kind(), "conversion");
    assert_eq!(tracker.progress(), Some(SCAN_LINE));
    // 61 seconds after the scan line (the normal bound is 60): not overdue.
    assert_eq!(tracker.overdue(spawned, at(spawned, 63), &BOUNDS), None);
    assert_eq!(tracker.overdue(spawned, at(spawned, 1000), &BOUNDS), None);
    // Without the scan line the same start was over after the normal bound.
    let mut unannounced = Tracker::default();
    unannounced.observe(PREPARING, at(spawned, 1));
    assert_eq!(
      unannounced.overdue(spawned, at(spawned, 62), &BOUNDS),
      Some(Overdue::Preparing)
    );
    // The warning says why nothing is converted. In a running scan it changes nothing.
    tracker.observe(DONE_LEFT_IN_PLACE, at(spawned, 1000));
    assert_eq!(tracker.state(), ImportState::Converting);
    assert_eq!(tracker.warnings().len(), 1);
    // The server answers: nothing was converted, and the server said why.
    tracker.ready();
    assert_eq!(tracker.state(), ImportState::NoOp);
    assert_eq!(tracker.bound_kind(), "normal");
    assert_eq!(tracker.progress(), None);
    assert_eq!(tracker.error(), None);
    assert_eq!(tracker.verifier_pass(), None);
    assert_eq!(tracker.warnings().len(), 1, "the warning is still shown");
    // A scan with no warning at all, then the server answers: an ordinary start.
    let mut silent = Tracker::default();
    silent.observe(PREPARING, at(spawned, 1));
    silent.observe(SCAN_LINE, at(spawned, 2));
    silent.observe(SCAN_LINE, at(spawned, 40));
    silent.ready();
    assert_eq!(silent.state(), ImportState::Idle);
    assert_eq!(silent.progress(), None);
    // A failure is not ended by an answer, nor is a finished conversion.
    let mut failed = Tracker::default();
    failed.observe(SCAN_LINE, at(spawned, 2));
    failed.observe(REFUSED, at(spawned, 3));
    failed.ready();
    assert_eq!(failed.state(), ImportState::Failed);
    let mut done = Tracker::default();
    done.observe(SCAN_LINE, at(spawned, 2));
    done.observe("sqlite import: complete: 1.151s total", at(spawned, 3));
    done.ready();
    assert_eq!(done.state(), ImportState::Done);
  }

  /// A finished import whose archive rename was interrupted (startup.rs `settle_done`
  /// and `finish_archive`). The server verifies again and renames, with the lines of a
  /// conversion. Since its import fix round 6 it ends that with the `complete` line.
  #[test]
  fn an_interrupted_archive_rename_that_is_finished_ends_as_done() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    tracker.observe(SCAN_LINE, at(spawned, 2));
    tracker.observe("sqlite import: verify start", at(spawned, 3));
    // The verification of a large database: the long bound, counted from the last line.
    assert_eq!(tracker.overdue(spawned, at(spawned, 64), &BOUNDS), None);
    assert_eq!(tracker.overdue(spawned, at(spawned, 1802), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 1803), &BOUNDS),
      Some(Overdue::Stalled)
    );
    for (line, secs) in [
      ("sqlite import: verifier PASS: 63 tables", 1000),
      ("sqlite import: verify complete: 997.212s", 1000),
      ("sqlite import: rename start", 1000),
      (
        "sqlite import: renamed original to /data/komika.sqlite3.imported-20261011T115920Z",
        1001,
      ),
      ("sqlite import: rename complete: 0.079s", 1001),
    ] {
      tracker.observe(line, at(spawned, secs));
      assert_eq!(tracker.state(), ImportState::Converting, "{line}");
      assert_eq!(tracker.bound_kind(), "conversion");
    }
    tracker.observe(
      "sqlite import: complete: 1000.151s total",
      at(spawned, 1001),
    );
    assert_eq!(tracker.state(), ImportState::Done);
    assert_eq!(tracker.bound_kind(), "normal");
    assert_eq!(tracker.progress(), None);
    assert_eq!(
      tracker.verifier_pass(),
      Some("sqlite import: verifier PASS: 63 tables")
    );
    assert!(tracker.renamed().is_some());
    assert!(tracker.warnings().is_empty());
    // From the `complete` line the server has the time of an ordinary start.
    assert_eq!(tracker.overdue(spawned, at(spawned, 1060), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 1061), &BOUNDS),
      Some(Overdue::AfterDone)
    );
    tracker.ready();
    assert_eq!(tracker.state(), ImportState::Done, "the answer keeps it");
  }

  /// The same start when the rename could not be finished: the server prints the lines
  /// of the work it did, then says so in warnings, prints no `complete` line and starts
  /// (PostgreSQL holds the data). Round 9 left that state `Converting` for as long as
  /// the server ran.
  #[test]
  fn an_archive_rename_that_could_not_be_finished_ends_when_the_server_answers() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    tracker.observe(SCAN_LINE, at(spawned, 2));
    for line in [
      "sqlite import: verify start",
      "sqlite import: verifier PASS: 63 tables",
      "sqlite import: verify complete: 0.212s",
      "sqlite import: rename start",
    ] {
      tracker.observe(line, at(spawned, 3));
    }
    tracker.observe(RENAME_NOT_FINISHED, at(spawned, 4));
    tracker.observe(DONE_LEFT_IN_PLACE, at(spawned, 4));
    // Until the server answers nothing tells the end of the work from a pause in it:
    // the state and the long bound stay, counted from the last line of work.
    assert_eq!(tracker.state(), ImportState::Converting);
    assert_eq!(tracker.bound_kind(), "conversion");
    assert_eq!(tracker.overdue(spawned, at(spawned, 64), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 1803), &BOUNDS),
      Some(Overdue::Stalled),
      "a server that never answers is stopped by the long bound"
    );
    // The server answers: no conversion is running, and the server said why.
    tracker.ready();
    assert_eq!(tracker.state(), ImportState::NoOp);
    assert_eq!(tracker.bound_kind(), "normal");
    assert_eq!(tracker.progress(), None);
    assert_eq!(tracker.error(), None);
    assert_eq!(tracker.warnings().len(), 2, "both warnings are shown");
    assert!(tracker.warnings()[0].starts_with("could not finish the earlier archive rename: "));
    assert_eq!(
      tracker.verifier_pass(),
      Some("sqlite import: verifier PASS: 63 tables"),
      "what the start proved is still reported"
    );

    // Every line of conversion work, then the answer, and no warning: an ordinary start.
    // The server converts before it listens, so an answer means that nothing runs.
    for work in REAL_PROGRESS {
      let mut answered = Tracker::default();
      answered.observe(PREPARING, at(spawned, 1));
      answered.observe(work, at(spawned, 3));
      assert_eq!(answered.state(), ImportState::Converting, "{work}");
      answered.ready();
      assert_eq!(answered.state(), ImportState::Idle, "{work}");
      assert_eq!(answered.progress(), None);
      // Lines that were still on their way when the server answered are read all the
      // same: the end of a conversion is not lost.
      answered.observe("sqlite import: rename complete: 0.079s", at(spawned, 4));
      assert_eq!(answered.state(), ImportState::Converting);
      answered.observe("sqlite import: complete: 3.151s total", at(spawned, 4));
      assert_eq!(answered.state(), ImportState::Done, "{work}");
    }
  }

  /// A start that is refused after kept copies were read (the server's import fix round
  /// 6): `preparing`, one scan line before each kept file whose rows are counted, the
  /// error line, and the server ends. A kept copy can be as large as the database, so
  /// each scan line gives the long bound again; the error line is kept whole.
  #[test]
  fn scans_of_kept_copies_before_a_refusal_are_each_waited_for_and_the_error_is_whole() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    assert_eq!(tracker.state(), ImportState::Preparing);
    tracker.observe(SCAN_LINE, at(spawned, 2));
    assert_eq!(tracker.state(), ImportState::Converting);
    // The normal bound of 60 seconds does not apply after the first scan line.
    assert_eq!(tracker.overdue(spawned, at(spawned, 63), &BOUNDS), None);
    assert_eq!(tracker.overdue(spawned, at(spawned, 1700), &BOUNDS), None);
    // The second kept file, 28 minutes later: the long bound counts from its line.
    tracker.observe(
      "sqlite import: source inventory start: 2147483648 bytes",
      at(spawned, 1700),
    );
    assert_eq!(tracker.overdue(spawned, at(spawned, 1802), &BOUNDS), None);
    assert_eq!(tracker.overdue(spawned, at(spawned, 3499), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 3500), &BOUNDS),
      Some(Overdue::Stalled),
      "one file that takes more than 30 minutes without a line is stopped"
    );
    assert_eq!(tracker.progress_lines(), 2);
    // The refusal.
    let line = long_refusal();
    assert!(
      line.chars().count() > crate::logbuf::MAX_LINE_CHARS,
      "longer than an ordinary log line may be: {}",
      line.chars().count()
    );
    tracker.observe(&line, at(spawned, 2000));
    assert_eq!(tracker.state(), ImportState::Failed);
    assert_eq!(tracker.bound_kind(), "failed");
    assert_eq!(tracker.progress(), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 2000), &BOUNDS),
      Some(Overdue::Failed),
      "no bound is waited out"
    );
    // The whole text, to its last word: the instructions are at the end.
    assert_eq!(
      tracker.error(),
      Some(&line["sqlite import: error: ".len()..])
    );
    let shown = tracker.failure_text();
    assert!(
      shown.starts_with("The database conversion failed: recovery stage: the SQLite file /Users/José Müller/Library/Application Support/komiq desktop/server/komika.sqlite3 named"),
      "{shown}"
    );
    assert!(
      shown.ends_with("-wal out of the data directory and start again"),
      "{shown}"
    );
    assert!(shown.contains("(comments: 4, user_library: 12, users: 3, work: 15)"));
    // The server process ends after the line; nothing it or the wait does changes that.
    tracker.ready();
    tracker.observe(SCAN_LINE, at(spawned, 2001));
    assert_eq!(tracker.state(), ImportState::Failed);
  }

  /// A start that is refused after the server compared the copies (startup.rs `copies`):
  /// `preparing`, the scan line, and the one error line.
  #[test]
  fn a_scan_before_a_refusal_is_waited_for_and_then_fails_at_once() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    tracker.observe(SCAN_LINE, at(spawned, 2));
    // 61 seconds after the scan line: not overdue.
    assert_eq!(tracker.overdue(spawned, at(spawned, 63), &BOUNDS), None);
    tracker.observe(REFUSED, at(spawned, 64));
    assert_eq!(tracker.state(), ImportState::Failed);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 64), &BOUNDS),
      Some(Overdue::Failed),
      "no bound is waited out"
    );
    assert!(tracker
      .failure_text()
      .starts_with("The database conversion failed: recovery stage: "));
  }

  /// The scan line gives a scan the long bound and no more: a scan that prints nothing
  /// further is stopped after the inactivity bound, and by the total cap.
  #[test]
  fn a_scan_that_truly_stalls_is_still_stopped() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    tracker.observe(SCAN_LINE, at(spawned, 2));
    assert_eq!(tracker.overdue(spawned, at(spawned, 1801), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 1802), &BOUNDS),
      Some(Overdue::Stalled),
      "30 minutes after the scan line"
    );
    // A warning in the middle (and the warning of the finished import) extends nothing.
    tracker.observe(DONE_LEFT_IN_PLACE, at(spawned, 1700));
    tracker.observe(KEPT_COPY, at(spawned, 1750));
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 1802), &BOUNDS),
      Some(Overdue::Stalled)
    );
    // Scan line after scan line, for ever: the total cap.
    let mut endless = Tracker::default();
    let mut clock = 0;
    while clock < 6 * 3600 {
      endless.observe(SCAN_LINE, at(spawned, clock));
      clock += 600;
    }
    assert_eq!(
      endless.overdue(spawned, at(spawned, 6 * 3600), &BOUNDS),
      Some(Overdue::TooLong)
    );
  }

  /// The two warnings about the owner's data are kept and stand first, also when fifty
  /// other warnings came before them.
  #[test]
  fn the_two_warnings_about_the_owners_data_survive_a_full_list() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 0));
    tracker.observe("sqlite import: copy start", at(spawned, 1));
    let unrecorded = |n: usize| {
      format!(
        "sqlite import: warning: unrecorded file /data/komika.sqlite3.import-work-{n}; \
         leaving it alone"
      )
    };
    for n in 0..MAX_WARNINGS {
      tracker.observe(&unrecorded(n), at(spawned, 2));
    }
    assert_eq!(tracker.warnings().len(), MAX_WARNINGS, "the list is full");
    tracker.observe(&unrecorded(MAX_WARNINGS), at(spawned, 3));
    assert_eq!(
      tracker.warnings().len(),
      MAX_WARNINGS,
      "one more ordinary warning is dropped"
    );
    tracker.observe(KEPT_COPY, at(spawned, 4));
    tracker.observe(SMALLER, at(spawned, 5));
    let kept = &KEPT_COPY[PREFIX.len() + " warning: ".len()..];
    let smaller = &SMALLER[PREFIX.len() + " warning: ".len()..];
    assert_eq!(tracker.warnings().len(), MAX_WARNINGS + 2);
    assert_eq!(tracker.warnings()[0], kept, "first in the list");
    assert_eq!(tracker.warnings()[1], smaller);
    assert!(tracker.warnings()[2].starts_with("unrecorded file "));
    assert!(tracker
      .warnings()
      .iter()
      .skip(2)
      .all(|warning| warning.starts_with("unrecorded file ")));
    // Each once, however often the server says it.
    tracker.observe(KEPT_COPY, at(spawned, 6));
    assert_eq!(tracker.warnings().len(), MAX_WARNINGS + 2);
    // After an error line too (the copy stage prints them, and a later stage can fail).
    let mut failed = Tracker::default();
    failed.observe(REFUSED, at(spawned, 1));
    for n in 0..MAX_WARNINGS {
      failed.observe(&unrecorded(n), at(spawned, 2));
    }
    failed.observe(SMALLER, at(spawned, 3));
    assert_eq!(failed.warnings()[0], smaller);
    assert_eq!(failed.warnings().len(), MAX_WARNINGS + 1);
    // In the order the server prints them now (before the left-over files): first as well,
    // and the ordinary ones are still capped at fifty.
    let mut ordered = Tracker::default();
    ordered.observe(KEPT_COPY, at(spawned, 1));
    ordered.observe(SMALLER, at(spawned, 1));
    for n in 0..MAX_WARNINGS + 5 {
      ordered.observe(&unrecorded(n), at(spawned, 2));
    }
    assert_eq!(ordered.warnings().len(), MAX_WARNINGS + 2);
    assert_eq!(ordered.warnings()[0], kept);
    assert_eq!(ordered.warnings()[1], smaller);
  }

  #[test]
  fn no_import_file_keeps_the_normal_bound_from_spawn() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe("listening soon", at(spawned, 1));
    assert_eq!(tracker.state(), ImportState::Idle);
    assert_eq!(tracker.bound_kind(), "normal");
    assert_eq!(tracker.overdue(spawned, at(spawned, 59), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 60), &BOUNDS),
      Some(Overdue::Start)
    );
  }

  #[test]
  fn checking_is_bounded_on_its_own_by_the_normal_bound() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 10));
    assert_eq!(tracker.state(), ImportState::Preparing);
    assert_eq!(tracker.progress(), Some(PREPARING));
    assert_eq!(tracker.bound_kind(), "normal");
    assert_eq!(tracker.overdue(spawned, at(spawned, 69), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 70), &BOUNDS),
      Some(Overdue::Preparing)
    );
  }

  /// The server prints `preparing` only when an import, a resume or the completion of an
  /// archive is really attempted; an earlier server printed it on every start that found
  /// a file or a record. Followed by nothing, it is neither a failure nor a conversion,
  /// whichever server printed it.
  #[test]
  fn checking_followed_by_nothing_is_an_ordinary_start() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    tracker.observe("listening on http://127.0.0.1:1/graphql", at(spawned, 2));
    assert_eq!(tracker.state(), ImportState::Preparing);
    assert_eq!(tracker.bound_kind(), "normal");
    assert_eq!(tracker.error(), None);
    assert!(tracker.warnings().is_empty());
    assert_eq!(tracker.progress_lines(), 0);
    // Not answering is over at the normal bound, never the conversion's.
    assert_eq!(tracker.overdue(spawned, at(spawned, 60), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 61), &BOUNDS),
      Some(Overdue::Preparing)
    );
    // Answering: an ordinary started server, with nothing left to show.
    tracker.ready();
    assert_eq!(tracker.state(), ImportState::Idle);
    assert_eq!(tracker.progress(), None);
    assert_eq!(tracker.error(), None);
    // A server that prints no import line at all is the same.
    let mut silent = Tracker::default();
    silent.ready();
    assert_eq!(silent.state(), ImportState::Idle);
    // `ready` never hides a conversion or a failure.
    let mut done = Tracker::default();
    done.observe("sqlite import: complete: 1.151s total", at(spawned, 3));
    done.ready();
    assert_eq!(done.state(), ImportState::Done);
  }

  #[test]
  fn a_restart_line_begins_the_conversion_again_and_extends_no_running_wait() {
    const RESTART: &str =
      "sqlite import: restarting from the beginning: the work copy is older than the source";
    assert_eq!(classify(RESTART), Some(Line::Restart));
    let spawned = Instant::now();
    // Straight after the check: a conversion, waited for by the inactivity bound.
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    tracker.observe(RESTART, at(spawned, 2));
    assert_eq!(tracker.state(), ImportState::Converting);
    assert_eq!(tracker.bound_kind(), "conversion");
    assert_eq!(tracker.progress(), Some(RESTART));
    assert_eq!(
      tracker.progress_lines(),
      0,
      "the line itself is not progress"
    );
    assert_eq!(tracker.overdue(spawned, at(spawned, 1801), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 1802), &BOUNDS),
      Some(Overdue::Stalled)
    );
    // In the middle of a conversion: earlier proof is void, the stage lines that follow
    // are the progress, and the line alone gives a stalled conversion no more time.
    let mut running = Tracker::default();
    running.observe("sqlite import: verify start", at(spawned, 10));
    running.observe("sqlite import: verifier PASS: 63 tables", at(spawned, 20));
    assert!(running.verifier_pass().is_some());
    running.observe(RESTART, at(spawned, 1000));
    assert_eq!(running.state(), ImportState::Converting);
    assert_eq!(running.verifier_pass(), None);
    assert_eq!(
      running.overdue(spawned, at(spawned, 1820), &BOUNDS),
      Some(Overdue::Stalled),
      "30 minutes after the last progress line"
    );
    running.observe("sqlite import: copy start", at(spawned, 1001));
    assert_eq!(running.overdue(spawned, at(spawned, 1820), &BOUNDS), None);
    assert_eq!(running.progress_lines(), 3);
    // After an error nothing restarts the wait.
    let mut failed = Tracker::default();
    failed.observe(
      "sqlite import: error: copy stage: disk full",
      at(spawned, 1),
    );
    failed.observe(RESTART, at(spawned, 2));
    assert_eq!(failed.state(), ImportState::Failed);
  }

  #[test]
  fn a_warning_is_a_no_op_with_the_normal_bound_from_that_moment() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    tracker.observe(NOOP, at(spawned, 2));
    assert_eq!(tracker.state(), ImportState::NoOp);
    assert_eq!(tracker.progress(), None, "nothing is being converted");
    assert_eq!(tracker.progress_lines(), 0);
    assert_eq!(tracker.warnings().len(), 1);
    assert!(tracker.warnings()[0].starts_with("PostgreSQL already holds data"));
    assert_eq!(tracker.bound_kind(), "normal");
    assert_eq!(tracker.overdue(spawned, at(spawned, 61), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 62), &BOUNDS),
      Some(Overdue::AfterNoOp),
      "a hung ordinary start is not given the conversion allowance"
    );
    // The same warning again is kept once.
    tracker.observe(NOOP, at(spawned, 3));
    assert_eq!(tracker.warnings().len(), 1);
  }

  #[test]
  fn an_unknown_import_line_is_not_progress() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 0));
    tracker.observe(
      "sqlite import: something a later server version prints",
      at(spawned, 50),
    );
    assert_eq!(tracker.state(), ImportState::Preparing);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 60), &BOUNDS),
      Some(Overdue::Preparing),
      "the unknown line did not restart the bound"
    );
  }

  #[test]
  fn a_conversion_lives_on_progress_and_a_stalled_one_is_stopped() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    tracker.observe(REAL_PROGRESS[0], at(spawned, 2));
    assert_eq!(tracker.state(), ImportState::Converting);
    assert_eq!(tracker.bound_kind(), "conversion");
    // Far past the normal bound, alive because lines keep coming.
    let mut clock = 2;
    let mut lines = REAL_PROGRESS.iter().cycle();
    while clock + 1700 < 6 * 3600 {
      clock += 1700;
      assert_eq!(tracker.overdue(spawned, at(spawned, clock), &BOUNDS), None);
      tracker.observe(lines.next().unwrap(), at(spawned, clock));
    }
    // A warning or an unknown line in the middle extends nothing.
    let mut stalled = Tracker::default();
    stalled.observe(PREPARING, at(spawned, 0));
    stalled.observe("sqlite import: copy start", at(spawned, 10));
    stalled.observe(
      "sqlite import: warning: unrecorded file /data/komika.sqlite3.import-work-old; leaving it alone",
      at(spawned, 1000),
    );
    stalled.observe(
      "sqlite import: precheck failed: 2 failing rules",
      at(spawned, 1500),
    );
    assert_eq!(stalled.state(), ImportState::Converting);
    assert_eq!(stalled.warnings().len(), 1);
    assert_eq!(stalled.overdue(spawned, at(spawned, 1809), &BOUNDS), None);
    assert_eq!(
      stalled.overdue(spawned, at(spawned, 1810), &BOUNDS),
      Some(Overdue::Stalled),
      "30 minutes after the last progress line, whatever else was printed"
    );
  }

  #[test]
  fn the_total_cap_ends_a_conversion_that_never_stalls() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    let mut clock = 0;
    while clock < 6 * 3600 {
      tracker.observe(
        "sqlite import: table chapter: 1 rows sent, 5.0s",
        at(spawned, clock),
      );
      clock += 600;
    }
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 6 * 3600), &BOUNDS),
      Some(Overdue::TooLong)
    );
  }

  #[test]
  fn an_error_line_fails_at_once_and_shows_the_repair_lines() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 1));
    tracker.observe("sqlite import: repair start", at(spawned, 2));
    tracker.observe(
      "sqlite import: repair: chapter: 2 rows would change",
      at(spawned, 3),
    );
    tracker.observe(
      "sqlite import: repair: chapter: 2 rows applied",
      at(spawned, 3),
    );
    tracker.observe(
      "sqlite import: error: verify stage: verifier did not pass",
      at(spawned, 4),
    );
    assert_eq!(tracker.state(), ImportState::Failed);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 4), &BOUNDS),
      Some(Overdue::Failed),
      "no bound is waited out"
    );
    assert_eq!(
      tracker.failure_text(),
      "The database conversion failed: verify stage: verifier did not pass\n\
       Repair step output:\n  chapter: 2 rows would change\n  chapter: 2 rows applied"
    );
    // Nothing printed afterwards revives the start.
    tracker.observe("sqlite import: copy start", at(spawned, 5));
    tracker.observe("sqlite import: complete: 1.0s total", at(spawned, 6));
    assert_eq!(tracker.state(), ImportState::Failed);
    // An error before any repair output is shown alone.
    let mut early = Tracker::default();
    early.observe(
      "sqlite import: error: decision stage: unfinished import has no SQLite source; refusing to serve",
      at(spawned, 1),
    );
    assert_eq!(
      early.failure_text(),
      "The database conversion failed: decision stage: unfinished import has no SQLite source; refusing to serve"
    );
  }

  #[test]
  fn a_finished_conversion_returns_to_the_normal_bound() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 0));
    for (i, line) in REAL_PROGRESS.iter().enumerate() {
      tracker.observe(line, at(spawned, 100 + i as u64));
    }
    assert_eq!(
      tracker.verifier_pass(),
      Some("sqlite import: verifier PASS: 63 tables")
    );
    assert!(tracker
      .renamed()
      .unwrap()
      .ends_with("komika.sqlite3.imported-20261011T115920Z"));
    assert_eq!(tracker.progress_lines(), REAL_PROGRESS.len() as u64);
    tracker.observe("sqlite import: complete: 1.151s total", at(spawned, 5000));
    assert_eq!(tracker.state(), ImportState::Done);
    assert_eq!(tracker.bound_kind(), "normal");
    assert_eq!(tracker.progress(), None);
    assert_eq!(tracker.overdue(spawned, at(spawned, 5059), &BOUNDS), None);
    assert_eq!(
      tracker.overdue(spawned, at(spawned, 5060), &BOUNDS),
      Some(Overdue::AfterDone)
    );
  }

  #[test]
  fn a_warning_while_checking_does_not_block_a_conversion_that_follows() {
    let spawned = Instant::now();
    let mut tracker = Tracker::default();
    tracker.observe(PREPARING, at(spawned, 0));
    tracker.observe(
      "sqlite import: warning: unrecorded file /data/komika.sqlite3.import-probe-x; leaving it alone",
      at(spawned, 1),
    );
    assert_eq!(tracker.state(), ImportState::NoOp);
    tracker.observe(REAL_PROGRESS[0], at(spawned, 2));
    assert_eq!(tracker.state(), ImportState::Converting);
    assert_eq!(tracker.overdue(spawned, at(spawned, 1000), &BOUNDS), None);
  }
}
