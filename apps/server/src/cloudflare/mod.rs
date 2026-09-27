//! Cloudflare setup for self-hosted servers (plan section D, "Cloudflare setup checklist").
//!
//! An admin gives a domain, a connection type and an API token; setup then runs a
//! checklist in the background and the admin UI polls its per-line state:
//!
//! 1. check the token, 2. find the zone,
//! 3. **Tunnel**: create a remotely managed tunnel routed to this server, start the
//!    connector, point the domain at the tunnel; **Public IP**: get an Origin CA
//!    certificate and serve HTTPS with it, set SSL to Full (strict), point the domain
//!    at the server's address,
//! 4. cache rules for covers, page images, proxied images and static files,
//! 5. make the zone respect the server's own cache headers,
//! 6. fetch `https://<domain>/health`.
//!
//! Visitor addresses: setup also sets the trusted proxies (loopback for a tunnel,
//! Cloudflare's published ranges for a public IP) unless `TRUSTED_PROXY_CIDRS` pins them.
//!
//! **What is kept.** The API token is used for one run and dropped; re-running or
//! removing asks for it again (plan open point 2). Only what "Remove" needs is stored
//! ([`Stored`], in `instance_setting`): zone, tunnel, record, certificate and rule ids,
//! and the zone settings as they were before. Each id is saved the moment the thing
//! exists, so a failed run can be retried or removed without leaking anything. The
//! tunnel's run token and the origin key are credentials the server needs across
//! restarts; they live in 0600 files next to the database (see `connector.rs`).
//!
//! Nothing here runs unless an admin starts it: with no stored setup, boot does nothing.
//!
//! **Without a domain.** The same manager also owns the "public without a domain" mode
//! (`direct.rs`): HTTPS at the server's own address, with a Let's Encrypt certificate
//! that the server gets and renews by itself (`acme.rs`). No Cloudflare is involved.

pub mod acme;
pub mod api;
pub mod connector;
mod direct;
mod fetch_connector;
pub mod origin_tls;

pub use direct::{
    from_internet, https_in_front, name_on_internet, plain_http_guard, typed_host, TypedHost,
};
#[cfg(test)]
mod tests;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use crate::db::Database;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::instance;
use api::{ApiToken, Client};
use connector::Connector;

/// Runtime knobs, read once at boot.
#[derive(Clone, Debug)]
pub struct Options {
    /// Cloudflare v4 API base (`KOMIKA_CLOUDFLARE_API`; tests point it at a mock).
    pub api_base: String,
    /// Replaces the pinned release base URL (`KOMIKA_CLOUDFLARED_DOWNLOAD_BASE`).
    pub download_base: String,
    /// Where "detect my public address" asks (`KOMIKA_CLOUDFLARE_TRACE_URL`); answers
    /// `ip=<addr>` lines like Cloudflare's `/cdn-cgi/trace`.
    pub trace_url: String,
    /// Replaces `https://<domain>` for the final health check
    /// (`KOMIKA_CLOUDFLARE_HEALTH_BASE`). For tests against a mock only.
    pub health_base: Option<String>,
    /// `CLOUDFLARED_BIN`; unset searches PATH, then `<data_dir>/bin`.
    pub cloudflared_bin: Option<String>,
    /// Where `cloudflared` is looked for when that is unset: the server's `PATH`.
    pub search_path: std::ffi::OsString,
    /// Holds the tunnel token and the origin key/certificate (0600 files).
    pub data_dir: PathBuf,
    /// What the tunnel routes to: this server's plain HTTP listener.
    pub origin_service: String,
    /// Where that plain HTTP listener is bound (`BIND_ADDR`, `PORT`).
    pub listen: SocketAddr,
    /// The HTTPS listener for public-IP mode and for direct mode
    /// (`KOMIKA_HTTPS_PORT`, default 443).
    pub https_addr: SocketAddr,
    /// Direct mode: the plain HTTP listener the certificate authority checks
    /// (`KOMIKA_ACME_PORT`, default 80; Let's Encrypt always connects to port 80).
    pub acme_addr: SocketAddr,
    /// Direct mode: the ACME directory (`KOMIKA_ACME_DIRECTORY`). Let's Encrypt unless
    /// set: a private certificate authority, or a mock in tests.
    pub acme_directory: String,
    /// A PEM root to trust when talking to that directory (`KOMIKA_ACME_CA_FILE`).
    pub acme_ca_file: Option<PathBuf>,
    /// Direct mode: how often the server looks whether its certificate is due.
    pub renew_check: Duration,
    pub health_attempts: u32,
    pub health_delay: Duration,
    pub connector_timeout: Duration,
}

impl Options {
    pub fn from_env(cfg: &crate::config::Config) -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let bind: IpAddr = cfg.bind_addr.parse().unwrap_or(IpAddr::from([0, 0, 0, 0]));
        // cloudflared runs on this machine: reach the listener over loopback unless it
        // is bound to one specific address.
        let origin_host = if bind.is_unspecified() || bind.is_loopback() {
            "127.0.0.1".to_string()
        } else if bind.is_ipv6() {
            format!("[{bind}]")
        } else {
            bind.to_string()
        };
        let https_port = env("KOMIKA_HTTPS_PORT")
            .and_then(|p| p.parse().ok())
            .unwrap_or(443);
        Self {
            api_base: env("KOMIKA_CLOUDFLARE_API")
                .unwrap_or_else(|| "https://api.cloudflare.com/client/v4".into()),
            download_base: env("KOMIKA_CLOUDFLARED_DOWNLOAD_BASE")
                .unwrap_or_else(|| fetch_connector::BASE.into()),
            trace_url: env("KOMIKA_CLOUDFLARE_TRACE_URL")
                .unwrap_or_else(|| "https://www.cloudflare.com/cdn-cgi/trace".into()),
            health_base: env("KOMIKA_CLOUDFLARE_HEALTH_BASE"),
            cloudflared_bin: env("CLOUDFLARED_BIN"),
            search_path: std::env::var_os("PATH").unwrap_or_default(),
            data_dir: env("KOMIKA_CLOUDFLARE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| data_dir_for(&cfg.database_url)),
            origin_service: format!("http://{origin_host}:{}", cfg.port),
            listen: SocketAddr::new(bind, cfg.port),
            https_addr: SocketAddr::new(bind, https_port),
            acme_addr: SocketAddr::new(
                bind,
                env("KOMIKA_ACME_PORT")
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(80),
            ),
            acme_directory: env("KOMIKA_ACME_DIRECTORY")
                .unwrap_or_else(|| acme::LETS_ENCRYPT.into()),
            acme_ca_file: env("KOMIKA_ACME_CA_FILE").map(PathBuf::from),
            renew_check: Duration::from_secs(3600),
            health_attempts: 30,
            health_delay: Duration::from_secs(4),
            connector_timeout: Duration::from_secs(45),
        }
    }
}

/// `<directory of the main database>/cloudflare`, so the credentials sit with the
/// data they belong to (and on the same volume in Docker).
fn data_dir_for(database_url: &str) -> PathBuf {
    let path = database_url
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:");
    let path = path.split('?').next().unwrap_or_default();
    if path.is_empty() || path.contains(":memory:") {
        return std::env::temp_dir().join("komika-cloudflare");
    }
    Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join("cloudflare")
}

const TUNNEL_TOKEN_FILE: &str = "tunnel-token";
const ORIGIN_KEY_FILE: &str = "origin-key.der";
const ORIGIN_CERT_FILE: &str = "origin-cert.pem";

