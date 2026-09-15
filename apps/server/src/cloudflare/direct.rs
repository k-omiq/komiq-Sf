//! HTTPS for the "public without a domain" mode.
//!
//! Readers reach the server at its own public address, with nothing in front. Let's
//! Encrypt signs certificates for bare IP addresses, so the server asks for one
//! (`acme.rs`), serves HTTPS with it, and renews it by itself: such a certificate lasts
//! about six days, and a new one is requested when half of that is gone.
//!
//! Two listeners run while this is on:
//!
//! - plain HTTP on port 80 (`KOMIKA_ACME_PORT`), which answers the certificate
//!   authority's check and sends every other request to HTTPS;
//! - HTTPS on port 443 (`KOMIKA_HTTPS_PORT`), which serves the public router.
//!
//! The server's usual plain HTTP port stays open, for the home network and for the
//! terminal setup. It would also still take a password in the clear from the
//! internet, so [`plain_http_guard`] closes it to clients on the internet as soon as
//! one of them has used the HTTPS address. That proves port 443 is open on the server's
//! side, so a firewall in front of the server locks nobody out. (An owner whose own
//! network blocks port 443 still has the terminal setup on the server itself.)
//!
//! Three 0600 files next to the database: the account key, the certificate's key and
//! the certificate chain. The name the certificate is for is `instance::DIRECT_TLS_HOST`;
//! it is empty whenever this is off.

use axum::extract::{ConnectInfo, Path as UrlPath, Request, State};
use axum::http::{header, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use super::*;

const ACCOUNT_KEY_FILE: &str = "acme-account.der";
pub(super) const DIRECT_KEY_FILE: &str = "direct-key.der";
pub(super) const DIRECT_CERT_FILE: &str = "direct-cert.pem";

/// One certificate request, start to end. Shorter than the two minutes `komika-server
/// setup` waits for an answer.
const ISSUE_TIMEOUT: Duration = Duration::from_secs(90);

type Dates = (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>);

/// What the owner typed, as the name the certificate is for: `203.0.113.5:8080` and
/// `https://203.0.113.5/` both become `203.0.113.5`. A port is dropped: HTTPS has its
/// own. `any_address` skips the "public addresses only" rule, for a private
/// certificate authority.
pub fn direct_host(raw: &str, any_address: bool) -> Result<String, String> {
    const HELP: &str = "Enter this server's public IP address, like 203.0.113.5.";
    let raw = raw.trim();
    let with_scheme = if let Ok(ip) = raw.parse::<IpAddr>() {
        match ip {
            IpAddr::V4(_) => format!("http://{ip}"),
            IpAddr::V6(_) => format!("http://[{ip}]"),
        }
    } else if raw.contains("://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let url = reqwest::Url::parse(&with_scheme).map_err(|_| HELP.to_string())?;
    let host = url
        .host_str()
        .filter(|_| matches!(url.scheme(), "http" | "https"))
        .ok_or(HELP)?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(ip) => {
            if !any_address && check_public_ip(bare).is_err() {
                return Err(format!(
                    "{ip} is not a public address. Let's Encrypt gives certificates for public \
                     addresses only."
                ));
            }
            Ok(ip.to_string())
        }
        Err(_) => normalize_domain(bare).map_err(|_| HELP.to_string()),
    }
}

/// `https://203.0.113.5`, `https://[2001:db8::1]:8443`.
fn https_origin(host: &str, port: u16) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    if port == 443 {
        format!("https://{host}")
    } else {
        format!("https://{host}:{port}")
    }
}

// ---- the port-80 listener ----------------------------------------------------------

/// The HTTPS address while direct HTTPS is on (`Manager::direct_origin`).
type DirectOrigin = Arc<RwLock<Option<String>>>;

fn challenge_router(challenges: acme::Challenges, origin: DirectOrigin) -> axum::Router {
    axum::Router::new()
        .route(
            "/.well-known/acme-challenge/{token}",
            axum::routing::get(answer_challenge),
        )
        .fallback(to_https)
        .with_state((challenges, origin))
}

