//! Android only: a loopback HTTP door for engine images.
//!
//! On the other platforms the web view loads an engine image from the `engine-image`
//! address scheme (see `lib.rs`). On Android that scheme has a cost: the web view asks
//! the app about every request on one queue (`shouldInterceptRequest`), and wry waits on
//! that queue until a custom-scheme request has its answer. A page image that waits for
//! a slow source then holds back every other request of the app. Measured on the
//! emulator, 2026-10-06: a 5 ms request for an app file took 1 to 6 s while pages loaded.
//!
//! So on Android the pages get a plain `http://127.0.0.1:<port>/<token>/api/v1/…`
//! address instead. The web view loads it as a normal network request, the queue stays
//! free, and several pages load at the same time.
//!
//! Loopback is not an authorization boundary (other apps on the phone can connect), so
//! every request must carry the token of this app run as its first path segment. The
//! token is not the engine's password: it opens engine image GETs only, behind the same
//! path guard as every other engine image (`suwayomi::engine_image`). The page never
//! sees the engine's port or its credential.

// The request rules are tested on the host, where nothing starts the listener.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use std::io::Read;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::suwayomi::{self, SuwayomiSupervisor};

/// A request head (request line and headers) may not be longer than this.
const MAX_HEAD_BYTES: usize = 8 * 1024;
/// A connection must send its whole request head within this time.
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections served at one time; one above this is closed at once. The web view opens
/// six per host, so this limit only stops a local caller that opens connections in bulk.
const MAX_CONNECTIONS: usize = 32;

/// The address prefix for engine images, `http://127.0.0.1:<port>/<token>`. The first
/// call starts the listener. `None` when it cannot start; the caller then keeps the
/// `engine-image` scheme, which works but is slow on Android.
pub fn base_url(supervisor: &Arc<SuwayomiSupervisor>) -> Option<&'static str> {
  static BASE: OnceLock<Option<String>> = OnceLock::new();
  BASE
    .get_or_init(|| match start(supervisor.clone()) {
      Ok(base) => Some(base),
      Err(err) => {
        log::warn!(target: "suwayomi", "engine image listener did not start: {err}");
        None
      }
    })
    .as_deref()
}

fn start(supervisor: Arc<SuwayomiSupervisor>) -> Result<String, String> {
  let token = random_token().map_err(|e| format!("no random token: {e}"))?;
  let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
  listener.set_nonblocking(true).map_err(|e| e.to_string())?;
  let port = listener.local_addr().map_err(|e| e.to_string())?.port();
  let base = format!("http://127.0.0.1:{port}/{token}");
  tauri::async_runtime::spawn(accept_loop(listener, token, supervisor));
  Ok(base)
}

/// 128 bits from the OS as hex. Never logged.
fn random_token() -> std::io::Result<String> {
  let mut bytes = [0u8; 16];
  std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
  Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

async fn accept_loop(
  listener: std::net::TcpListener,
  token: String,
  supervisor: Arc<SuwayomiSupervisor>,
) {
  let listener = match TcpListener::from_std(listener) {
    Ok(listener) => listener,
    Err(err) => {
      log::warn!(target: "suwayomi", "engine image listener stopped: {err}");
      return;
    }
  };
  let token: Arc<str> = token.into();
  let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
  loop {
    let stream = match listener.accept().await {
      Ok((stream, _)) => stream,
      Err(_) => {
        // Usually "too many open files". Wait, do not spin.
        tokio::time::sleep(Duration::from_millis(100)).await;
        continue;
      }
    };
    let Ok(permit) = connections.clone().try_acquire_owned() else {
      continue;
    };
    let token = token.clone();
    let supervisor = supervisor.clone();
    tauri::async_runtime::spawn(async move {
      let _permit = permit;
      serve(stream, &token, &supervisor).await;
    });
  }
}

/// One request per connection, then close. Loopback connections are cheap to open, and
/// this keeps the server free of keep-alive state.
async fn serve(mut stream: TcpStream, token: &str, supervisor: &SuwayomiSupervisor) {
  let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
    Ok(Some(head)) => head,
    _ => return,
  };
  let (status, content_type, body) = match engine_path(&head, token) {
    Ok(path) => match suwayomi::engine_image(supervisor, &path).await {
      Ok((bytes, content_type)) => (
        "200 OK",
        content_type.unwrap_or_else(|| "application/octet-stream".to_owned()),
        bytes,
      ),
      Err(err) => {
        log::warn!(target: "suwayomi", "engine image {path}: {err}");
        ("502 Bad Gateway", TEXT.to_owned(), err.into_bytes())
      }
    },
    Err(status) => (status, TEXT.to_owned(), Vec::new()),
  };
  // `no-store`: the port and the token change with each app run, so a stored copy can
  // never be used again. The engine keeps the pages on disk itself.
  let head = format!(
    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
     cache-control: no-store\r\nx-content-type-options: nosniff\r\nconnection: close\r\n\r\n",
    body.len()
  );
  if stream.write_all(head.as_bytes()).await.is_ok() {
    let _ = stream.write_all(&body).await;
  }
  let _ = stream.shutdown().await;
}

const TEXT: &str = "text/plain; charset=utf-8";

