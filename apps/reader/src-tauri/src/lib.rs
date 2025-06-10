use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// The embedded engine has three per-platform implementations under the SAME
// `suwayomi` module name, so the rest of `run()` (manage/start/invoke_handler/exit)
// stays platform-agnostic:
//   * desktop            -> suwayomi.rs        (child-process JVM supervisor + the
//                           WebView Cloudflare shim `cloudflare.rs`, both desktop-only)
//   * iOS REAL DEVICE    -> suwayomi_ios.rs    (in-process JNI boot of the statically
//                           linked Zero JVM, N4.2 — device only: the vendored .a files
//                           have no simulator slice, so the sim keeps the stub)
//   * other mobile       -> suwayomi_mobile.rs ("engine unavailable" stubs; Android
//                           and the iOS simulator)
//
// The device arm matches the bare-ABI iOS triple by ALLOWLIST (`target_abi = ""`, which
// is what aarch64-apple-ios reports) rather than by excluding `sim`: any other Apple ABI
// — Mac Catalyst is `macabi` — then falls back to the stub instead of pulling in the JNI
// module and build.rs's iOS-device static libs, which cannot link for it. build.rs
// applies the same allowlist to the `jre-ios` link flags; keep the two in step.
#[cfg(desktop)]
mod cloudflare;
#[cfg(desktop)]
mod suwayomi;
#[cfg(all(target_os = "ios", target_abi = ""))]
#[path = "suwayomi_ios.rs"]
mod suwayomi;
#[cfg(all(not(desktop), not(all(target_os = "ios", target_abi = ""))))]
#[path = "suwayomi_mobile.rs"]
mod suwayomi;

/// Total budget for a single native image fetch (connect + transfer).
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on the decoded body — a missing/lying Content-Length can't OOM us.
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024; // 32 MiB
/// Redirect hops we'll follow manually (each one re-validated).
const MAX_REDIRECTS: usize = 5;
/// Wall-clock ceiling for one `fetch_image` call across ALL hops and retries. Without
/// it the worst case is (MAX_REDIRECTS + 1) hops × retries × FETCH_TIMEOUT — minutes of
/// a held permit for a page nobody is still looking at.
const FETCH_TOTAL_BUDGET: Duration = Duration::from_secs(60);
/// Extra attempts per hop after the first. Image GETs are idempotent, so one retry turns
/// a DNS blip or a dropped connection into a short delay instead of a blank page.
const IMAGE_FETCH_RETRIES: usize = 1;
/// Base delay before a retry; scaled by attempt and jittered (see `retry_backoff`).
const RETRY_BASE_BACKOFF: Duration = Duration::from_millis(250);
/// Max concurrent native image fetches. Bounds peak native heap at roughly
/// N × MAX_IMAGE_BYTES so a burst of page prefetches (or a malicious source
/// serving large images) can't OOM/jetsam the app — critical on iOS, where the
/// in-process JVM already runs under `-Xmx256m` in a jetsam-limited process.
const MAX_CONCURRENT_IMAGE_FETCHES: usize = 6;

/// Global permit pool gating every native image command (`fetch_image` here and
/// the loopback `suwayomi_image` proxies in the `suwayomi` module). Shared so the
/// cap is a true process-wide bound on concurrent 32 MiB buffers, not per-command.
pub(crate) fn image_fetch_semaphore() -> &'static tokio::sync::Semaphore {
  static SEM: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
  SEM.get_or_init(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_IMAGE_FETCHES))
}