const LOOPBACK_CIDRS: [&str; 2] = ["127.0.0.1/32", "::1/128"];

/// Connection type: a Cloudflare Tunnel, or a proxied record at the server's address.
#[derive(async_graphql::Enum, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connection {
    Tunnel,
    PublicIp,
}

/// What setup created, for "Remove" and re-runs. Never holds a secret.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Stored {
    pub domain: String,
    pub connection: Option<Connection>,
    #[serde(default)]
    pub public_ip: Option<String>,
    /// Re-detect on retry while keeping an address the owner entered.
    #[serde(default)]
    pub public_ip_detected: bool,
    #[serde(default)]
    pub zone_id: Option<String>,
    #[serde(default)]
    pub zone_name: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub tunnel_id: Option<String>,
    #[serde(default)]
    pub dns_record_ids: Vec<String>,
    #[serde(default)]
    pub certificate_id: Option<String>,
    /// The zone's `ssl` value before setup changed it (restored by "Remove").
    #[serde(default)]
    pub ssl_previous: Option<Value>,
    #[serde(default)]
    pub ruleset_id: Option<String>,
    /// Setup created the zone's cache-rule entrypoint (so "Remove" may delete it once
    /// our rules are gone and it is empty).
    #[serde(default)]
    pub ruleset_created: bool,
    #[serde(default)]
    pub rule_ids: Vec<String>,
    /// The zone's `browser_cache_ttl` before setup changed it.
    #[serde(default)]
    pub browser_cache_ttl_previous: Option<Value>,
    /// Setup wrote the trusted-proxy setting (so "Remove" clears it).
    #[serde(default)]
    pub trusted_proxies_set: bool,
    #[serde(default)]
    pub complete: bool,
}

impl Stored {
    /// Anything left that "Remove" would have to delete or restore.
    pub fn has_resources(&self) -> bool {
        self.tunnel_id.is_some()
            || !self.dns_record_ids.is_empty()
            || self.certificate_id.is_some()
            || self.ssl_previous.is_some()
            || self.ruleset_id.is_some()
            || !self.rule_ids.is_empty()
            || self.browser_cache_ttl_previous.is_some()
            || self.trusted_proxies_set
    }
}

pub async fn load_stored(pool: &Database) -> Option<Stored> {
    let raw = instance::resolve(pool, &instance::CLOUDFLARE_STATE)
        .await
        .value;
    if raw.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&raw)
        .inspect_err(|e| tracing::warn!(error = %e, "unreadable cloudflare_state; ignoring"))
        .ok()
}

async fn save_stored(pool: &Database, s: Option<&Stored>) -> Result<(), String> {
    let raw = match s {
        Some(s) => serde_json::to_string(s).map_err(|e| e.to_string())?,
        None => String::new(),
    };
    instance::store(pool, &instance::CLOUDFLARE_STATE, &raw)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "saving cloudflare_state failed");
            "The server could not save its progress. Check its log.".to_string()
        })
}

// ---- checklist -----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Token,
    Zone,
    GetConnector,
    Tunnel,
    Connector,
    TunnelDns,
    Certificate,
    Ssl,
    Dns,
    ClientIp,
    CacheRules,
    CacheHeaders,
    Health,
    // remove
    DeleteDns,
    StopConnector,
    DeleteTunnel,
    RestoreSsl,
    RevokeCertificate,
    DeleteCacheRules,
    RestoreCacheHeaders,
    ResetClientIp,
}

impl Step {
    fn id(self) -> &'static str {
        match self {
            Step::Token => "token",
            Step::Zone => "zone",
            Step::GetConnector => "get_connector",
            Step::Tunnel => "tunnel",
            Step::Connector => "connector",
            Step::TunnelDns => "tunnel_dns",
            Step::Certificate => "certificate",
            Step::Ssl => "ssl",
            Step::Dns => "dns",
            Step::ClientIp => "client_ip",
            Step::CacheRules => "cache_rules",
            Step::CacheHeaders => "cache_headers",
            Step::Health => "health",
            Step::DeleteDns => "delete_dns",
            Step::StopConnector => "stop_connector",
            Step::DeleteTunnel => "delete_tunnel",
            Step::RestoreSsl => "restore_ssl",
            Step::RevokeCertificate => "revoke_certificate",
            Step::DeleteCacheRules => "delete_cache_rules",
            Step::RestoreCacheHeaders => "restore_cache_headers",
            Step::ResetClientIp => "reset_client_ip",
        }
    }

    fn label(self, domain: &str) -> String {
        match self {
            Step::Token => "Check the token".into(),
            Step::Zone => format!("Find the zone for {domain}"),
            Step::GetConnector => "Get the connector (cloudflared)".into(),
            Step::Tunnel => "Create the tunnel and route it to this server".into(),
            Step::Connector => "Start the connector".into(),
            Step::TunnelDns => format!("Point {domain} at the tunnel"),
            Step::Certificate => "Create the origin certificate and serve HTTPS".into(),
            Step::Ssl => "Set SSL to Full (strict)".into(),
            Step::Dns => format!("Point {domain} at this server"),
            Step::ClientIp => "Read visitor addresses from Cloudflare".into(),
            Step::CacheRules => "Create cache rules for images and static files".into(),
            Step::CacheHeaders => "Respect the server's cache headers".into(),
            Step::Health => format!("Test https://{domain}/health"),
            Step::DeleteDns => format!("Delete the DNS record for {domain}"),
            Step::StopConnector => "Stop the connector".into(),
            Step::DeleteTunnel => "Delete the tunnel".into(),
            Step::RestoreSsl => "Restore the SSL mode".into(),
            Step::RevokeCertificate => "Revoke the origin certificate".into(),
            Step::DeleteCacheRules => "Delete the cache rules".into(),
            Step::RestoreCacheHeaders => "Restore the browser cache setting".into(),
            Step::ResetClientIp => "Stop reading visitor addresses from Cloudflare".into(),
        }
    }
}

fn setup_steps(c: Connection) -> Vec<Step> {
    // The record that sends traffic here comes after everything that makes the origin
    // ready for it (connector or HTTPS, client addresses).
    let mut v = vec![Step::Token, Step::Zone];
    match c {
        Connection::Tunnel => v.extend([
            Step::GetConnector,
            Step::Tunnel,
            Step::Connector,
            Step::ClientIp,
            Step::TunnelDns,
        ]),
        Connection::PublicIp => v.extend([Step::Certificate, Step::Ssl, Step::ClientIp, Step::Dns]),
    }
    v.extend([Step::CacheRules, Step::CacheHeaders, Step::Health]);
    v
}