/// Read up to the empty line that ends the request head. `None` when the peer closes
/// first or the head is too long.
async fn read_head(stream: &mut TcpStream) -> Option<Vec<u8>> {
  let mut head = Vec::new();
  let mut chunk = [0u8; 1024];
  while !head.windows(4).any(|w| w == b"\r\n\r\n") {
    let read = stream.read(&mut chunk).await.ok()?;
    if read == 0 || head.len() + read > MAX_HEAD_BYTES {
      return None;
    }
    head.extend_from_slice(&chunk[..read]);
  }
  Some(head)
}

/// The engine path a request asks for (`/api/v1/…`), or the status line to refuse it
/// with. Only `GET /<token>/…` passes. The path itself is not checked here:
/// `suwayomi::engine_image` does that for every caller.
fn engine_path(head: &[u8], token: &str) -> Result<String, &'static str> {
  const BAD_REQUEST: &str = "400 Bad Request";
  let line = head.split(|b| *b == b'\r').next().unwrap_or_default();
  let line = std::str::from_utf8(line).map_err(|_| BAD_REQUEST)?;
  let mut parts = line.split(' ');
  let (Some(method), Some(target), Some(version), None) =
    (parts.next(), parts.next(), parts.next(), parts.next())
  else {
    return Err(BAD_REQUEST);
  };
  if !version.starts_with("HTTP/1.") {
    return Err(BAD_REQUEST);
  }
  if method != "GET" {
    return Err("405 Method Not Allowed");
  }
  let rest = target.strip_prefix('/').ok_or(BAD_REQUEST)?;
  let (given, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
  if !same_bytes(given.as_bytes(), token.as_bytes()) {
    return Err("403 Forbidden");
  }
  Ok(path.to_owned())
}

/// Compare without an early exit, so the time taken tells a caller nothing about how
/// much of a guessed token was correct.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
  a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
  use super::*;

  const TOKEN: &str = "0123456789abcdef0123456789abcdef";

  fn head(request_line: &str) -> Vec<u8> {
    format!("{request_line}\r\nhost: 127.0.0.1\r\n\r\n").into_bytes()
  }

  #[test]
  fn a_get_with_the_token_gives_the_engine_path() {
    let request = head(&format!("GET /{TOKEN}/api/v1/manga/3/chapter/7/page/0 HTTP/1.1"));
    assert_eq!(
      engine_path(&request, TOKEN).as_deref(),
      Ok("/api/v1/manga/3/chapter/7/page/0")
    );
    // The query belongs to the engine path.
    let request = head(&format!("GET /{TOKEN}/api/v1/manga/3/thumbnail?x=1 HTTP/1.1"));
    assert_eq!(engine_path(&request, TOKEN).as_deref(), Ok("/api/v1/manga/3/thumbnail?x=1"));
  }

  #[test]
  fn a_request_without_the_token_is_refused() {
    for target in [
      "/api/v1/manga/3/chapter/7/page/0".to_owned(),
      "/".to_owned(),
      format!("/{}/api/v1/manga/3/thumbnail", TOKEN.replace('0', "1")),
      format!("/{}/api/v1/manga/3/thumbnail", &TOKEN[..31]),
      format!("/{TOKEN}0/api/v1/manga/3/thumbnail"),
    ] {
      let request = head(&format!("GET {target} HTTP/1.1"));
      assert_eq!(engine_path(&request, TOKEN), Err("403 Forbidden"), "target {target}");
    }
  }

  #[test]
  fn only_a_well_formed_get_passes() {
    let path = format!("/{TOKEN}/api/v1/manga/3/thumbnail");
    assert_eq!(
      engine_path(&head(&format!("POST {path} HTTP/1.1")), TOKEN),
      Err("405 Method Not Allowed")
    );
    for line in [
      format!("GET {path}"),
      format!("GET {path} HTTP/2"),
      format!("GET {path} HTTP/1.1 extra"),
      format!("GET {} HTTP/1.1", &path[1..]),
      String::new(),
    ] {
      assert_eq!(engine_path(&head(&line), TOKEN), Err("400 Bad Request"), "line {line:?}");
    }
    assert_eq!(engine_path(&[0xff, 0xfe, b'\r', b'\n'], TOKEN), Err("400 Bad Request"));
  }

  #[test]
  fn the_token_is_128_bits_of_hex_and_not_constant() {
    let token = random_token().expect("os randomness");
    assert_eq!(token.len(), 32);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(token, random_token().expect("os randomness"));
  }

  /// The listener end to end, without an engine: the token check and the HTTP framing.
  /// A request with the token reaches the engine module, which has no engine here, so the
  /// answer is the 502 of a failed image.
  #[tokio::test]
  async fn the_listener_answers_over_http() {
    let supervisor = Arc::new(SuwayomiSupervisor::default());
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(accept_loop(listener, TOKEN.to_owned(), supervisor));

    let client = reqwest::Client::new();
    let wrong = client
      .get(format!("http://127.0.0.1:{port}/nope/api/v1/manga/3/thumbnail"))
      .send()
      .await
      .expect("request without token");
    assert_eq!(wrong.status(), 403);
    assert_eq!(wrong.headers()["cache-control"], "no-store");

    let right = client
      .get(format!("http://127.0.0.1:{port}/{TOKEN}/api/v1/manga/3/thumbnail"))
      .send()
      .await
      .expect("request with token");
    assert_eq!(right.status(), 502);
    assert!(right.text().await.expect("body").contains("not ready"));
  }
}
