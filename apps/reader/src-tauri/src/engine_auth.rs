//! The embedded engine's access control, shared by every target that runs an engine:
//! the desktop child-process supervisor (`suwayomi.rs`, also compiled into the admin
//! shell) and the in-process phone engine (`suwayomi_inprocess.rs`). Each of those declares this
//! file as a child module by `#[path]`, so there is one implementation and no host crate
//! has to know about it.
//!
//! Loopback is not an authorization boundary: a `127.0.0.1` bind keeps out the network,
//! not other software on the same machine or phone. The engine's GraphQL API can add
//! extension repos and install extensions, i.e. run arbitrary JVM code in the engine's
//! process, so it runs behind HTTP Basic auth with a credential made once per app run.
//! The credential exists in two places only: this process's memory, and the `server.conf`
//! the engine reads (inside the app's own data dir). It is never logged.

use std::sync::OnceLock;
use std::time::Duration;

/// How long the client keeps an unused connection to the engine. It must stay below the
/// engine's own idle limit of 30 s (see `build_client`).
const ENGINE_IDLE_CONNECTION: Duration = Duration::from_secs(20);

/// Basic-auth username for the embedded engine; the password is the per-run secret.
pub(crate) const ENGINE_AUTH_USER: &str = "komika";

/// The engine's Basic-auth password, generated once per app run. The conf is rewritten
/// with it before every (re)launch and the supervisor's client sends it on every request,
/// so an in-run engine restart keeps working while a credential never outlives the process.
pub(crate) fn engine_password() -> &'static str {
  static PASSWORD: OnceLock<String> = OnceLock::new();
  PASSWORD.get_or_init(random_token)
}

/// 128 bits of process-local entropy as hex, without adding an RNG dependency for one
/// loopback credential: `RandomState`'s SipHash keys are seeded from the OS the first
/// time the process uses them and are not observable from another process.
fn random_token() -> String {
  use std::collections::hash_map::RandomState;
  use std::hash::{BuildHasher, Hasher};
  let nonce = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_nanos() as u64)
    .unwrap_or_default()
    ^ u64::from(std::process::id());
  let mut token = String::with_capacity(32);
  for round in 0..2u64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(nonce ^ round);
    token.push_str(&format!("{:016x}", hasher.finish()));
  }
  token
}

/// Render an `Authorization: Basic …` value. Base64 is inlined rather than pulled in as a
/// dependency: this is the only encoder the crate needs and the input shape is fixed.
fn basic_auth_value(user: &str, password: &str) -> String {
  const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let raw = format!("{user}:{password}");
  let bytes = raw.as_bytes();
  let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
  for chunk in bytes.chunks(3) {
    let triple = ((chunk[0] as u32) << 16)
      | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
      | (*chunk.get(2).unwrap_or(&0) as u32);
    encoded.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
    encoded.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
    // The trailing group is padded, not truncated: 1 byte → "xx==", 2 bytes → "xxx=".
    encoded.push(if chunk.len() > 1 {
      ALPHABET[((triple >> 6) & 0x3f) as usize] as char
    } else {
      '='
    });
    encoded.push(if chunk.len() > 2 {
      ALPHABET[(triple & 0x3f) as usize] as char
    } else {
      '='
    });
  }
  format!("Basic {encoded}")
}

/// Build the engine's HTTP client. The credential rides as a default header so every
/// caller of this client — the readiness poll, the iOS heartbeat, the
/// `suwayomi_gql`/`suwayomi_image` proxies, and desktop's `cloudflare::apply_settings`
/// (handed a clone) — is authenticated without the secret being threaded through each
/// call site. `timeout` is per request and differs per target (the iOS interpreter is slow).
pub(crate) fn build_client(timeout: Duration) -> reqwest::Client {
  let mut auth =
    reqwest::header::HeaderValue::from_str(&basic_auth_value(ENGINE_AUTH_USER, engine_password()))
      .expect("basic auth header is ascii");
  // Keeps the value out of `Debug` output of the header map and of requests.
  auth.set_sensitive(true);
  let mut headers = reqwest::header::HeaderMap::new();
  headers.insert(reqwest::header::AUTHORIZATION, auth);
  reqwest::Client::builder()
    .timeout(timeout)
    // The engine's HTTP server (Jetty) closes a connection that was idle for 30 s. A
    // request sent on a kept connection at that same moment fails at once, and the 30 s
    // heartbeat hit exactly that ("heartbeat missed (1/3)" on an engine that was fine).
    // So this side lets go of an idle connection first.
    .pool_idle_timeout(ENGINE_IDLE_CONNECTION)
    .default_headers(headers)
    // No redirect policy / SSRF host guard here on purpose: loopback is allowed.
    .build()
    .expect("build suwayomi http client")
}