fn remove_steps(s: &Stored) -> Vec<Step> {
    // DNS first, so traffic stops arriving before the origin side goes away.
    let mut v = vec![Step::Token, Step::DeleteDns];
    if s.tunnel_id.is_some() || s.connection == Some(Connection::Tunnel) {
        v.extend([Step::StopConnector, Step::DeleteTunnel]);
    }
    if s.certificate_id.is_some() || s.ssl_previous.is_some() {
        v.extend([Step::RestoreSsl, Step::RevokeCertificate]);
    }
    v.extend([
        Step::DeleteCacheRules,
        Step::RestoreCacheHeaders,
        Step::ResetClientIp,
    ]);
    v
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineState {
    Waiting,
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug)]
struct Line {
    step: Step,
    label: String,
    state: LineState,
    message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobKind {
    Setup,
    Remove,
}

#[derive(Clone, Debug)]
struct Job {
    kind: JobKind,
    lines: Vec<Line>,
    running: bool,
}

// ---- GraphQL views -------------------------------------------------------------

/// One checklist line.
#[derive(async_graphql::SimpleObject, Clone, Debug)]
pub struct CloudflareStep {
    pub id: String,
    pub label: String,
    /// `waiting`, `running`, `done` or `failed`.
    pub state: String,
    pub message: Option<String>,
}

/// The current or last setup / remove run.
#[derive(async_graphql::SimpleObject, Clone, Debug)]
pub struct CloudflareJob {
    /// `setup` or `remove`.
    pub kind: String,
    pub running: bool,
    pub failed: bool,
    pub steps: Vec<CloudflareStep>,
}

/// What setup created (no secrets).
#[derive(async_graphql::SimpleObject, Clone, Debug)]
pub struct CloudflareSetup {
    pub domain: String,
    pub connection: Option<Connection>,
    pub zone_name: Option<String>,
    pub complete: bool,
    /// Whether anything is left for "Remove".
    pub removable: bool,
}

/// Settings › Access.
#[derive(async_graphql::SimpleObject, Clone, Debug)]
pub struct AccessStatus {
    /// `local`, `internet`, `direct`, or empty when nobody chose yet.
    pub mode: String,
    pub domain: Option<String>,
    /// What readers type: `https://<domain>` in internet mode, the stored public
    /// address in direct mode.
    pub address: Option<String>,
    /// `http://<address>:<port>` for each way a device on the home network reaches
    /// this server, best first. Empty when the server only answers on this computer,
    /// or runs in a container and cannot know.
    pub lan_addresses: Vec<String>,
    /// Last private socket peer seen since boot (RFC 3339); unknown in a container.
    pub lan_client_seen_at: Option<String>,
    pub cloudflare: Option<CloudflareSetup>,
    /// `off`, `starting`, `running`, `failed` or `missing`.
    pub connector: String,
    pub connector_message: Option<String>,
    /// The HTTPS listener is up (Cloudflare public-IP mode, or direct mode with a
    /// certificate).
    pub https: bool,
    /// Direct mode with HTTPS: when the current certificate stops being valid
    /// (RFC 3339). The server renews it long before.
    pub certificate_expires: Option<String>,
    /// Direct mode with HTTPS: why the last renewal failed, until one succeeds.
    pub certificate_error: Option<String>,
    /// Direct mode with HTTPS: the plain HTTP port refuses clients on the internet.
    /// It starts to after the first visit from the internet over HTTPS.
    pub plain_http_closed: bool,
    pub trusted_proxies: Vec<String>,
    /// `TRUSTED_PROXY_CIDRS` is set on the server, so setup leaves it alone.
    pub trusted_proxies_pinned: bool,
    pub job: Option<CloudflareJob>,
}

/// "Check token": what the token can see for the domain. No secrets.
#[derive(async_graphql::SimpleObject, Clone, Debug)]
pub struct CloudflareTokenCheck {
    pub zone_name: String,
    pub zone_status: String,
    pub account_name: String,
    /// Read probes can suggest missing rows, but cannot prove Edit permission.
    pub missing: Vec<String>,
}

/// Dependency checks for Settings › Access. No setup changes are made.
#[derive(async_graphql::SimpleObject, Clone, Debug)]
pub struct AccessPreflight {
    /// `found`, `download` or `unsupported`.
    pub cloudflared: String,
    pub cloudflared_detail: Option<String>,
    pub http_port: i32,
    pub https_port: i32,
    /// `free`, `serving`, `in_use` or `denied`; `unknown` for other OS errors.
    pub https_port_state: String,
    pub acme_port: i32,
    pub acme_port_state: String,
    pub in_container: bool,
    pub public_ip: Option<String>,
}

#[derive(async_graphql::InputObject)]
pub struct CloudflareSetupInput {
    pub token: String,
    pub domain: String,
    pub connection: Connection,
    /// Public-IP mode only; empty detects the server's address.
    pub public_ip: Option<String>,
}

// ---- manager -------------------------------------------------------------------

pub struct Manager {
    pool: Database,
    opts: Options,
    trusted: Arc<crate::TrustedProxies>,
    connector: Connector,
    job: Mutex<Option<Job>>,
    /// The public router, handed over by `main` once built, for the HTTPS listener.
    router: OnceLock<axum::Router>,
    /// The HTTPS server task and its accept loop.
    https: Mutex<Option<[tokio::task::JoinHandle<()>; 2]>>,
    /// Direct mode with HTTPS (`direct.rs`): its port-80 listener, its renewal loop,
    /// the answers to the certificate authority's checks, a lock so that one
    /// certificate is requested at a time, and why the last renewal failed.
    challenge_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    renew_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    challenges: acme::Challenges,
    issuing: tokio::sync::Mutex<()>,
    certificate_error: Mutex<Option<String>>,
    /// Direct mode with HTTPS, for `plain_http_guard`: the HTTPS address, and whether
    /// a client on the internet has used it (`instance::DIRECT_HTTPS_SEEN`).
    direct_origin: Arc<RwLock<Option<String>>>,
    https_seen: AtomicBool,
    lan_client_seen_at: AtomicI64,
    in_container: bool,
}

type StepResult = Result<Option<String>, String>;

impl Manager {
    pub fn new(pool: Database, opts: Options, trusted: Arc<crate::TrustedProxies>) -> Self {
        let connector = Connector::new(
            opts.cloudflared_bin.clone(),
            opts.search_path.clone(),
            &opts.data_dir,
        );
        Self {
            pool,
            opts,
            trusted,
            connector,
            job: Mutex::new(None),
            router: OnceLock::new(),
            https: Mutex::new(None),
            challenge_task: Mutex::new(None),
            renew_task: Mutex::new(None),
            challenges: acme::Challenges::default(),
            issuing: tokio::sync::Mutex::new(()),
            certificate_error: Mutex::new(None),
            direct_origin: Arc::new(RwLock::new(None)),
            https_seen: AtomicBool::new(false),
            lan_client_seen_at: AtomicI64::new(0),
            in_container: crate::lan::in_container(),
        }
    }

    pub fn set_router(&self, router: axum::Router) {
        let _ = self.router.set(router);
    }

    /// Boot: bring back what a completed or partial setup left running (connector,
    /// HTTPS listener). No stored setup, no work — the hosted instance never gets here.
    pub async fn resume(self: &Arc<Self>) {
        self.resume_direct_tls().await;
        let mode = instance::resolve(&self.pool, &instance::ACCESS_MODE)
            .await
            .value;
        if !matches!(mode.as_str(), "local" | "direct") {
            if let Err(e) = self.resume_cloudflare().await {
                tracing::warn!("cloudflare not resumed: {e}");
            }
        }
    }

    /// Keep Cloudflare's resources and credentials for a later return or removal.
    async fn pause_cloudflare(&self) {
        self.connector.stop().await;
        if self.direct_tls_host().await.is_none()
            && load_stored(&self.pool)
                .await
                .is_some_and(|s| s.certificate_id.is_some())
        {
            self.stop_https().await;
        }
    }

    async fn resume_cloudflare(&self) -> Result<(), String> {
        let Some(s) = load_stored(&self.pool).await else {
            return Ok(());
        };
        if s.tunnel_id.is_some() && self.token_file().is_file() {
            self.connector.start(&self.token_file())?;
        }
        if s.certificate_id.is_some() {
            self.start_https(ORIGIN_KEY_FILE, ORIGIN_CERT_FILE).await?;
        }
        Ok(())
    }

