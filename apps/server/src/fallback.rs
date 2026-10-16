//! Independent, opt-in TLS reader/API listener and its process-wide media reserve.

mod tls;

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, RwLock, RwLockReadGuard};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Request, State};
use axum::http::header::{CACHE_CONTROL, HOST, RETRY_AFTER};
use axum::http::{Method, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::cloudflare::origin_tls;
use crate::config::{parse_fallback_authority, Config, FallbackConfig};

#[derive(Clone, Copy)]
pub(crate) struct FallbackListener;

#[derive(Clone)]
struct Authorities {
    hosts: Vec<(String, Option<u16>)>,
    port: u16,
}

impl Authorities {
    fn new(hosts: &[String], port: u16) -> Self {
        Self {
            hosts: hosts
                .iter()
                .map(|v| parse_fallback_authority(v).expect("validated fallback host"))
                .collect(),
            port,
        }
    }

    fn allows(&self, authority: &(String, Option<u16>)) -> bool {
        self.hosts.iter().any(|(host, port)| {
            host == &authority.0
                && match port {
                    Some(port) => {
                        authority.1 == Some(*port) || (*port == 443 && authority.1.is_none())
                    }
                    None => {
                        authority.1.is_none()
                            || authority.1 == Some(443)
                            || authority.1 == Some(self.port)
                    }
                }
        })
    }

    fn matches(&self, request: &Request) -> bool {
        // Reject duplicate Host headers as well as missing/malformed ones.
        let mut headers = request.headers().get_all(HOST).iter();
        let host = headers
            .next()
            .and_then(|v| v.to_str().ok())
            .and_then(parse_fallback_authority);
        let Some(host) = host.filter(|_| headers.next().is_none()) else {
            return false;
        };
        if !self.allows(&host) {
            return false;
        }
        request.uri().authority().is_none_or(|uri| {
            parse_fallback_authority(uri.as_str()).is_some_and(|other| {
                // Absolute-form must name the same authority, not merely another
                // listed host. Omitted HTTPS ports and explicit :443 are equivalent.
                other.0 == host.0
                    && other.1.unwrap_or(443) == host.1.unwrap_or(443)
                    && self.allows(&other)
            })
        })
    }
}

fn refusal(status: StatusCode, message: &'static str) -> Response {
    (status, [(CACHE_CONTROL, "no-store")], message).into_response()
}

async fn authority_guard(
    State(hosts): State<Authorities>,
    request: Request,
    next: Next,
) -> Response {
    if !hosts.matches(&request) {
        return refusal(StatusCode::MISDIRECTED_REQUEST, "Misdirected request\n");
    }
    next.run(request).await
}

/// Must list every media route of `public_router`, cross-checked against all its
/// telemetry::MediaLayer tags: covers, avatars, comment media, extension icons,
/// engine pages/thumbnails, all /media/ objects/pages/manifests/keys, and /img.
fn is_media(path: &str) -> bool {
    path == "/img"
        || [
            "/covers/",
            "/avatars/",
            "/comment-media/",
            "/ext-icons/",
            "/api/v1/manga/",
            "/media/",
        ]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

struct DayUsage {
    day: u64,
    bytes: AtomicU64,
    warned: AtomicBool,
}

impl DayUsage {
    fn new(day: u64) -> Self {
        Self {
            day,
            bytes: AtomicU64::new(0),
            warned: AtomicBool::new(false),
        }
    }
}

struct Reserve {
    limit: u64,
    clock: Clock,
    // Concurrent readers update atomics. A brief exclusive lock at midnight
    // makes resetting day/bytes/warned atomic with respect to streamed frames.
    usage: RwLock<DayUsage>,
}

impl Reserve {
    fn new(limit: u64, clock: Clock) -> Self {
        let day = clock() / 86_400;
        Self {
            limit,
            clock,
            usage: RwLock::new(DayUsage::new(day)),
        }
    }

    fn usage(&self) -> (u64, RwLockReadGuard<'_, DayUsage>) {
        loop {
            let usage = self.usage.read().unwrap_or_else(|e| e.into_inner());
            let now = (self.clock)();
            let day = now / 86_400;
            if usage.day == day {
                return (now, usage);
            }
            drop(usage);
            let mut usage = self.usage.write().unwrap_or_else(|e| e.into_inner());
            // Re-read after taking the lock: a frame paused across midnight
            // must not reset a newer day's usage back to its stale timestamp.
            let day = (self.clock)() / 86_400;
            if usage.day != day {
                *usage = DayUsage::new(day);
            }
        }
    }

    fn warn_exhausted(&self, usage: &DayUsage) {
        if usage.bytes.load(Relaxed) >= self.limit && !usage.warned.swap(true, Relaxed) {
            tracing::warn!(
                utc_day = usage.day,
                limit = self.limit,
                "fallback media reserve exhausted"
            );
        }
    }

    fn retry_after(&self) -> Option<u64> {
        let (now, usage) = self.usage();
        self.warn_exhausted(&usage);
        (usage.bytes.load(Relaxed) >= self.limit).then_some(86_400 - now % 86_400)
    }

    fn add(&self, bytes: usize) {
        let (_, usage) = self.usage();
        let _ = usage.bytes.fetch_update(Relaxed, Relaxed, |old| {
            Some(old.saturating_add(bytes as u64))
        });
        self.warn_exhausted(&usage);
    }
}

struct CountedBody {
    inner: Body,
    reserve: Arc<Reserve>,
}

impl HttpBody for CountedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        let frame = std::task::ready!(Pin::new(&mut self.inner).poll_frame(cx));
        if let Some(Ok(frame)) = &frame {
            if let Some(data) = frame.data_ref() {
                self.reserve.add(data.len());
            }
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

async fn media_reserve(
    State(reserve): State<Arc<Reserve>>,
    request: Request,
    next: Next,
) -> Response {
    if !is_media(request.uri().path()) {
        return next.run(request).await;
    }
    if let Some(retry) = reserve.retry_after() {
        let mut response = refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "Fallback media reserve exhausted\n",
        );
        response.headers_mut().insert(RETRY_AFTER, retry.into());
        return response;
    }
    let head = request.method() == Method::HEAD;
    let response = next.run(request).await;
    // Already streaming responses finish; overshoot by in-flight responses is
    // allowed. Meter encoded data frames outside all public_router layers,
    // regardless of Host or TLS SNI. HEAD never sends any body bytes.
    response.map(|inner| {
        if head {
            Body::empty()
        } else {
            Body::new(CountedBody { inner, reserve })
        }
    })
}

fn protect(app: Router, hosts: &[String], port: u16, reserve: Arc<Reserve>) -> Router {
    app.layer(from_fn_with_state(reserve, media_reserve))
        .layer(from_fn_with_state(
            Authorities::new(hosts, port),
            authority_guard,
        ))
        .layer(axum::Extension(FallbackListener))
}

pub(crate) fn router(
    cfg: &Config,
    fb: &FallbackConfig,
    mut state: crate::RouterState,
    cloudflare: Arc<crate::cloudflare::Manager>,
    listener_port: u16,
) -> Router {
    // This listener is reached directly, even when the main listener trusts proxies.
    state.trusted_proxies = Arc::new(crate::TrustedProxies::new(Vec::new(), true));
    let app = crate::public_router(
        cfg,
        state,
        cloudflare,
        crate::web_dir::Apps::ReaderOnly(fb.reader_dir.clone()),
        true,
    );
    let reserve = Arc::new(Reserve::new(
        fb.media_bytes_per_day,
        Arc::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        }),
    ));
    protect(app, &fb.hosts, listener_port, reserve)
}

