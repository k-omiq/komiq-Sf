//! Bounded, in-memory logs of the engine and the server for the start screen's log
//! view (`local_server_log`).
//!
//! Nothing here is written to disk. That is deliberate: the server prints its one-time
//! setup code in its output, and the engine's credential rides in the server's startup
//! config dump. Every line is cleaned on the way in (ANSI colours stripped, known
//! secrets masked, length capped), so what the view shows is safe to copy into a bug
//! report.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Lines kept per log. A failed boot needs the last screenful or two, not history.
pub const MAX_LINES: usize = 400;
/// Characters kept per line; the rest is cut with an ellipsis.
pub const MAX_LINE_CHARS: usize = 2000;
/// What a masked secret reads as.
pub const MASK: &str = "••••";

/// One ring buffer of cleaned lines.
#[derive(Default)]
pub struct LogBuf {
  lines: Mutex<VecDeque<String>>,
  secrets: Mutex<Vec<String>>,
}

impl LogBuf {
  pub fn new() -> Arc<Self> {
    Arc::new(Self::default())
  }

  /// Mask `secret` in every line pushed from now on.
  pub fn add_secret(&self, secret: &str) {
    if secret.is_empty() {
      return;
    }
    let mut secrets = self.secrets.lock().unwrap_or_else(|e| e.into_inner());
    if !secrets.iter().any(|s| s == secret) {
      secrets.push(secret.to_string());
    }
  }

  pub fn push(&self, raw: &str) {
    let mut line = strip_ansi(raw);
    for secret in self
      .secrets
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .iter()
    {
      if line.contains(secret.as_str()) {
        line = line.replace(secret.as_str(), MASK);
      }
    }
    if line.chars().count() > MAX_LINE_CHARS {
      line = line.chars().take(MAX_LINE_CHARS).collect::<String>() + "…";
    }
    let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
    lines.push_back(line);
    while lines.len() > MAX_LINES {
      lines.pop_front();
    }
  }

  pub fn text(&self) -> String {
    let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
    lines
      .iter()
      .map(String::as_str)
      .collect::<Vec<_>>()
      .join("\n")
  }

  /// Whether any kept line contains `needle` (used to name a boot failure's cause).
  pub fn contains(&self, needle: &str) -> bool {
    let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
    lines.iter().any(|l| l.contains(needle))
  }
}

/// Drop ANSI escape sequences (CSI `ESC [ … letter` and two-byte `ESC x`).
pub fn strip_ansi(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  let mut chars = s.chars().peekable();
  while let Some(c) = chars.next() {
    if c != '\u{1b}' {
      out.push(c);
      continue;
    }
    // `ESC [` starts a CSI sequence: skip up to and including its final byte (@..~).
    // Any other byte after ESC is a two-byte sequence and is dropped with it.
    if chars.next() == Some('[') {
      for c in chars.by_ref() {
        if ('@'..='~').contains(&c) {
          break;
        }
      }
    }
  }
  out
}

/// The server's first-run line reads `… enter setup code XXXX-XXXX-XXXX (…)`
/// (apps/server/src/instance.rs). Returns the code when `line` carries one.
pub fn find_setup_code(line: &str) -> Option<String> {
  const MARK: &str = "setup code ";
  let at = line.find(MARK)? + MARK.len();
  let code: String = line[at..]
    .chars()
    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
    .collect();
  let groups: Vec<&str> = code.split('-').collect();
  let well_formed = groups.len() == 3
    && groups.iter().all(|g| {
      g.len() == 4
        && g
          .chars()
          .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    });
  well_formed.then_some(code)
}

/// A `log` sink that files the shared engine supervisor's lines (target `suwayomi`, see
/// `apps/reader/src-tauri/src/suwayomi.rs`) into the engine log. Everything else from this
/// app goes to stderr only, so a `tauri dev` terminal still shows it.
pub struct Logger {
  pub engine: Arc<LogBuf>,
}

impl log::Log for Logger {
  fn enabled(&self, metadata: &log::Metadata) -> bool {
    metadata.level() <= log::Level::Info
      && (metadata.target() == "suwayomi" || metadata.target().starts_with("app_lib"))
  }

  fn log(&self, record: &log::Record) {
    if !self.enabled(record.metadata()) {
      return;
    }
    if record.target() == "suwayomi" {
      self.engine.push(&record.args().to_string());
    } else {
      eprintln!("[{}] {}", record.level(), record.args());
    }
  }

  fn flush(&self) {}
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ring_keeps_the_last_lines() {
    let buf = LogBuf::default();
    for i in 0..(MAX_LINES + 25) {
      buf.push(&format!("line {i}"));
    }
    let text = buf.text();
    assert_eq!(text.lines().count(), MAX_LINES);
    assert!(text.starts_with("line 25\n"));
    assert!(text.ends_with(&format!("line {}", MAX_LINES + 24)));
  }

  #[test]
  fn long_lines_are_cut() {
    let buf = LogBuf::default();
    buf.push(&"x".repeat(MAX_LINE_CHARS * 2));
    assert_eq!(buf.text().chars().count(), MAX_LINE_CHARS + 1);
  }

  #[test]
  fn secrets_and_colours_are_removed() {
    let buf = LogBuf::default();
    buf.add_secret("hunter2hunter2");
    buf.push(
      "\u{1b}[2m2026\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m url=http://u:hunter2hunter2@127.0.0.1:1",
    );
    assert_eq!(
      buf.text(),
      format!("2026  INFO url=http://u:{MASK}@127.0.0.1:1")
    );
  }

  #[test]
  fn setup_code_is_found_in_the_server_line() {
    let line =
      "2026-10-04T10:00:00Z  WARN komika_server::instance: no admin account yet: open the \
                setup screen and enter setup code ABCD-2345-WXYZ (valid until the owner account \
                is created; a restart issues a new code)";
    assert_eq!(find_setup_code(line).as_deref(), Some("ABCD-2345-WXYZ"));
    assert_eq!(
      find_setup_code("listening on http://127.0.0.1:1/graphql"),
      None
    );
    assert_eq!(find_setup_code("setup code abcd"), None);
    assert_eq!(find_setup_code("setup code ABCD-2345"), None);
  }
}