    fn token_file(&self) -> PathBuf {
        self.opts.data_dir.join(TUNNEL_TOKEN_FILE)
    }

    fn job_view(&self) -> Option<CloudflareJob> {
        let g = self.job.lock().unwrap_or_else(|p| p.into_inner());
        g.as_ref().map(|j| CloudflareJob {
            kind: match j.kind {
                JobKind::Setup => "setup",
                JobKind::Remove => "remove",
            }
            .into(),
            running: j.running,
            failed: j.lines.iter().any(|l| l.state == LineState::Failed),
            steps: j
                .lines
                .iter()
                .map(|l| CloudflareStep {
                    id: l.step.id().into(),
                    label: l.label.clone(),
                    state: match l.state {
                        LineState::Waiting => "waiting",
                        LineState::Running => "running",
                        LineState::Done => "done",
                        LineState::Failed => "failed",
                    }
                    .into(),
                    message: l.message.clone(),
                })
                .collect(),
        })
    }

    pub async fn status(&self) -> AccessStatus {
        let stored = load_stored(&self.pool).await;
        let mode = instance::resolve(&self.pool, &instance::ACCESS_MODE)
            .await
            .value;
        let domain = Some(
            instance::resolve(&self.pool, &instance::PUBLIC_DOMAIN)
                .await
                .value,
        )
        .filter(|d| !d.is_empty())
        .or_else(|| stored.as_ref().map(|s| s.domain.clone()));
        let (connector, connector_message) = self.connector.status();
        let https = self.https_up();
        let direct_tls = mode == "direct" && self.direct_tls_host().await.is_some();
        let address = match mode.as_str() {
            "internet" => domain.as_ref().map(|d| format!("https://{d}")),
            "direct" => Some(
                instance::resolve(&self.pool, &instance::PUBLIC_ADDRESS)
                    .await
                    .value,
            )
            .filter(|a| !a.is_empty()),
            _ => None,
        };
        AccessStatus {
            address,
            lan_addresses: crate::lan::addresses(self.opts.listen.ip(), self.opts.listen.port()),
            lan_client_seen_at: match self.lan_client_seen_at.load(Ordering::Relaxed) {
                0 => None,
                at => chrono::DateTime::from_timestamp(at, 0).map(|t| t.to_rfc3339()),
            },
            mode,
            domain,
            cloudflare: stored.map(|s| CloudflareSetup {
                removable: s.has_resources(),
                domain: s.domain,
                connection: s.connection,
                zone_name: s.zone_name,
                complete: s.complete,
            }),
            connector: connector.as_str().into(),
            connector_message,
            https,
            certificate_expires: direct_tls
                .then(|| self.certificate_dates())
                .flatten()
                .map(|(_, until)| until.to_rfc3339()),
            certificate_error: self
                .certificate_error
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
                .filter(|_| direct_tls),
            plain_http_closed: direct_tls && self.https_seen.load(Ordering::Relaxed),
            trusted_proxies: self.trusted.raw(),
            trusted_proxies_pinned: self.trusted.is_pinned(),
            job: self.job_view(),
        }
    }

    pub fn record_lan_peer(&self, peer: IpAddr) {
        if !self.in_container && crate::lan::is_lan_client(peer) {
            self.lan_client_seen_at
                .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
        }
    }

    pub async fn preflight(&self) -> AccessPreflight {
        let (cloudflared, cloudflared_detail) = match self.connector.resolve_bin() {
            Ok(path) => ("found", Some(path.display().to_string())),
            Err(message) if self.opts.cloudflared_bin.is_some() => ("unsupported", Some(message)),
            Err(_) if fetch_connector::current_release().is_some() => ("download", None),
            Err(_) => ("unsupported", Some(fetch_connector::UNSUPPORTED.into())),
        };
        let acme_serving = self
            .challenge_task
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|task| !task.is_finished());
        let (https_state, acme_state, public_ip) = tokio::join!(
            port_state(self.opts.https_addr, self.https_up()),
            port_state(self.opts.acme_addr, acme_serving),
            tokio::time::timeout(
                Duration::from_secs(3),
                detect_public_ip(&self.opts.trace_url)
            ),
        );
        AccessPreflight {
            cloudflared: cloudflared.into(),
            cloudflared_detail,
            http_port: self.opts.listen.port().into(),
            https_port: self.opts.https_addr.port().into(),
            https_port_state: https_state.into(),
            acme_port: self.opts.acme_addr.port().into(),
            acme_port_state: acme_state.into(),
            in_container: self.in_container,
            public_ip: public_ip.ok().and_then(Result::ok),
        }
    }

    async fn get_connector(&self) -> StepResult {
        match self.connector.resolve_bin() {
            Ok(path) => return Ok(Some(format!("Found at {}.", path.display()))),
            Err(message) if self.opts.cloudflared_bin.is_some() => return Err(message),
            Err(_) => {}
        }
        let pin = fetch_connector::current_release().ok_or(fetch_connector::UNSUPPORTED)?;
        fetch_connector::fetch(&self.opts.data_dir, &self.opts.download_base, &pin).await?;
        Ok(Some(format!(
            "Downloaded cloudflared {}.",
            fetch_connector::VERSION
        )))
    }

    /// "Check token": verify it and find the zone for `domain`. Nothing is changed.
    pub async fn check_token(
        &self,
        token: &str,
        domain: &str,
        connection: Option<Connection>,
    ) -> Result<CloudflareTokenCheck, String> {
        let domain = normalize_domain(domain)?;
        let token = ApiToken::new(token).ok_or("Paste the API token first.")?;
        let api = Client::new(&self.opts.api_base, token);
        verify(&api).await?;
        let zone = find_zone(&api, &domain).await?;
        let missing = api.missing_permissions(&zone, connection).await;
        Ok(CloudflareTokenCheck {
            zone_name: zone.name,
            zone_status: zone.status,
            account_name: zone.account.name,
            missing,
        })
    }

    fn begin(&self, kind: JobKind, steps: &[Step], domain: &str) -> Result<(), String> {
        let mut g = self.job.lock().unwrap_or_else(|p| p.into_inner());
        if g.as_ref().is_some_and(|j| j.running) {
            return Err("Cloudflare setup is already running. Wait for it to finish.".into());
        }
        *g = Some(Job {
            kind,
            running: true,
            lines: steps
                .iter()
                .map(|s| Line {
                    step: *s,
                    label: s.label(domain),
                    state: LineState::Waiting,
                    message: None,
                })
                .collect(),
        });
        Ok(())
    }

    fn mark(&self, i: usize, state: LineState, message: Option<String>) {
        let mut g = self.job.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(line) = g.as_mut().and_then(|j| j.lines.get_mut(i)) {
            line.state = state;
            line.message = message;
        }
    }

    fn finish(&self) {
        let mut g = self.job.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(j) = g.as_mut() {
            j.running = false;
        }
    }