/// Base config for a native image-fetch client: a request timeout and NO automatic
/// redirect following — we follow manually so every hop's host is re-validated
/// (a 3xx to `http://169.254.169.254/…` must not slip past the host guard).
///
/// Callers add a per-host `.resolve(host, addr)` pin before `.build()`, so we hand
/// back a fresh builder rather than a shared client: the connection then dials
/// exactly the IP that passed `is_blocked_ip`, closing the DNS-rebinding TOCTOU
/// where reqwest would otherwise re-resolve to a second, unchecked address.
fn image_client_builder() -> reqwest::ClientBuilder {
  reqwest::Client::builder()
    .timeout(FETCH_TIMEOUT)
    .redirect(reqwest::redirect::Policy::none())
    // Some CDNs (MangaDex among them) gate on a UA/Referer and 403 requests
    // without them; the Worker sets both, so the native path must match to avoid
    // a web/native divergence on exactly those hosts (I4).
    .user_agent("Komika/0.1 (+https://komika.app)")
}

/// The app's OWN backend origin, as `(host, port)`.
///
/// `NativeImageProvider` routes own-origin assets (`/covers/…`, `/avatars/…`, and the
/// hosted image proxy) through `fetch_image`, so when that backend is a dev server or a
/// self-hosted LAN box the SSRF guard would otherwise blackhole every image in the app.
/// This is the ONE host allowed past the address check — see `validate_image_url`.
struct ApiOrigin {
  host: String,
  port: u16,
}

impl ApiOrigin {
  /// Exact host+port match. Deliberately not a suffix/subnet test: exempting
  /// `localhost:8080` must not also exempt `localhost:22` or any other LAN address.
  fn matches(&self, host: &str, port: u16) -> bool {
    self.port == port && self.host.eq_ignore_ascii_case(host)
  }
}

/// Parse an API origin out of a configured URL (the reader passes its GraphQL endpoint,
/// e.g. `http://localhost:8080/graphql`); the path is dropped, only the origin matters.
fn parse_api_origin(raw: &str) -> Option<ApiOrigin> {
  let url = reqwest::Url::parse(raw.trim()).ok()?;
  if !matches!(url.scheme(), "http" | "https") {
    return None;
  }
  Some(ApiOrigin {
    host: url.host_str()?.to_string(),
    port: url.port_or_known_default()?,
  })
}

/// The configured API origin, resolved once.
///
/// Rust cannot see the frontend's `$env/static/public` values, so we take the same
/// setting from the two places that ARE visible here, most specific first:
///   * `KOMIKA_API_ORIGIN` in the process env — the escape hatch for `tauri dev` and for
///     a self-hoster pointing an already-built app at their own server;
///   * `PUBLIC_KOMIKA_API` at compile time — set it when invoking cargo/tauri build and
///     the shipped binary trusts exactly the backend it was built against.
///
/// There is intentionally NO IPC setter: a command that let the webview name the exempt
/// host would hand any injected script a general-purpose SSRF bypass.
fn api_origin() -> Option<&'static ApiOrigin> {
  static ORIGIN: OnceLock<Option<ApiOrigin>> = OnceLock::new();
  ORIGIN
    .get_or_init(|| {
      std::env::var("KOMIKA_API_ORIGIN")
        .ok()
        .as_deref()
        .and_then(parse_api_origin)
        .or_else(|| option_env!("PUBLIC_KOMIKA_API").and_then(parse_api_origin))
    })
    .as_ref()
}

/// True when `host:port` is the app's own backend (the single SSRF-guard exemption).
fn is_own_api_origin(host: &str, port: u16) -> bool {
  api_origin().is_some_and(|o| o.matches(host, port))
}

/// Log which path a native image actually came from.
///
/// The product claim is "native fetches images itself, the Worker/hosted proxy is
/// web-only", and a silent regression to `hosted` (bad build-time flag, degraded engine,
/// source that only resolves server-side) is otherwise invisible — the app looks
/// identical. Info on the first sighting of each route so a log file always shows which
/// one a build settled on; debug per fetch, since a chapter is ~20 of these.
fn note_image_route(target: &reqwest::Url) {
  static SEEN_HOSTED: AtomicBool = AtomicBool::new(false);
  static SEEN_CDN: AtomicBool = AtomicBool::new(false);
  let host = target.host_str().unwrap_or("");
  let port = target.port_or_known_default().unwrap_or(0);
  let (route, seen) = if is_own_api_origin(host, port) {
    ("hosted", &SEEN_HOSTED)
  } else {
    ("cdn", &SEEN_CDN)
  };
  if seen.swap(true, Ordering::Relaxed) {
    log::debug!(target: "images", "native image fetch route={route} host={host}");
  } else {
    log::info!(target: "images", "native image fetch route={route} host={host} (first use)");
  }
}