/// Render the authoritative HOCON `server.conf`. The persisted conf is authoritative
/// (JVM `-Dserver.*` sysprops do NOT reliably override Suwayomi's reactive config), so
/// this is rewritten before every launch. `kcefEnabled = false` is CRITICAL on desktop
/// (else the server downloads 226 MB of Chromium and SIGTRAPs headless) and
/// harmless-but-correct on a phone.
///
/// The `authMode`/`authUsername`/`authPassword` trio is v2.3.2243's authentication
/// surface (the older `basicAuth*` keys are deprecated and migrate onto these), and
/// `BASIC_AUTH` is the enum constant name the config parser expects. It is not optional
/// hardening — see the module docs.
pub(crate) fn render_server_conf(port: u16) -> String {
  format!(
    "server.ip = \"127.0.0.1\"\n\
     server.port = {port}\n\
     server.webUIEnabled = false\n\
     server.initialOpenInBrowserEnabled = false\n\
     server.systemTrayEnabled = false\n\
     server.kcefEnabled = false\n\
     server.flareSolverrEnabled = false\n\
     server.authMode = \"BASIC_AUTH\"\n\
     server.authUsername = \"{user}\"\n\
     server.authPassword = \"{password}\"\n",
    user = ENGINE_AUTH_USER,
    password = engine_password()
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn server_conf_has_required_lines_and_kcef_off() {
    let conf = render_server_conf(54321);
    assert!(conf.contains("server.ip = \"127.0.0.1\""));
    assert!(conf.contains("server.port = 54321"));
    assert!(conf.contains("server.webUIEnabled = false"));
    assert!(conf.contains("server.initialOpenInBrowserEnabled = false"));
    assert!(conf.contains("server.systemTrayEnabled = false"));
    // The two lines that keep the JVM from crashing / phoning out.
    assert!(conf.contains("server.kcefEnabled = false"));
    assert!(conf.contains("server.flareSolverrEnabled = false"));
    // Auth is mandatory: loopback keeps remote callers out, not local ones.
    assert!(conf.contains("server.authMode = \"BASIC_AUTH\""));
    assert!(conf.contains(&format!("server.authUsername = \"{ENGINE_AUTH_USER}\"")));
    assert!(conf.contains(&format!("server.authPassword = \"{}\"", engine_password())));
  }

  #[test]
  fn engine_password_is_stable_and_unguessable() {
    let password = engine_password();
    assert_eq!(password, engine_password(), "one credential per process");
    assert_eq!(password.len(), 32, "128 bits of hex");
    assert!(password.chars().all(|c| c.is_ascii_hexdigit()));
    // Two fresh draws must not collide (a constant token would defeat the whole point).
    assert_ne!(random_token(), random_token());
  }

  #[test]
  fn basic_auth_value_matches_rfc_vector() {
    // RFC 7617's own example, which also exercises both padding lengths.
    assert_eq!(
      basic_auth_value("Aladdin", "open sesame"),
      "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
    );
    assert_eq!(basic_auth_value("a", "b"), "Basic YTpi");
    assert_eq!(basic_auth_value("ab", "c"), "Basic YWI6Yw==");
  }

  /// Accept `count` loopback connections, answer each `200 {}`, and hand back the request
  /// heads (request line + headers) as the client actually put them on the wire.
  fn capture_request_heads(count: usize) -> (u16, std::thread::JoinHandle<Vec<String>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind capture listener");
    let port = listener.local_addr().expect("listener addr").port();
    let handle = std::thread::spawn(move || {
      let mut heads = Vec::with_capacity(count);
      for _ in 0..count {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut raw = Vec::new();
        let mut buf = [0u8; 1024];
        while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
          let n = stream.read(&mut buf).expect("read request");
          if n == 0 {
            break;
          }
          raw.extend_from_slice(&buf[..n]);
        }
        let _ = stream
          .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}");
        heads.push(String::from_utf8_lossy(&raw).to_ascii_lowercase());
      }
      heads
    });
    (port, handle)
  }

  /// The credential is a client default, so it must be on the wire for both request
  /// shapes the engine modules make: the GraphQL POST (readiness poll, heartbeat,
  /// `suwayomi_gql`, `cloudflare::apply_settings`) and the image GET (`suwayomi_image`).
  #[tokio::test]
  async fn client_sends_the_credential_on_every_request() {
    let (port, server) = capture_request_heads(2);
    let client = build_client(Duration::from_secs(5));
    client
      .post(format!("http://127.0.0.1:{port}/api/graphql"))
      .header(reqwest::header::CONTENT_TYPE, "application/json")
      .body("{}")
      .send()
      .await
      .expect("post");
    client
      .get(format!("http://127.0.0.1:{port}/api/v1/manga/3/thumbnail"))
      .send()
      .await
      .expect("get");

    let expected = format!(
      "authorization: {}",
      basic_auth_value(ENGINE_AUTH_USER, engine_password()).to_ascii_lowercase()
    );
    let heads = server.join().expect("capture thread");
    assert!(heads[0].starts_with("post /api/graphql "), "head: {}", heads[0]);
    assert!(heads[1].starts_with("get /api/v1/manga/3/thumbnail "), "head: {}", heads[1]);
    for head in &heads {
      assert!(head.contains(&expected), "request carries no engine credential");
    }
  }

  /// `suwayomi_inprocess.rs` compiles for the phone targets only, so its request paths
  /// cannot run in a host `cargo test`. This is the host-side tripwire instead: the file
  /// must take its one client from [`build_client`] and must have no other way to make an
  /// HTTP request, so every call it makes is the authenticated client proven above.
  #[test]
  fn ios_engine_has_no_client_without_the_credential() {
    let source = include_str!("suwayomi_inprocess.rs");
    assert!(
      source.contains("engine_auth::build_client("),
      "the iOS supervisor must build its client with the engine credential"
    );
    for bare in ["Client::builder", "Client::new", "ClientBuilder", "reqwest::get"] {
      assert!(
        !source.contains(bare),
        "suwayomi_inprocess.rs uses `{bare}`: a client built there sends no engine credential"
      );
    }
    assert!(
      source.contains("engine_auth::render_server_conf("),
      "the iOS engine must write the shared, auth-enabled server.conf"
    );
  }
}