    /// Start setup in the background. The token stays in memory for this run only.
    pub async fn start_setup(self: &Arc<Self>, input: CloudflareSetupInput) -> Result<(), String> {
        let domain = normalize_domain(&input.domain)?;
        let token = ApiToken::new(&input.token).ok_or("Paste the API token first.")?;
        let public_ip = input
            .public_ip
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(ip) = &public_ip {
            check_public_ip(ip)?;
        }
        if input.connection == Connection::PublicIp && self.direct_tls_host().await.is_some() {
            return Err(
                "This server serves HTTPS at its own address, on the port Cloudflare would \
                 use. Choose \"Only my devices\" first, or use a Tunnel."
                    .into(),
            );
        }
        let existing = load_stored(&self.pool).await;
        let stored = match existing {
            Some(s)
                if s.has_resources()
                    && (s.domain != domain || s.connection != Some(input.connection)) =>
            {
                return Err(format!(
                    "Cloudflare is already set up for {}. Remove that setup first.",
                    s.domain
                ));
            }
            Some(mut s) if s.domain == domain => {
                s.connection = Some(input.connection);
                s.public_ip_detected = public_ip.is_none();
                s.public_ip = public_ip;
                s.complete = false;
                s
            }
            _ => Stored {
                domain: domain.clone(),
                connection: Some(input.connection),
                public_ip_detected: public_ip.is_none(),
                public_ip,
                ..Stored::default()
            },
        };
        let steps = setup_steps(input.connection);
        self.begin(JobKind::Setup, &steps, &domain)?;
        if let Err(e) = save_stored(&self.pool, Some(&stored)).await {
            self.finish();
            return Err(e);
        }
        let me = self.clone();
        tokio::spawn(async move { me.run(JobKind::Setup, steps, stored, token).await });
        Ok(())
    }

    /// Run setup again for the stored domain and connection (after a failure, or to
    /// repair a finished one). Steps whose result is already in place pass quickly.
    pub async fn retry_setup(self: &Arc<Self>, token: &str) -> Result<(), String> {
        let s = load_stored(&self.pool)
            .await
            .ok_or("There is no Cloudflare setup to retry. Start it from the beginning.")?;
        let connection = s
            .connection
            .ok_or("The stored setup has no connection type.")?;
        self.start_setup(CloudflareSetupInput {
            token: token.to_string(),
            domain: s.domain,
            connection,
            public_ip: if s.public_ip_detected {
                None
            } else {
                s.public_ip
            },
        })
        .await
    }

    /// Start "Remove" in the background: delete exactly what setup created.
    pub async fn start_remove(self: &Arc<Self>, token: &str) -> Result<(), String> {
        let token = ApiToken::new(token).ok_or("Paste the API token first.")?;
        let stored = load_stored(&self.pool)
            .await
            .ok_or("Cloudflare is not set up on this server.")?;
        let steps = remove_steps(&stored);
        self.begin(JobKind::Remove, &steps, &stored.domain)?;
        let me = self.clone();
        tokio::spawn(async move { me.run(JobKind::Remove, steps, stored, token).await });
        Ok(())
    }