/// Same-origin Referer for the target (mirrors the Worker's `scheme://host[:port]/`).
fn referer_for(url: &reqwest::Url) -> String {
  let mut r = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
  if let Some(port) = url.port() {
    r.push_str(&format!(":{port}"));
  }
  r.push('/');
  r
}

/// Fetch a source image's raw bytes directly from its CDN.
///
/// This is the native half of Komika's image pipeline: desktop and mobile builds
/// scrape/fetch images themselves and never touch the Cloudflare Worker proxy
/// (which exists for the CORS-bound web build). Returning `tauri::ipc::Response`
/// sends the bytes over IPC efficiently and resolves to an `ArrayBuffer` in JS.
///
/// The `url` comes from third-party source data, so the fetch is hardened against
/// client-side SSRF (I2): http(s) only, every hop's host must resolve to a public
/// address (no loopback/LAN/link-local/metadata targets) unless it is the app's own
/// configured API origin, redirects are followed manually with re-validation, and the
/// body is bounded by a per-request timeout, a total budget and a size cap.
#[tauri::command]
async fn fetch_image(url: String) -> Result<tauri::ipc::Response, String> {
  // Gate concurrency: one permit, held across the whole redirect chain, so the
  // in-flight 32 MiB buffers can't stack up past MAX_CONCURRENT_IMAGE_FETCHES.
  let _permit = image_fetch_semaphore()
    .acquire()
    .await
    .map_err(|_| "image fetch pool closed".to_string())?;
  let deadline = Instant::now() + FETCH_TOTAL_BUDGET;
  let mut next = url;
  for _ in 0..=MAX_REDIRECTS {
    let (target, addr) = validate_image_url(&next).await?;
    note_image_route(&target);
    // Pin the connection to the validated IP. Without this, reqwest resolves DNS
    // a SECOND time at connect, so a short-TTL/round-robin rebind could swap in a
    // blocked address (LAN / 169.254.169.254) after the check passed. `.resolve`
    // dials exactly the vetted IP while host/SNI/cert stay the original hostname.
    let host = target.host_str().unwrap_or("").to_string();
    let client = image_client_builder()
      .resolve(&host, addr)
      .build()
      .map_err(|e| e.to_string())?;
    let resp = send_image_request(&client, &target, deadline).await?;
    let status = resp.status();
    if status.is_redirection() {
      let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "redirect without a Location header".to_string())?;
      // Resolve relative redirects against the current URL; the next loop turn
      // re-runs the host guard on the resolved target.
      next = target
        .join(location)
        .map_err(|e| format!("invalid redirect target: {e}"))?
        .to_string();
      continue;
    }
    if !status.is_success() {
      return Err(format!("upstream returned {status}"));
    }
    return read_capped(resp).await.map(tauri::ipc::Response::new);
  }
  Err("too many redirects".to_string())
}