fn validate_reader(fb: &FallbackConfig) -> anyhow::Result<()> {
    use anyhow::Context;
    anyhow::ensure!(
        fb.reader_dir.join("index.html").is_file(),
        "KOMIKA_FALLBACK_READER_DIR must contain index.html: {}",
        fb.reader_dir.display()
    );
    std::fs::read(fb.reader_dir.join("index.html")).with_context(|| {
        format!(
            "reading KOMIKA_FALLBACK_READER_DIR index.html: {}",
            fb.reader_dir.display()
        )
    })?;
    Ok(())
}

pub(crate) async fn start(
    cfg: &Config,
    fb: &FallbackConfig,
    state: crate::RouterState,
    cloudflare: Arc<crate::cloudflare::Manager>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    use anyhow::Context;
    validate_reader(fb)?;
    let (config, resolver) = tls::ReloadingCert::load(&fb.cert_file, &fb.key_file)?;
    let tcp = tokio::net::TcpListener::bind(SocketAddr::new(fb.bind_addr, fb.port))
        .await
        .context("binding KOMIKA_FALLBACK_BIND_ADDR:KOMIKA_FALLBACK_PORT")?;
    let addr = tcp.local_addr()?;
    let (listener, _accept) = origin_tls::TlsListener::new(tcp, config)?;
    let app = router(cfg, fb, state, cloudflare, addr.port());
    resolver.spawn_reload();
    let server = tokio::spawn(origin_tls::serve(listener, app, Some(shutdown)));
    tracing::info!(hosts = ?fb.hosts, "fallback listening on https://{addr}");
    Ok(server)
}

#[cfg(test)]
mod tests;