    async fn run(self: Arc<Self>, kind: JobKind, steps: Vec<Step>, mut s: Stored, token: ApiToken) {
        let api = Client::new(&self.opts.api_base, token);
        let mut ok = true;
        for (i, step) in steps.iter().enumerate() {
            self.mark(i, LineState::Running, None);
            let result = self.step(*step, &api, &mut s).await;
            // Save after every step, failed or not: an id recorded mid-step must not be
            // lost, or "Remove" could not delete what it names.
            let saved = save_stored(&self.pool, Some(&s)).await;
            match result.and_then(|m| saved.map(|_| m)) {
                Ok(message) => self.mark(i, LineState::Done, message),
                Err(message) => {
                    tracing::warn!(
                        step = step.id(),
                        "cloudflare {:?} step failed: {message}",
                        kind
                    );
                    self.mark(i, LineState::Failed, Some(message));
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            match kind {
                JobKind::Setup => {
                    s.complete = true;
                    let _ = save_stored(&self.pool, Some(&s)).await;
                }
                JobKind::Remove => {
                    let _ = save_stored(&self.pool, None).await;
                    let _ = instance::store(&self.pool, &instance::PUBLIC_DOMAIN, "").await;
                    let _ = instance::store(&self.pool, &instance::ACCESS_MODE, "local").await;
                }
            }
            // Either way the server is no longer public at its own address.
            let _one = self.issuing.lock().await;
            self.disable_direct_tls().await;
        }
        self.finish();
    }

    async fn step(&self, step: Step, api: &Client, s: &mut Stored) -> StepResult {
        match step {
            Step::Token => verify(api)
                .await
                .map(|_| Some("The token is active.".into())),
            Step::Zone => {
                let zone = find_zone(api, &s.domain).await?;
                let msg = format!("Zone {} ({}).", zone.name, zone.status);
                s.zone_id = Some(zone.id);
                s.zone_name = Some(zone.name);
                s.account_id = Some(zone.account.id);
                save_stored(&self.pool, Some(s)).await?;
                Ok(Some(msg))
            }
            Step::GetConnector => self.get_connector().await,
            Step::Tunnel => self.step_tunnel(api, s).await,
            Step::Connector => {
                self.connector.start(&self.token_file())?;
                self.connector
                    .wait_running(self.opts.connector_timeout)
                    .await?;
                Ok(Some("Connected to Cloudflare.".into()))
            }
            Step::TunnelDns => {
                let tunnel = s.tunnel_id.clone().ok_or("No tunnel yet.")?;
                self.ensure_record(api, s, "CNAME", &format!("{tunnel}.cfargotunnel.com"))
                    .await
            }
            Step::Certificate => self.step_certificate(api, s).await,
            Step::Ssl => {
                let zone = zone_id(s)?;
                let current = api
                    .zone_setting(&zone, "ssl")
                    .await
                    .map_err(|e| e.message)?;
                if s.ssl_previous.is_none() {
                    s.ssl_previous = Some(current.clone());
                    save_stored(&self.pool, Some(s)).await?;
                }
                if current != json!("strict") {
                    api.set_zone_setting(&zone, "ssl", json!("strict"))
                        .await
                        .map_err(|e| e.message)?;
                }
                Ok(Some(
                    "SSL is Full (strict). This applies to the whole zone.".into(),
                ))
            }
            Step::Dns => {
                let ip = match &s.public_ip {
                    Some(ip) => ip.clone(),
                    None => {
                        let ip = detect_public_ip(&self.opts.trace_url).await?;
                        s.public_ip = Some(ip.clone());
                        s.public_ip_detected = true;
                        ip
                    }
                };
                let kind = if ip.contains(':') { "AAAA" } else { "A" };
                self.ensure_record(api, s, kind, &ip).await
            }
            Step::ClientIp => {
                if self.trusted.is_pinned() {
                    return Ok(Some(
                        "TRUSTED_PROXY_CIDRS is set on this server, so it was left as is.".into(),
                    ));
                }
                let list: Vec<String> = match s.connection {
                    Some(Connection::PublicIp) => {
                        let ips = api.cloudflare_ips().await.map_err(|e| e.message)?;
                        ips.ipv4_cidrs.into_iter().chain(ips.ipv6_cidrs).collect()
                    }
                    _ => LOOPBACK_CIDRS.iter().map(|c| c.to_string()).collect(),
                };
                if list.is_empty() {
                    return Err("Cloudflare returned no address ranges.".into());
                }
                instance::store(&self.pool, &instance::TRUSTED_PROXY_CIDRS, &list.join(","))
                    .await
                    .map_err(|_| "The server could not save the trusted proxies.".to_string())?;
                self.trusted.replace(&list);
                s.trusted_proxies_set = true;
                save_stored(&self.pool, Some(s)).await?;
                Ok(Some(match s.connection {
                    Some(Connection::PublicIp) => {
                        format!("Trusting Cloudflare's {} address ranges.", list.len())
                    }
                    _ => "The connector on this machine passes visitor addresses on.".into(),
                }))
            }
            Step::CacheRules => self.step_cache_rules(api, s).await,
            Step::CacheHeaders => {
                let zone = zone_id(s)?;
                let current = api
                    .zone_setting(&zone, "browser_cache_ttl")
                    .await
                    .map_err(|e| e.message)?;
                if s.browser_cache_ttl_previous.is_none() {
                    s.browser_cache_ttl_previous = Some(current.clone());
                    save_stored(&self.pool, Some(s)).await?;
                }
                // 0 is "Respect Existing Headers".
                if current != json!(0) {
                    api.set_zone_setting(&zone, "browser_cache_ttl", json!(0))
                        .await
                        .map_err(|e| e.message)?;
                }
                Ok(Some("Browsers keep what the server's headers say.".into()))
            }
            Step::Health => {
                let message = self.step_health(s).await?;
                instance::store(&self.pool, &instance::PUBLIC_DOMAIN, &s.domain)
                    .await
                    .map_err(|_| "Could not save the domain.".to_string())?;
                instance::store(&self.pool, &instance::ACCESS_MODE, "internet")
                    .await
                    .map_err(|_| "Could not save the access choice.".to_string())?;
                Ok(message)
            }

            Step::DeleteDns => {
                let zone = zone_id(s);
                while let Some(id) = s.dns_record_ids.first().cloned() {
                    let zone = zone.clone()?;
                    gone_ok(api.delete_dns_record(&zone, &id).await)?;
                    s.dns_record_ids.remove(0);
                }
                Ok(None)
            }
            Step::StopConnector => {
                self.connector.stop().await;
                remove_file(&self.token_file())?;
                Ok(None)
            }
            Step::DeleteTunnel => {
                if let Some(t) = s.tunnel_id.clone() {
                    let account = s.account_id.clone().ok_or("No account id stored.")?;
                    gone_ok(api.delete_tunnel(&account, &t).await)?;
                    s.tunnel_id = None;
                }
                Ok(None)
            }
            Step::RestoreSsl => {
                if let Some(prev) = s.ssl_previous.clone() {
                    let zone = zone_id(s)?;
                    if prev != json!("strict") {
                        api.set_zone_setting(&zone, "ssl", prev)
                            .await
                            .map_err(|e| e.message)?;
                    }
                    s.ssl_previous = None;
                }
                Ok(None)
            }
            Step::RevokeCertificate => {
                self.stop_https().await;
                if let Some(id) = s.certificate_id.clone() {
                    gone_ok(api.revoke_origin_certificate(&id).await)?;
                    s.certificate_id = None;
                }
                remove_file(&self.opts.data_dir.join(ORIGIN_KEY_FILE))?;
                remove_file(&self.opts.data_dir.join(ORIGIN_CERT_FILE))?;
                Ok(None)
            }
            Step::DeleteCacheRules => {
                if let Some(rs) = s.ruleset_id.clone() {
                    let zone = zone_id(s)?;
                    while let Some(id) = s.rule_ids.first().cloned() {
                        gone_ok(api.delete_rule(&zone, &rs, &id).await)?;
                        s.rule_ids.remove(0);
                    }
                    if s.ruleset_created {
                        match api.ruleset(&zone, &rs).await {
                            Ok(r) if r.rules.is_empty() => {
                                gone_ok(api.delete_ruleset(&zone, &rs).await)?
                            }
                            Ok(_) => {}
                            Err(e) if e.is_not_found() => {}
                            Err(e) => return Err(e.message),
                        }
                    }
                    s.ruleset_id = None;
                    s.ruleset_created = false;
                }
                s.rule_ids.clear();
                Ok(None)
            }
            Step::RestoreCacheHeaders => {
                if let Some(prev) = s.browser_cache_ttl_previous.clone() {
                    let zone = zone_id(s)?;
                    if prev != json!(0) {
                        api.set_zone_setting(&zone, "browser_cache_ttl", prev)
                            .await
                            .map_err(|e| e.message)?;
                    }
                    s.browser_cache_ttl_previous = None;
                }
                Ok(None)
            }
            Step::ResetClientIp => {
                if s.trusted_proxies_set {
                    if !self.trusted.is_pinned() {
                        instance::store(&self.pool, &instance::TRUSTED_PROXY_CIDRS, "")
                            .await
                            .map_err(|_| {
                                "The server could not save the trusted proxies.".to_string()
                            })?;
                        self.trusted.replace(&[]);
                    }
                    s.trusted_proxies_set = false;
                }
                Ok(None)
            }
        }
    }

    async fn step_tunnel(&self, api: &Client, s: &mut Stored) -> StepResult {
        let account = s.account_id.clone().ok_or("No account id yet.")?;
        let tunnel = match s.tunnel_id.clone() {
            Some(t) => t,
            None => {
                // A short random suffix: tunnel names are unique per account, and a
                // second server for the same domain must not collide with the first.
                let name = format!(
                    "komiq {} {}",
                    s.domain,
                    &uuid::Uuid::new_v4().simple().to_string()[..6]
                );
                let t = api
                    .create_tunnel(&account, &name)
                    .await
                    .map_err(|e| e.message)?;
                s.tunnel_id = Some(t.id.clone());
                save_stored(&self.pool, Some(s)).await?;
                t.id
            }
        };
        api.put_tunnel_ingress(&account, &tunnel, &s.domain, &self.opts.origin_service)
            .await
            .map_err(|e| e.message)?;
        let run_token = api
            .tunnel_token(&account, &tunnel)
            .await
            .map_err(|e| e.message)?;
        origin_tls::write_private(&self.token_file(), run_token.as_bytes())
            .map_err(|e| format!("Could not save the connector token: {e}"))?;
        Ok(Some(format!(
            "Tunnel routes {} to {}.",
            s.domain, self.opts.origin_service
        )))
    }

    /// Repair only our record; another server on the zone keeps its own.
    async fn ensure_record(
        &self,
        api: &Client,
        s: &mut Stored,
        kind: &str,
        content: &str,
    ) -> StepResult {
        let zone = zone_id(s)?;
        let existing = api
            .dns_records_named(&zone, &s.domain)
            .await
            .map_err(|e| e.message)?;
        if let Some(r) = existing.iter().find(|r| s.dns_record_ids.contains(&r.id)) {
            if r.kind == kind && r.content == content && r.proxied {
                return Ok(Some(format!("{kind} record already in place.")));
            }
            if r.kind == kind {
                api.update_dns_record(&zone, &r.id, kind, &s.domain, content)
                    .await
                    .map_err(|e| e.message)?;
                return Ok(Some(format!("Proxied {kind} record to {content}.")));
            }
            gone_ok(api.delete_dns_record(&zone, &r.id).await)?;
        }
        // Ours is gone or needs a new type: record that before creating its replacement.
        let owned = std::mem::take(&mut s.dns_record_ids);
        save_stored(&self.pool, Some(s)).await?;
        if let Some(r) = existing
            .iter()
            .find(|r| !owned.contains(&r.id) && matches!(r.kind.as_str(), "A" | "AAAA" | "CNAME"))
        {
            return Err(format!(
                "{} already has a {} record ({}). Delete it in Cloudflare, or pick another \
                 domain, then retry.",
                s.domain, r.kind, r.content
            ));
        }
        let r = api
            .create_dns_record(&zone, kind, &s.domain, content)
            .await
            .map_err(|e| e.message)?;
        s.dns_record_ids.push(r.id);
        save_stored(&self.pool, Some(s)).await?;
        Ok(Some(format!("Proxied {kind} record to {content}.")))
    }

    async fn step_certificate(&self, api: &Client, s: &mut Stored) -> StepResult {
        let key_path = self.opts.data_dir.join(ORIGIN_KEY_FILE);
        let cert_path = self.opts.data_dir.join(ORIGIN_CERT_FILE);
        if s.certificate_id.is_none() || !key_path.is_file() || !cert_path.is_file() {
            if let Some(old) = s.certificate_id.take() {
                // Files lost: the old certificate is useless without its key.
                let _ = gone_ok(api.revoke_origin_certificate(&old).await);
            }
            let key = origin_tls::generate_key()?;
            let csr = origin_tls::csr_pem(&key, &s.domain)?;
            origin_tls::write_private(&key_path, &key)
                .map_err(|e| format!("Could not save the origin key: {e}"))?;
            let cert = api
                .create_origin_certificate(&csr, &s.domain)
                .await
                .map_err(|e| e.message)?;
            s.certificate_id = Some(cert.id);
            save_stored(&self.pool, Some(s)).await?;
            origin_tls::write_private(&cert_path, cert.certificate.as_bytes())
                .map_err(|e| format!("Could not save the origin certificate: {e}"))?;
        }
        self.start_https(ORIGIN_KEY_FILE, ORIGIN_CERT_FILE).await?;
        Ok(Some(format!(
            "Serving HTTPS on port {}.",
            self.opts.https_addr.port()
        )))
    }

    /// Serve the public router over HTTPS with the key and certificate in these two
    /// files of the data directory, in place of whatever the HTTPS port served before.
    async fn start_https(&self, key_file: &str, cert_file: &str) -> Result<(), String> {
        let key = std::fs::read(self.opts.data_dir.join(key_file))
            .map_err(|_| "The HTTPS key file is missing. Run setup again.".to_string())?;
        let cert = std::fs::read_to_string(self.opts.data_dir.join(cert_file))
            .map_err(|_| "The HTTPS certificate file is missing. Run setup again.".to_string())?;
        let config = origin_tls::server_config(&cert, &key)?;
        self.stop_https().await;
        let router = self
            .router
            .get()
            .cloned()
            .ok_or("The server is still starting. Retry in a moment.")?;
        let addr = self.opts.https_addr;
        let tcp = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
            format!(
                "Could not listen on port {} ({e}). The server needs permission to use that \
                 port (with Docker, publish it), or set KOMIKA_HTTPS_PORT.",
                addr.port()
            )
        })?;
        let (listener, accept) = origin_tls::TlsListener::new(tcp, config)
            .map_err(|e| format!("Could not start HTTPS: {e}"))?;
        let serve = tokio::spawn(origin_tls::serve(listener, router));
        *self.https.lock().unwrap_or_else(|p| p.into_inner()) = Some([serve, accept]);
        Ok(())
    }