/// GET one hop, retrying a transport error or a transient upstream status up to
/// `IMAGE_FETCH_RETRIES` times. Every attempt is bounded by `deadline`, so retries can
/// never push a redirect chain past `FETCH_TOTAL_BUDGET`; a retry that wouldn't fit
/// surfaces the pending error instead of sleeping into the ceiling.
async fn send_image_request(
  client: &reqwest::Client,
  target: &reqwest::Url,
  deadline: Instant,
) -> Result<reqwest::Response, String> {
  let mut attempt = 0usize;
  loop {
    if Instant::now() >= deadline {
      return Err("image fetch exceeded its time budget".to_string());
    }
    let outcome = client
      .get(target.clone())
      .header(reqwest::header::REFERER, referer_for(target))
      .send()
      .await;
    // Anything final (success, 4xx, a redirect the caller must follow) returns as-is;
    // only the retryable cases fall through with a reason.
    let reason = match outcome {
      Ok(resp) if attempt >= IMAGE_FETCH_RETRIES || !is_transient_status(resp.status()) => {
        return Ok(resp)
      }
      Ok(resp) => format!("upstream returned {}", resp.status()),
      Err(e) if attempt >= IMAGE_FETCH_RETRIES => return Err(e.to_string()),
      Err(e) => e.to_string(),
    };
    let backoff = retry_backoff(attempt);
    if Instant::now() + backoff >= deadline {
      return Err(reason);
    }
    log::debug!(target: "images", "retrying image fetch ({reason})");
    tokio::time::sleep(backoff).await;
    attempt += 1;
  }
}

/// Statuses worth one more attempt: an overloaded/rate-limited CDN edge, not a 404.
fn is_transient_status(status: reqwest::StatusCode) -> bool {
  status.is_server_error()
    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    || status == reqwest::StatusCode::REQUEST_TIMEOUT
}

/// Linear backoff plus a little jitter, so a burst of page prefetches that all failed on
/// the same blip doesn't retry in lockstep. The clock's sub-second nanos are random
/// enough for that and save pulling in `rand`.
fn retry_backoff(attempt: usize) -> Duration {
  let jitter = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| u64::from(d.subsec_nanos() % 150))
    .unwrap_or(0);
  RETRY_BASE_BACKOFF * (attempt as u32 + 1) + Duration::from_millis(jitter)
}

/// Parse `raw`, require http(s), and reject any host that resolves to a
/// non-public address — the pre-fetch SSRF guard, with the single exemption for the
/// app's own API origin. Returns the parsed URL together with the vetted `SocketAddr`
/// the caller must pin the connection to (so reqwest cannot re-resolve to a different,
/// unchecked address).
async fn validate_image_url(raw: &str) -> Result<(reqwest::Url, std::net::SocketAddr), String> {
  let url = reqwest::Url::parse(raw).map_err(|e| format!("invalid url: {e}"))?;
  match url.scheme() {
    "http" | "https" => {}
    other => return Err(format!("unsupported scheme: {other}")),
  }
  let host = url.host_str().ok_or_else(|| "url has no host".to_string())?;
  let port = url.port_or_known_default().unwrap_or(80);
  // `lookup_host` handles both IP literals (no DNS) and names; require EVERY
  // resolved address to be public so a name pointing at the LAN is blocked too.
  let addrs: Vec<_> = tokio::net::lookup_host((host, port))
    .await
    .map_err(|e| format!("dns lookup failed: {e}"))?
    .collect();
  if addrs.is_empty() {
    return Err("host did not resolve".to_string());
  }
  // Narrow exemption for the app's OWN backend (`http://localhost:8080` in dev, a LAN
  // address for a self-hoster): the provider sends `/covers/…`, `/avatars/…` and the
  // hosted image proxy here, so without this every image in the app is a grey box. Scope
  // is one configured host:port — private ranges as a class stay blocked, and third-party
  // source URLs (the actual SSRF vector) are unaffected. Mirrors the deliberate loopback
  // exemption `suwayomi_image` already makes for the embedded engine.
  let blocked = if is_own_api_origin(host, port) {
    None
  } else {
    addrs.iter().find(|a| is_blocked_ip(&a.ip()))
  };
  if let Some(addr) = blocked {
    return Err(format!("host resolves to a non-public address: {}", addr.ip()));
  }
  // Every resolved address passed the guard; pin the connection to the first so
  // the dialed IP is exactly one we validated (defeats the DNS-rebinding TOCTOU).
  Ok((url, addrs[0]))
}