async fn answer_challenge(
    State((challenges, _)): State<(acme::Challenges, DirectOrigin)>,
    UrlPath(token): UrlPath<String>,
) -> Response {
    let answer = challenges
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&token)
        .cloned();
    match answer {
        Some(a) => a.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Everything else on port 80 moves to the server's HTTPS address: the stored one,
/// never one a request names. Nothing to send it to while the first certificate is
/// still being requested.
async fn to_https(
    State((_, origin)): State<(acme::Challenges, DirectOrigin)>,
    uri: Uri,
) -> Response {
    let origin = origin.read().unwrap_or_else(|p| p.into_inner()).clone();
    let Some(origin) = origin else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    // Readable across origins, so the reader app can see where the server moved and
    // connect there instead (`checkServer`). It carries nothing but the address.
    (
        [(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")],
        Redirect::permanent(&format!("{origin}{path}")),
    )
        .into_response()
}

// ---- the plain HTTP port ------------------------------------------------------------

/// A client that is not on this computer, the home network or a private network
/// (VPN ranges included).
pub fn from_internet(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    let shared = match ip {
        // 100.64.0.0/10: carrier-grade NAT, which mesh VPNs use.
        IpAddr::V4(v4) => v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64,
        // fe80::/10: link-local.
        IpAddr::V6(v6) => (v6.segments()[0] & 0xFFC0) == 0xFE80,
    };
    !shared && check_public_ip(&ip.to_string()).is_ok()
}

/// What the address a client typed (the `Host` it sent) says about where the client
/// is. The socket peer is the proof when the server sees it. Behind Docker Desktop or
/// rootless Docker it does not: every connection arrives from Docker's own private
/// address. There the typed address decides, because nobody reaches a public address
/// without crossing the internet.
#[derive(Debug, PartialEq, Eq)]
pub enum TypedHost {
    /// This computer, a private address, or a name only a home network has.
    Local,
    /// A public IP address.
    Public,
    /// A DNS name: public when it leads to a public address ([`name_on_internet`]).
    Name(String),
}

/// The host of a `Host` header or a URL authority: no port, no brackets, lower case.
/// Empty when it cannot be read.
fn host_of(authority: &str) -> String {
    authority
        .trim()
        .parse::<axum::http::uri::Authority>()
        .map(|a| {
            a.host()
                .trim_matches(['[', ']'])
                .trim_end_matches('.')
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}

pub fn typed_host(authority: &str) -> TypedHost {
    // Names no public DNS zone holds: home routers and mDNS hand them out.
    const HOME: [&str; 6] = [
        "localhost",
        "local",
        "lan",
        "home",
        "internal",
        "localdomain",
    ];
    let host = host_of(authority);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if from_internet(ip) {
            TypedHost::Public
        } else {
            TypedHost::Local
        };
    }
    let last = host.rsplit('.').next().unwrap_or_default();
    if !host.contains('.') || HOME.contains(&last) || host.ends_with(".home.arpa") {
        return TypedHost::Local;
    }
    TypedHost::Name(host)
}

/// Whether a typed DNS name leads to the internet: it resolves to a public address. A
/// name this server cannot resolve counts as public: the client reached the server by
/// it, and a wrong "yes" costs the owner one command in a terminal.
pub async fn name_on_internet(name: &str) -> bool {
    let lookup = tokio::net::lookup_host((name, 0));
    match tokio::time::timeout(Duration::from_secs(2), lookup).await {
        Ok(Ok(mut found)) => found.any(|a| from_internet(a.ip())),
        _ => true,
    }
}

/// HTTPS ended in front of this server: a proxy says so (`X-Forwarded-Proto`), or a
/// browser does, whose page came over HTTPS from the address it now calls (`Origin`;
/// a browser does not let an HTTPS page call plain HTTP). A client can send either
/// header itself. That is its own loss: the check keeps an honest owner's password
/// off the open internet, and the setup code is what keeps strangers out.
pub fn https_in_front(headers: &axum::http::HeaderMap) -> bool {
    let text = |name| {
        headers
            .get(name)
            .and_then(|v: &axum::http::HeaderValue| v.to_str().ok())
    };
    let forwarded = text("x-forwarded-proto")
        .and_then(|v| v.split(',').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"));
    let same_origin = || {
        let page = text("origin")?.strip_prefix("https://")?;
        let host = host_of(text("host")?);
        Some(!host.is_empty() && host_of(page) == host)
    };
    forwarded || same_origin().unwrap_or(false)
}

/// The client typed a public address: a public IP address, or the name this server
/// is public at (`origin`).
fn typed_public(request: &Request, origin: &str) -> bool {
    // HTTP/2 carries the typed address in the request target, not in `Host`.
    let typed = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| request.uri().authority().map(|a| a.as_str()));
    match typed.map(typed_host) {
        Some(TypedHost::Public) => true,
        Some(TypedHost::Name(name)) => {
            name == host_of(origin.rsplit("://").next().unwrap_or(origin))
        }
        _ => false,
    }
}

/// Middleware on the public router. While direct mode serves HTTPS, and once a client
/// on the internet has used it, a request from the internet over plain HTTP is not
/// handled: a page load is sent to the HTTPS address, anything else is refused with
/// that address. Home-network clients and this computer are let through. With direct
/// HTTPS off this does nothing.
///
/// "From the internet" is a public peer, or a public address the client typed
/// ([`TypedHost`]): behind Docker Desktop or rootless Docker every peer is private.
pub async fn plain_http_guard(
    State(manager): State<Arc<Manager>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(peer) = request.extensions().get::<ConnectInfo<SocketAddr>>() {
        manager.record_lan_peer(peer.0.ip());
    }
    let origin = manager
        .direct_origin
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let Some(origin) = origin else {
        return next.run(request).await;
    };
    let internet = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| from_internet(peer.0.ip()))
        || typed_public(&request, &origin);
    if request.extensions().get::<origin_tls::OverTls>().is_some() {
        if internet && !manager.https_seen.swap(true, Ordering::Relaxed) {
            let manager = manager.clone();
            tokio::spawn(async move {
                let stored =
                    instance::store(&manager.pool, &instance::DIRECT_HTTPS_SEEN, "1").await;
                // Not written: a restart would open the plain port again. Let the next
                // visit over HTTPS try once more.
                if stored.is_err() {
                    manager.https_seen.store(false, Ordering::Relaxed);
                }
            });
        }
        return next.run(request).await;
    }
    if !internet || !manager.https_seen.load(Ordering::Relaxed) {
        return next.run(request).await;
    }
    let message = format!("This server uses HTTPS. Use {origin} instead of this address.");
    let page_load = matches!(*request.method(), Method::GET | Method::HEAD)
        && !request.headers().contains_key(header::AUTHORIZATION);
    if page_load {
        let path = request
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        return Redirect::permanent(&format!("{origin}{path}")).into_response();
    }
    // A GraphQL client shows `errors[].message`; it would hide the body of a 403.
    if request.uri().path() == "/graphql" {
        return axum::Json(json!({ "data": null, "errors": [{ "message": message }] }))
            .into_response();
    }
    (StatusCode::FORBIDDEN, message).into_response()
}

// ---- manager -----------------------------------------------------------------------

impl Manager {
    /// Record the access choice. `address` is the domain for `internet` (optional) and
    /// the server's public address for `direct` (required). `encrypt` (direct only)
    /// gets a certificate for that address and serves HTTPS with it.
    pub async fn set_access(
        self: &Arc<Self>,
        mode: &str,
        address: Option<&str>,
        encrypt: bool,
    ) -> Result<(), String> {
        // One change at a time, and never in the middle of a certificate request: the
        // last choice made is the one that holds.
        let _one = self.issuing.lock().await;
        if mode == "direct" && encrypt {
            return self.enable_direct_tls(address.unwrap_or_default()).await;
        }
        set_access(&self.pool, mode, address).await?;
        self.disable_direct_tls().await;
        if matches!(mode, "local" | "direct") {
            self.pause_cloudflare().await;
        } else {
            self.resume_cloudflare().await?;
        }
        Ok(())
    }

    /// The name the server holds a direct-mode certificate for, when that is on.
    pub(super) async fn direct_tls_host(&self) -> Option<String> {
        Some(
            instance::resolve(&self.pool, &instance::DIRECT_TLS_HOST)
                .await
                .value,
        )
        .filter(|h| !h.is_empty())
    }

    /// When the direct-mode certificate on disk starts and stops being valid.
    pub(super) fn certificate_dates(&self) -> Option<Dates> {
        let pem = std::fs::read_to_string(self.opts.data_dir.join(DIRECT_CERT_FILE)).ok()?;
        origin_tls::validity(&pem)
    }

    /// The certificate on disk fits the key on disk, and less than half of its life is
    /// gone. A pair that does not fit (a crash between the two writes) counts as due.
    fn certificate_fresh(&self) -> bool {
        let dir = &self.opts.data_dir;
        let fits = std::fs::read_to_string(dir.join(DIRECT_CERT_FILE))
            .ok()
            .zip(std::fs::read(dir.join(DIRECT_KEY_FILE)).ok())
            .is_some_and(|(cert, key)| origin_tls::server_config(&cert, &key).is_ok());
        fits && self
            .certificate_dates()
            .is_some_and(|(from, until)| chrono::Utc::now() < from + (until - from) / 2)
    }

    /// What `plain_http_guard` works from: the HTTPS address while direct HTTPS is on,
    /// and whether the internet has used it.
    fn set_plain_guard(&self, origin: Option<String>, https_seen: bool) {
        *self
            .direct_origin
            .write()
            .unwrap_or_else(|p| p.into_inner()) = origin;
        self.https_seen.store(https_seen, Ordering::Relaxed);
    }

    pub(super) async fn https_seen_stored(&self) -> bool {
        instance::resolve(&self.pool, &instance::DIRECT_HTTPS_SEEN)
            .await
            .value
            == "1"
    }

    fn set_certificate_error(&self, error: Option<String>) {
        *self
            .certificate_error
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = error;
    }

    /// The caller holds `issuing`.
    async fn enable_direct_tls(self: &Arc<Self>, raw: &str) -> Result<(), String> {
        let host = direct_host(raw, self.opts.acme_directory != acme::LETS_ENCRYPT)?;
        let previous = self.direct_tls_host().await;
        self.start_challenge_listener().await?;
        // The same address again, with a certificate that is still good: keep it.
        let keep = previous.as_deref() == Some(host.as_str()) && self.certificate_fresh();
        let served = async {
            if !keep {
                self.issue(&host).await?;
            }
            self.pause_cloudflare().await;
            self.start_https(DIRECT_KEY_FILE, DIRECT_CERT_FILE).await
        }
        .await;
        if let Err(e) = served {
            // A failed change leaves an earlier certificate serving as it was.
            if previous.is_none() {
                self.stop_direct_tasks().await;
                let mode = instance::resolve(&self.pool, &instance::ACCESS_MODE)
                    .await
                    .value;
                if !matches!(mode.as_str(), "local" | "direct") {
                    let _ = self.resume_cloudflare().await;
                }
            }
            return Err(e);
        }
        let address = https_origin(&host, self.opts.https_addr.port());
        // A new address has to prove itself before the plain port closes again.
        let same = previous.as_deref() == Some(host.as_str());
        let seen = same && self.https_seen_stored().await;
        for (setting, value) in [
            (&instance::DIRECT_HTTPS_SEEN, if seen { "1" } else { "" }),
            (&instance::DIRECT_TLS_HOST, host.as_str()),
            (&instance::PUBLIC_ADDRESS, address.as_str()),
            (&instance::ACCESS_MODE, "direct"),
        ] {
            instance::store(&self.pool, setting, value)
                .await
                .map_err(|_| "Could not save the address.".to_string())?;
        }
        self.set_plain_guard(Some(address), seen);
        self.set_certificate_error(None);
        self.start_renewals();
        Ok(())
    }

    /// Leave direct mode with HTTPS: stop its listeners and its renewals. The files
    /// stay, for a later return to the same address. The caller holds `issuing`.
    pub(super) async fn disable_direct_tls(&self) {
        if self.direct_tls_host().await.is_none() {
            return;
        }
        self.set_plain_guard(None, false);
        let _ = instance::store(&self.pool, &instance::DIRECT_TLS_HOST, "").await;
        let _ = instance::store(&self.pool, &instance::DIRECT_HTTPS_SEEN, "").await;
        self.stop_direct_tasks().await;
        self.stop_https().await;
        self.set_certificate_error(None);
    }

    /// Boot: serve the stored certificate again and keep renewing it.
    pub(super) async fn resume_direct_tls(self: &Arc<Self>) {
        let Some(host) = self.direct_tls_host().await else {
            return;
        };
        if let Err(e) = self.start_challenge_listener().await {
            tracing::warn!("direct HTTPS: {e}");
        }
        self.set_plain_guard(
            Some(https_origin(&host, self.opts.https_addr.port())),
            self.https_seen_stored().await,
        );
        match self.start_https(DIRECT_KEY_FILE, DIRECT_CERT_FILE).await {
            Ok(()) => tracing::info!(%host, addr = %self.opts.https_addr, "direct HTTPS listening"),
            Err(e) => tracing::warn!("direct HTTPS not started: {e}"),
        }
        self.start_renewals();
    }

    /// Plain HTTP on port 80: the certificate authority's check, and a redirect to
    /// HTTPS for everything else. Does nothing when it is already up.
    async fn start_challenge_listener(&self) -> Result<(), String> {
        let up =
            |t: &Option<tokio::task::JoinHandle<()>>| t.as_ref().is_some_and(|t| !t.is_finished());
        if up(&self
            .challenge_task
            .lock()
            .unwrap_or_else(|p| p.into_inner()))
        {
            return Ok(());
        }
        let addr = self.opts.acme_addr;
        let tcp = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
            format!(
                "Could not listen on port {} ({e}). The certificate authority checks this \
                 server on that port. With Docker, publish it. Stop any other web server that \
                 uses it.",
                addr.port()
            )
        })?;
        let router = challenge_router(self.challenges.clone(), self.direct_origin.clone());
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(tcp, router).await {
                tracing::warn!(error = %e, "port-80 listener stopped");
            }
        });
        *self
            .challenge_task
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(task);
        Ok(())
    }

    fn start_renewals(self: &Arc<Self>) {
        let mut slot = self.renew_task.lock().unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let me = self.clone();
        *slot = Some(tokio::spawn(async move {
            loop {
                me.renew_if_due().await;
                tokio::time::sleep(me.opts.renew_check).await;
            }
        }));
    }

    /// Stop the port-80 listener and the renewal loop, and wait until the port is free.
    pub(super) async fn stop_direct_tasks(&self) {
        for slot in [&self.challenge_task, &self.renew_task] {
            let task = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
            if let Some(t) = task {
                t.abort();
                let _ = t.await;
            }
        }
        // A request that was cut short leaves its answers behind.
        self.challenges
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// Get a new certificate when the current one is half gone (or missing), and bring
    /// the HTTPS listener back when it is down. A failure is kept for Settings › Access
    /// and tried again at the next look.
    async fn renew_if_due(&self) {
        let Some(host) = self.direct_tls_host().await else {
            return;
        };
        let _one = self.issuing.lock().await;
        // Port 80 was taken at boot, perhaps: without it no renewal can pass.
        if let Err(e) = self.start_challenge_listener().await {
            tracing::warn!("direct HTTPS: {e}");
        }
        let fresh = self.certificate_fresh();
        if fresh && self.https_up() {
            return;
        }
        let served = async {
            if !fresh {
                self.issue(&host).await?;
            }
            self.start_https(DIRECT_KEY_FILE, DIRECT_CERT_FILE).await
        }
        .await;
        match served {
            Ok(()) => {
                tracing::info!(%host, renewed = !fresh, "direct HTTPS certificate in place");
                self.set_certificate_error(None);
            }
            Err(e) => {
                tracing::warn!(%host, "direct HTTPS certificate not renewed: {e}");
                self.set_certificate_error(Some(e));
            }
        }
    }

    /// Ask the certificate authority for a certificate for `host` and write it, with a
    /// new key, to the data directory. The files change only when the request succeeds.
    async fn issue(&self, host: &str) -> Result<(), String> {
        let dir = &self.opts.data_dir;
        let save = |name: &str, bytes: &[u8]| {
            origin_tls::write_private(&dir.join(name), bytes)
                .map_err(|e| format!("Could not save {name}: {e}"))
        };
        let account = match std::fs::read(dir.join(ACCOUNT_KEY_FILE)) {
            Ok(key) => key,
            Err(_) => {
                let key = origin_tls::generate_key()?;
                save(ACCOUNT_KEY_FILE, &key)?;
                key
            }
        };
        let key = origin_tls::generate_key()?;
        let csr = origin_tls::acme_csr_der(&key, host)?;
        let mut http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            // A signed request names its URL; it is never sent anywhere else.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("komiq-server");
        if let Some(path) = &self.opts.acme_ca_file {
            let root = std::fs::read(path)
                .ok()
                .and_then(|pem| reqwest::Certificate::from_pem(&pem).ok())
                .ok_or("KOMIKA_ACME_CA_FILE is not a readable PEM certificate.")?;
            http = http.add_root_certificate(root);
        }
        let http = http
            .build()
            .map_err(|e| format!("Could not start the certificate request: {e}"))?;
        let issued = tokio::time::timeout(
            ISSUE_TIMEOUT,
            acme::issue(
                &http,
                &self.opts.acme_directory,
                &account,
                host,
                &csr,
                &self.challenges,
            ),
        )
        .await;
        self.challenges
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        let chain = issued
            .unwrap_or_else(|_| Err("the certificate authority took too long".into()))
            .map_err(|e| {
                format!(
                    "Could not get a certificate for {host} ({e}). The certificate authority \
                     must reach this server at that address on port 80. Open ports 80 and \
                     {} in your firewall (with Docker, publish them), then retry.",
                    self.opts.https_addr.port()
                )
            })?;
        // Refuse a chain the HTTPS listener could not serve before anything is replaced.
        origin_tls::server_config(&chain, &key)?;
        save(DIRECT_KEY_FILE, &key)?;
        save(DIRECT_CERT_FILE, chain.as_bytes())
    }
}