    fn https_up(&self) -> bool {
        self.https
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|h| !h[0].is_finished())
    }

    /// Stop the HTTPS listener and wait until its port is free again.
    async fn stop_https(&self) {
        let tasks = self.https.lock().unwrap_or_else(|p| p.into_inner()).take();
        for t in tasks.into_iter().flatten() {
            t.abort();
            let _ = t.await;
        }
    }

    async fn step_cache_rules(&self, api: &Client, s: &mut Stored) -> StepResult {
        let zone = zone_id(s)?;
        let wanted = cache_rules(&s.domain);
        let refs: Vec<String> = wanted
            .iter()
            .filter_map(|r| r["ref"].as_str().map(String::from))
            .collect();
        let ruleset = match api.cache_entrypoint(&zone).await.map_err(|e| e.message)? {
            Some(r) => r,
            None => {
                let r = api
                    .create_cache_ruleset(&zone, wanted.clone())
                    .await
                    .map_err(|e| e.message)?;
                s.ruleset_id = Some(r.id.clone());
                s.ruleset_created = true;
                s.rule_ids = r
                    .rules
                    .iter()
                    .filter(|x| refs.contains(&x.reference))
                    .map(|x| x.id.clone())
                    .collect();
                save_stored(&self.pool, Some(s)).await?;
                return Ok(Some(format!("{} rules created.", s.rule_ids.len())));
            }
        };
        if s.ruleset_id.as_deref() != Some(&ruleset.id) {
            s.ruleset_id = Some(ruleset.id.clone());
            s.ruleset_created = false;
            save_stored(&self.pool, Some(s)).await?;
        }
        let mut current = ruleset.rules;
        let mut added = 0;
        for rule in wanted {
            let r#ref = rule["ref"].as_str().unwrap_or_default().to_string();
            let old_ref = r#ref
                .rsplit_once('_')
                .map(|(old, _)| old)
                .unwrap_or_default();
            let host = format!("http.host eq \"{}\"", s.domain);
            let owned = |x: &&api::Rule| {
                x.reference == r#ref || (x.reference == old_ref && x.expression.contains(&host))
            };
            let mut id = current.iter().find(owned).map(|x| x.id.clone());
            if id.is_none() {
                current = api
                    .add_rule(&zone, &ruleset.id, rule)
                    .await
                    .map_err(|e| e.message)?
                    .rules;
                added += 1;
                id = current.iter().find(owned).map(|x| x.id.clone());
            }
            // Record the id at once (also adopts our rule left by an earlier run).
            if let Some(id) = id {
                if !s.rule_ids.contains(&id) {
                    s.rule_ids.push(id);
                    save_stored(&self.pool, Some(s)).await?;
                }
            }
        }
        let host = format!("http.host eq \"{}\"", s.domain);
        s.rule_ids.retain(|id| {
            current.iter().any(|x| {
                &x.id == id
                    && refs.iter().any(|reference| {
                        x.reference == *reference
                            || (reference
                                .rsplit_once('_')
                                .is_some_and(|(old, _)| x.reference == old)
                                && x.expression.contains(&host))
                    })
            })
        });
        Ok(Some(if added == 0 {
            "Cache rules already in place.".into()
        } else {
            format!("{added} rules created.")
        }))
    }

    async fn step_health(&self, s: &Stored) -> StepResult {
        let base = self
            .opts
            .health_base
            .clone()
            .unwrap_or_else(|| format!("https://{}", s.domain));
        let url = format!("{}/health", base.trim_end_matches('/'));
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;
        let mut last = String::new();
        // A new record and connector take a little while to answer everywhere.
        for attempt in 0..self.opts.health_attempts.max(1) {
            if attempt > 0 {
                tokio::time::sleep(self.opts.health_delay).await;
            }
            match http.get(&url).send().await {
                Ok(r) if r.status().is_success() => {
                    let body = r.text().await.unwrap_or_default();
                    if body.trim() == "ok" {
                        return Ok(Some(format!("https://{} answers.", s.domain)));
                    }
                    last = "The address answered, but not with this server's health check.".into();
                }
                Ok(r) => last = format!("The address answered HTTP {}.", r.status().as_u16()),
                Err(e) => last = format!("No answer yet: {}", e.without_url()),
            }
        }
        Err(format!(
            "{last} DNS can take a few minutes; retry this step."
        ))
    }
}

fn zone_id(s: &Stored) -> Result<String, String> {
    s.zone_id
        .clone()
        .ok_or_else(|| "No zone found yet.".to_string())
}