/// True for addresses a source image must never point at: loopback, private,
/// link-local (incl. the cloud metadata endpoint 169.254.169.254), CGNAT,
/// unspecified, broadcast, and their IPv6 equivalents / v4-mapped forms.
fn is_blocked_ip(ip: &IpAddr) -> bool {
  match ip {
    IpAddr::V4(v4) => {
      let o = v4.octets();
      v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        || o[0] == 0
        // Carrier-grade NAT 100.64.0.0/10.
        || (o[0] == 100 && (o[1] & 0xc0) == 0x40)
        // Class E / reserved 240.0.0.0/4 (255.255.255.255 is is_broadcast).
        || o[0] >= 240
        // Benchmarking 198.18.0.0/15.
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        // IETF protocol assignments 192.0.0.0/24.
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
    }
    IpAddr::V6(v6) => {
      if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_blocked_ip(&IpAddr::V4(mapped));
      }
      let seg = v6.segments();
      // Deprecated IPv4-compatible address ::a.b.c.d (::/96), excluding :: and
      // ::1 (handled below): fold to the embedded v4 so ::169.254.169.254 etc.
      // can't tunnel past the guard the way only ::ffff: forms did before.
      if seg[..6].iter().all(|&s| s == 0) && !(seg[6] == 0 && seg[7] <= 1) {
        let v4 = std::net::Ipv4Addr::new(
          (seg[6] >> 8) as u8,
          seg[6] as u8,
          (seg[7] >> 8) as u8,
          seg[7] as u8,
        );
        return is_blocked_ip(&IpAddr::V4(v4));
      }
      v6.is_loopback()
        || v6.is_unspecified()
        // Unique-local fc00::/7.
        || (seg[0] & 0xfe00) == 0xfc00
        // Link-local fe80::/10.
        || (seg[0] & 0xffc0) == 0xfe80
        // Documentation 2001:db8::/32.
        || (seg[0] == 0x2001 && seg[1] == 0x0db8)
    }
  }
}

/// Read the body, enforcing `MAX_IMAGE_BYTES` from Content-Length (fail fast) and
/// while streaming (guards against a missing/understated header).
async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>, String> {
  if let Some(len) = resp.content_length() {
    if len > MAX_IMAGE_BYTES as u64 {
      return Err(format!("image too large: {len} bytes"));
    }
  }
  let mut buf: Vec<u8> = Vec::new();
  while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
    if buf.len() + chunk.len() > MAX_IMAGE_BYTES {
      return Err("image exceeds size cap".to_string());
    }
    buf.extend_from_slice(&chunk);
  }
  Ok(buf)
}

/// Set once `suwayomi::start` has actually been called, so the exit hook knows whether a
/// supervision task can exist at all.
static ENGINE_STARTED: AtomicBool = AtomicBool::new(false);

/// Should the embedded engine boot during `setup`?
///
/// The product switch is the frontend's build-time `PUBLIC_KOMIKA_NATIVE_ENGINE` (default
/// off) and Rust cannot read `$env/static/public`, so we accept the same value from the
/// two places visible here — the process env (works on an already-built binary) and a
/// compile-time env var — and fall back to the historical "always start" so no existing
/// build silently loses its engine. With it off, a flag-off desktop build stops paying
/// for a `-Xmx512m` JVM, an H2 data dir, a lockfile and the loopback CF shim it will
/// never call; JS can still bring the engine up on demand via `suwayomi_start`.
fn engine_autostart_enabled() -> bool {
  fn parse(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
      "1" | "true" | "on" | "yes" => Some(true),
      "0" | "false" | "off" | "no" => Some(false),
      _ => None,
    }
  }
  std::env::var("KOMIKA_NATIVE_ENGINE")
    .ok()
    .as_deref()
    .and_then(parse)
    .or_else(|| option_env!("PUBLIC_KOMIKA_NATIVE_ENGINE").and_then(parse))
    .unwrap_or(true)
}