/// A delete that finds nothing has nothing left to do.
fn gone_ok(r: api::ApiResult<()>) -> Result<(), String> {
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.is_not_found() => Ok(()),
        Err(e) => Err(e.message),
    }
}

fn remove_file(p: &Path) -> Result<(), String> {
    match std::fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Could not delete {}: {e}", p.display())),
    }
}

async fn verify(api: &Client) -> Result<(), String> {
    let t = api.verify_token().await.map_err(|e| {
        if matches!(e.status, Some(400 | 401 | 403)) {
            format!("Cloudflare did not accept this token. {}", e.message)
        } else {
            e.message
        }
    })?;
    if t.status != "active" {
        return Err(format!("The token is {}, not active.", t.status));
    }
    Ok(())
}

/// The zone for `domain`: the domain itself, else each parent down to two labels.
async fn find_zone(api: &Client, domain: &str) -> Result<api::Zone, String> {
    let labels: Vec<&str> = domain.split('.').collect();
    for i in 0..labels.len().saturating_sub(1) {
        let candidate = labels[i..].join(".");
        if let Some(z) = api.zone_by_name(&candidate).await.map_err(|e| e.message)? {
            return Ok(z);
        }
    }
    Err(format!(
        "The token cannot see a zone for {domain}. Add the domain to Cloudflare, and give the \
         token access to that zone."
    ))
}

/// The cache rules setup adds, scoped to the komiq hostname so other sites on the
/// zone are untouched. Each makes matching responses cacheable and keeps the
/// server's own `Cache-Control` for both edge and browser (covers, pages and hashed
/// assets are `immutable`; `/img` passes its upstream's lifetime).
fn cache_rules(domain: &str) -> Vec<Value> {
    let suffix = hex::encode(Sha256::digest(domain.as_bytes()));
    let suffix = &suffix[..8];
    let host = format!("http.host eq \"{domain}\"");
    let rule = |r#ref: &str, what: &str, paths: &str| {
        json!({
            "ref": format!("{}_{suffix}", r#ref),
            "description": format!("komiq: cache {what}"),
            "expression": format!("({host} and ({paths}))"),
            "action": "set_cache_settings",
            "action_parameters": {
                "cache": true,
                "edge_ttl": { "mode": "respect_origin" },
                "browser_ttl": { "mode": "respect_origin" }
            },
            "enabled": true
        })
    };
    vec![
        rule(
            "komiq_covers",
            "covers",
            "starts_with(http.request.uri.path, \"/covers/\")",
        ),
        rule(
            "komiq_pages",
            "page images",
            "starts_with(http.request.uri.path, \"/api/v1/manga/\")",
        ),
        rule(
            "komiq_img",
            "proxied images",
            "http.request.uri.path eq \"/img\"",
        ),
        rule(
            "komiq_static",
            "static files",
            "starts_with(http.request.uri.path, \"/_app/immutable/\") or \
             starts_with(http.request.uri.path, \"/admin/_app/immutable/\")",
        ),
    ]
}

/// `https://Read.Example.com/` → `read.example.com`; rejects anything that is not a
/// plain hostname with at least one dot.
/// The public address for direct mode as an origin: `203.0.113.5:8080` and
/// `http://203.0.113.5:8080/` both become `http://203.0.113.5:8080`. Plain HTTP unless
/// the owner wrote `https://` (their own TLS in front). An address of this computer
/// only (loopback) is refused: nobody else can use it.
pub fn normalize_address(raw: &str) -> Result<String, String> {
    const HELP: &str = "Enter this server's public address, like 203.0.113.5:8080.";
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
    let plain = matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none();
    let Some(host) = url.host_str().filter(|_| plain) else {
        return Err(HELP.into());
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let local = match bare.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback() || ip.is_unspecified(),
        Err(_) => bare == "localhost" || bare.ends_with(".localhost"),
    };
    if local {
        return Err("That address only works on this computer. Enter the public one.".into());
    }
    Ok(url.origin().ascii_serialization())
}

pub fn normalize_domain(raw: &str) -> Result<String, String> {
    let mut d = raw.trim().to_ascii_lowercase();
    for p in ["https://", "http://"] {
        if let Some(rest) = d.strip_prefix(p) {
            d = rest.to_string();
        }
    }
    let d = d
        .split('/')
        .next()
        .unwrap_or_default()
        .trim_end_matches('.');
    let ok = !d.is_empty()
        && d.len() <= 253
        && d.contains('.')
        && d.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    if ok {
        Ok(d.to_string())
    } else {
        Err("Enter a domain name like read.example.com.".into())
    }
}

/// A public-IP record must point somewhere Cloudflare can reach.
fn check_public_ip(ip: &str) -> Result<IpAddr, String> {
    let addr: IpAddr = ip
        .parse()
        .map_err(|_| format!("{ip} is not an IP address."))?;
    let private = match addr {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    };
    if private {
        return Err(format!(
            "{ip} is not a public address. Use the server's public address, or a Tunnel."
        ));
    }
    Ok(addr)
}

async fn detect_public_ip(trace_url: &str) -> Result<String, String> {
    let fail = || "Could not detect this server's public address. Enter it and retry.".to_string();
    let body = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| fail())?
        .get(trace_url)
        .send()
        .await
        .map_err(|_| fail())?
        .text()
        .await
        .map_err(|_| fail())?;
    let ip = body
        .lines()
        .find_map(|l| l.strip_prefix("ip="))
        .map(|s| s.trim().to_string())
        .ok_or_else(fail)?;
    check_public_ip(&ip)?;
    Ok(ip)
}

/// Record the wizard's access choice (and the domain an owner set up by hand).
/// Record the access choice. `domain` is the domain for `internet` (optional: set by
/// Cloudflare setup otherwise) and the server's public address for `direct` (required).
pub async fn set_access(pool: &Database, mode: &str, domain: Option<&str>) -> Result<(), String> {
    if !matches!(mode, "local" | "internet" | "direct") {
        return Err("Access must be local, internet or direct.".into());
    }
    let given = domain.map(str::trim).filter(|d| !d.is_empty());
    let store = |s, v: &str| {
        let v = v.to_string();
        async move { instance::store(pool, s, &v).await }
    };
    if mode == "direct" {
        let address = normalize_address(given.unwrap_or_default())?;
        store(&instance::PUBLIC_ADDRESS, &address)
            .await
            .map_err(|_| "Could not save the address.".to_string())?;
        return store(&instance::ACCESS_MODE, mode)
            .await
            .map_err(|_| "Could not save the access choice.".to_string());
    }
    let domain = match given {
        Some(d) => normalize_domain(d)?,
        None => String::new(),
    };
    store(&instance::ACCESS_MODE, mode)
        .await
        .map_err(|_| "Could not save the access choice.".to_string())?;
    if mode == "internet" && !domain.is_empty() {
        store(&instance::PUBLIC_DOMAIN, &domain)
            .await
            .map_err(|_| "Could not save the domain.".to_string())?;
    }
    Ok(())
}

/// Bind briefly unless our listener already holds the address. Unexpected OS errors
/// use `unknown`: never claim a busy port or a permission problem without evidence.
async fn port_state(addr: SocketAddr, serving: bool) -> &'static str {
    if serving {
        return "serving";
    }
    match tokio::net::TcpListener::bind(addr).await {
        Ok(_) => "free",
        Err(e) => port_error_state(e.kind()),
    }
}

fn port_error_state(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::AddrInUse => "in_use",
        std::io::ErrorKind::PermissionDenied => "denied",
        _ => "unknown",
    }
}