/// Start the engine at most once. `suwayomi::start` is not itself idempotent (a second
/// call would spawn a second supervision loop), so the flag is claimed with a CAS before
/// anything runs.
fn start_engine_once(app: &tauri::AppHandle) {
  if ENGINE_STARTED
    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
    .is_ok()
  {
    suwayomi::start(app.clone());
  }
}

/// Bring the embedded engine up on demand, for builds where autostart is off. Idempotent
/// and cheap to call — JS invokes it when the native-engine flag is on, before it starts
/// polling `suwayomi_status`.
#[tauri::command]
fn suwayomi_start(app: tauri::AppHandle) {
  start_engine_once(&app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  let app = tauri::Builder::default()
    .setup(|app| {
      // Logging is installed in RELEASE too, to a rotating file in the OS log dir. The
      // supervisor pumps every JVM stdout/stderr line into `log::*` targeted at
      // `suwayomi`; without a registered logger a production engine failure leaves only
      // "engine did not become ready within 30s" and the real stack trace is discarded.
      app.handle().plugin(
        tauri_plugin_log::Builder::default()
          .level(if cfg!(debug_assertions) {
            log::LevelFilter::Debug
          } else {
            log::LevelFilter::Info
          })
          .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(3))
          .max_file_size(2 * 1024 * 1024)
          .targets([
            tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::Stdout),
            tauri_plugin_log::Target::new(tauri_plugin_log::TargetKind::LogDir {
              file_name: Some("komika".into()),
            }),
          ])
          .build(),
      )?;
      // Embedded Suwayomi sidecar (N1.1). On desktop this starts the JVM supervisor; a
      // start failure only logs + sets a degraded state and never aborts app startup. The
      // engine binds loopback only and JS reaches it via the `suwayomi_gql` IPC proxy. On
      // mobile (`suwayomi_mobile.rs`) both `new()` and `start()` are inert no-ops: the
      // engine reports "unavailable" and JS routes content to the hosted backend.
      use tauri::Manager;
      app.manage(Arc::new(suwayomi::SuwayomiSupervisor::new()));
      if engine_autostart_enabled() {
        start_engine_once(app.handle());
      } else {
        log::info!(target: "suwayomi", "engine autostart disabled; waiting for suwayomi_start");
      }
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      fetch_image,
      suwayomi_start,
      suwayomi::suwayomi_gql,
      suwayomi::suwayomi_image,
      suwayomi::suwayomi_status,
      suwayomi::suwayomi_base_url
    ])
    .build(tauri::generate_context!())
    .expect("error while building tauri application");

  // Graceful shutdown: on exit, terminate the JVM child (Drop's kill_on_drop is a
  // backstop). block_on is safe here — the run callback is on the main thread, not a
  // tokio worker. Skipped entirely when the engine was never started: `stop_and_wait`
  // polls for a `Stopped` transition only a supervision task can make, so with no task it
  // would burn its full 6 s timeout on the UI thread at every quit.
  app.run(|handle, event| {
    if let tauri::RunEvent::Exit = event {
      if !ENGINE_STARTED.load(Ordering::SeqCst) {
        return;
      }
      use tauri::Manager;
      if let Some(sup) = handle.try_state::<Arc<suwayomi::SuwayomiSupervisor>>() {
        let sup = sup.inner().clone();
        tauri::async_runtime::block_on(async move { sup.stop_and_wait().await });
      }
    }
  });
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn blocks_non_public_ipv4() {
    for ip in [
      "127.0.0.1",
      "10.0.0.5",
      "172.16.0.1",
      "192.168.1.1",
      "169.254.169.254", // cloud metadata
      "100.64.0.1",      // CGNAT
      "0.0.0.0",
      "255.255.255.255",
      "240.0.0.1",       // class E / reserved 240.0.0.0/4
      "198.18.0.1",      // benchmarking 198.18.0.0/15
      "198.19.255.255",  // benchmarking (upper half)
      "192.0.0.1",       // IETF protocol assignments 192.0.0.0/24
    ] {
      assert!(is_blocked_ip(&ip.parse().unwrap()), "{ip} must be blocked");
    }
  }

  #[test]
  fn allows_public_ipv4() {
    for ip in ["1.1.1.1", "8.8.8.8", "104.16.0.1"] {
      assert!(!is_blocked_ip(&ip.parse().unwrap()), "{ip} must be allowed");
    }
  }

  #[test]
  fn blocks_non_public_ipv6() {
    for ip in [
      "::1",
      "::",
      "fe80::1",
      "fc00::1",
      "::ffff:127.0.0.1",
      "2001:db8::1",     // documentation 2001:db8::/32
      "::192.168.1.1",   // deprecated IPv4-compatible folding to a private v4
      "::169.254.169.254", // IPv4-compatible folding to link-local metadata
    ] {
      assert!(is_blocked_ip(&ip.parse().unwrap()), "{ip} must be blocked");
    }
  }

  #[test]
  fn allows_public_ipv6() {
    assert!(!is_blocked_ip(&"2606:4700:4700::1111".parse().unwrap()));
  }

  #[test]
  fn referer_is_origin_with_trailing_slash() {
    let u = reqwest::Url::parse("https://uploads.mangadex.org/covers/x/y.jpg").unwrap();
    assert_eq!(referer_for(&u), "https://uploads.mangadex.org/");
    let p = reqwest::Url::parse("http://example.com:8080/a/b").unwrap();
    assert_eq!(referer_for(&p), "http://example.com:8080/");
  }

  #[tokio::test]
  async fn rejects_non_http_schemes() {
    assert!(validate_image_url("file:///etc/passwd").await.is_err());
    assert!(validate_image_url("ftp://example.com/x.png").await.is_err());
    assert!(validate_image_url("not a url").await.is_err());
  }

  #[test]
  fn parses_api_origin_from_a_graphql_url() {
    let o = parse_api_origin("http://localhost:8080/graphql").unwrap();
    assert!(o.matches("localhost", 8080));
    assert!(o.matches("LOCALHOST", 8080));
    // Only that exact host:port — a different port on the same host is still guarded.
    assert!(!o.matches("localhost", 22));
    assert!(!o.matches("127.0.0.1", 8080));
    // Default ports are materialized so `https://api.komiq.cc` matches port 443.
    assert!(parse_api_origin("https://api.komiq.cc/graphql")
      .unwrap()
      .matches("api.komiq.cc", 443));
    assert!(parse_api_origin("ws://localhost:8080").is_none());
    assert!(parse_api_origin("not a url").is_none());
  }

  #[test]
  fn retries_only_transient_statuses() {
    use reqwest::StatusCode;
    for s in [
      StatusCode::INTERNAL_SERVER_ERROR,
      StatusCode::BAD_GATEWAY,
      StatusCode::SERVICE_UNAVAILABLE,
      StatusCode::TOO_MANY_REQUESTS,
      StatusCode::REQUEST_TIMEOUT,
    ] {
      assert!(is_transient_status(s), "{s} should be retried");
    }
    for s in [StatusCode::OK, StatusCode::NOT_FOUND, StatusCode::FORBIDDEN] {
      assert!(!is_transient_status(s), "{s} should not be retried");
    }
  }

  #[test]
  fn backoff_grows_and_stays_bounded() {
    let first = retry_backoff(0);
    assert!(first >= RETRY_BASE_BACKOFF);
    assert!(first < RETRY_BASE_BACKOFF + Duration::from_millis(150));
    assert!(retry_backoff(1) >= RETRY_BASE_BACKOFF * 2);
  }

  #[tokio::test]
  async fn rejects_loopback_and_lan_literals() {
    assert!(validate_image_url("http://127.0.0.1/x.png").await.is_err());
    assert!(validate_image_url("http://[::1]/x.png").await.is_err());
    assert!(validate_image_url("http://192.168.0.1/x.png").await.is_err());
    assert!(validate_image_url("http://169.254.169.254/latest/meta-data")
      .await
      .is_err());
  }
}
