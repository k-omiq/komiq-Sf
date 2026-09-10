//! Typed client for the parts of the Cloudflare v4 API that setup uses.
//!
//! Every call is one of the endpoints listed in `docs/self-host-cloudflare.md`. The
//! base URL is injectable so tests (and a manual end-to-end run) talk to a local mock;
//! nothing here ever reaches Cloudflare unless an admin starts setup with a real token.
//!
//! The API token lives in [`ApiToken`], whose `Debug` is redacted, and is only ever
//! sent as the `Authorization` header. Error messages are built from Cloudflare's own
//! `errors[]` text and the HTTP status, never from the request, so a failure message
//! cannot echo the token back into a log or the admin UI.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

/// A Cloudflare API token, held for one setup or remove run and then dropped.
#[derive(Clone)]
pub struct ApiToken(String);

impl ApiToken {
    pub fn new(raw: &str) -> Option<Self> {
        let t = raw.trim();
        (!t.is_empty()).then(|| Self(t.to_string()))
    }
}

impl std::fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiToken(<redacted>)")
    }
}

/// A failed API call: Cloudflare's own message (or a transport error) plus the HTTP
/// status, so callers can treat a 404 on delete as "already gone".
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl ApiError {
    pub fn is_not_found(&self) -> bool {
        self.status == Some(404)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// The v4 envelope: `{ success, errors: [{code, message}], result }`.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    errors: Vec<EnvelopeError>,
    #[serde(default)]
    result: Value,
}

#[derive(Deserialize)]
struct EnvelopeError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenStatus {
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Zone {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub status: String,
    pub account: ZoneAccount,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZoneAccount {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tunnel {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DnsRecord {
    pub id: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OriginCertificate {
    pub id: String,
    pub certificate: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ruleset {
    pub id: String,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub id: String,
    #[serde(default, rename = "ref")]
    pub reference: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CloudflareIps {
    #[serde(default)]
    pub ipv4_cidrs: Vec<String>,
    #[serde(default)]
    pub ipv6_cidrs: Vec<String>,
}

/// Cache-rule phase that setup writes to (Rulesets API).
pub const CACHE_PHASE: &str = "http_request_cache_settings";

pub struct Client {
    http: reqwest::Client,
    base: String,
    token: ApiToken,
}

impl Client {
    pub fn new(base: &str, token: ApiToken) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("komika-server/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client builds with static config");
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token,
        }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> ApiResult<T> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.http.request(method, &url).bearer_auth(&self.token.0);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.map_err(|e| ApiError {
            status: None,
            // `without_url`: the URL holds no secret, but the message stays short.
            message: format!("Could not reach Cloudflare: {}", e.without_url()),
        })?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let env: Envelope = serde_json::from_str(&text).map_err(|_| ApiError {
            status: Some(status),
            message: format!("Cloudflare answered HTTP {status} with an unreadable body."),
        })?;
        if !env.success || !(200..300).contains(&status) {
            let detail = env
                .errors
                .iter()
                .map(|e| format!("{} (code {})", e.message, e.code))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ApiError {
                status: Some(status),
                message: if detail.is_empty() {
                    format!("Cloudflare refused the request (HTTP {status}).")
                } else {
                    format!("Cloudflare: {detail}")
                },
            });
        }
        serde_json::from_value(env.result).map_err(|e| ApiError {
            status: Some(status),
            message: format!("Cloudflare returned an unexpected result: {e}"),
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        self.call(reqwest::Method::GET, path, None).await
    }

    // ---- token + zone ------------------------------------------------------

    /// `GET /user/tokens/verify`.
    pub async fn verify_token(&self) -> ApiResult<TokenStatus> {
        self.get("/user/tokens/verify").await
    }

    /// `GET /zones?name=<name>`: the zone with exactly this name, if the token sees it.
    pub async fn zone_by_name(&self, name: &str) -> ApiResult<Option<Zone>> {
        let zones: Vec<Zone> = self.get(&format!("/zones?name={}", enc(name))).await?;
        Ok(zones
            .into_iter()
            .find(|z| z.name.eq_ignore_ascii_case(name)))
    }

    // ---- tunnel ------------------------------------------------------------

    /// `POST /accounts/{account}/cfd_tunnel` (remotely managed: `config_src: cloudflare`).
    pub async fn create_tunnel(&self, account: &str, name: &str) -> ApiResult<Tunnel> {
        self.call(
            reqwest::Method::POST,
            &format!("/accounts/{account}/cfd_tunnel"),
            Some(json!({ "name": name, "config_src": "cloudflare" })),
        )
        .await
    }

    /// `PUT /accounts/{account}/cfd_tunnel/{id}/configurations`: route `hostname` to
    /// `service`, everything else 404 (the API requires a catch-all last rule).
    pub async fn put_tunnel_ingress(
        &self,
        account: &str,
        tunnel: &str,
        hostname: &str,
        service: &str,
    ) -> ApiResult<Value> {
        self.call(
            reqwest::Method::PUT,
            &format!("/accounts/{account}/cfd_tunnel/{tunnel}/configurations"),
            Some(json!({
                "config": {
                    "ingress": [
                        { "hostname": hostname, "service": service, "originRequest": {} },
                        { "service": "http_status:404" }
                    ]
                }
            })),
        )
        .await
    }

    /// `GET /accounts/{account}/cfd_tunnel/{id}/token`: the connector's run token.
    pub async fn tunnel_token(&self, account: &str, tunnel: &str) -> ApiResult<String> {
        self.get(&format!("/accounts/{account}/cfd_tunnel/{tunnel}/token"))
            .await
    }

    /// `DELETE /accounts/{account}/cfd_tunnel/{id}/connections`, then
    /// `DELETE /accounts/{account}/cfd_tunnel/{id}`. A tunnel with live connections
    /// cannot be deleted, and Cloudflare keeps a stopped connector's connections for a
    /// while, so they are cleared first.
    pub async fn delete_tunnel(&self, account: &str, tunnel: &str) -> ApiResult<()> {
        match self
            .call::<Value>(
                reqwest::Method::DELETE,
                &format!("/accounts/{account}/cfd_tunnel/{tunnel}/connections"),
                None,
            )
            .await
        {
            Ok(_) => {}
            Err(e) if e.is_not_found() => return Ok(()),
            Err(e) => return Err(e),
        }
        self.call::<Value>(
            reqwest::Method::DELETE,
            &format!("/accounts/{account}/cfd_tunnel/{tunnel}"),
            None,
        )
        .await
        .map(|_| ())
    }

    // ---- DNS ---------------------------------------------------------------

    /// `GET /zones/{zone}/dns_records?name.exact=<name>`.
    pub async fn dns_records_named(&self, zone: &str, name: &str) -> ApiResult<Vec<DnsRecord>> {
        self.get(&format!(
            "/zones/{zone}/dns_records?name.exact={}",
            enc(name)
        ))
        .await
    }

    /// `POST /zones/{zone}/dns_records`: a proxied record (orange cloud).
    pub async fn create_dns_record(
        &self,
        zone: &str,
        kind: &str,
        name: &str,
        content: &str,
    ) -> ApiResult<DnsRecord> {
        self.call(
            reqwest::Method::POST,
            &format!("/zones/{zone}/dns_records"),
            Some(json!({
                "type": kind,
                "name": name,
                "content": content,
                "proxied": true,
                "ttl": 1,
                "comment": "Created by komiq setup"
            })),
        )
        .await
    }

    /// `DELETE /zones/{zone}/dns_records/{id}`.
    pub async fn delete_dns_record(&self, zone: &str, id: &str) -> ApiResult<()> {
        self.call::<Value>(
            reqwest::Method::DELETE,
            &format!("/zones/{zone}/dns_records/{id}"),
            None,
        )
        .await
        .map(|_| ())
    }

    // ---- origin certificate + zone settings --------------------------------

    /// `POST /certificates` (Origin CA, ECDSA, 15 years).
    pub async fn create_origin_certificate(
        &self,
        csr_pem: &str,
        hostname: &str,
    ) -> ApiResult<OriginCertificate> {
        self.call(
            reqwest::Method::POST,
            "/certificates",
            Some(json!({
                "csr": csr_pem,
                "hostnames": [hostname],
                "request_type": "origin-ecc",
                "requested_validity": 5475
            })),
        )
        .await
    }

    /// `DELETE /certificates/{id}` (revoke).
    pub async fn revoke_origin_certificate(&self, id: &str) -> ApiResult<()> {
        self.call::<Value>(
            reqwest::Method::DELETE,
            &format!("/certificates/{id}"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// `GET /zones/{zone}/settings/{setting}` → its `value`.
    pub async fn zone_setting(&self, zone: &str, setting: &str) -> ApiResult<Value> {
        let r: Value = self
            .get(&format!("/zones/{zone}/settings/{setting}"))
            .await?;
        Ok(r.get("value").cloned().unwrap_or(Value::Null))
    }

    /// `PATCH /zones/{zone}/settings/{setting}` with `{ value }`.
    pub async fn set_zone_setting(&self, zone: &str, setting: &str, value: Value) -> ApiResult<()> {
        self.call::<Value>(
            reqwest::Method::PATCH,
            &format!("/zones/{zone}/settings/{setting}"),
            Some(json!({ "value": value })),
        )
        .await
        .map(|_| ())
    }

    /// `GET /ips`: Cloudflare's edge ranges, for trusting `CF-Connecting-IP`.
    pub async fn cloudflare_ips(&self) -> ApiResult<CloudflareIps> {
        self.get("/ips").await
    }

    // ---- cache rules (Rulesets API) ----------------------------------------

    /// `GET /zones/{zone}/rulesets/phases/{phase}/entrypoint`; `None` when the zone has
    /// no cache rules yet (404).
    pub async fn cache_entrypoint(&self, zone: &str) -> ApiResult<Option<Ruleset>> {
        match self
            .get(&format!(
                "/zones/{zone}/rulesets/phases/{CACHE_PHASE}/entrypoint"
            ))
            .await
        {
            Ok(r) => Ok(Some(r)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `POST /zones/{zone}/rulesets`: create the phase entrypoint with `rules`.
    pub async fn create_cache_ruleset(&self, zone: &str, rules: Vec<Value>) -> ApiResult<Ruleset> {
        self.call(
            reqwest::Method::POST,
            &format!("/zones/{zone}/rulesets"),
            Some(json!({
                "name": "Cache rules",
                "kind": "zone",
                "phase": CACHE_PHASE,
                "rules": rules
            })),
        )
        .await
    }

    /// `POST /zones/{zone}/rulesets/{ruleset}/rules`: append one rule; returns the
    /// whole ruleset.
    pub async fn add_rule(&self, zone: &str, ruleset: &str, rule: Value) -> ApiResult<Ruleset> {
        self.call(
            reqwest::Method::POST,
            &format!("/zones/{zone}/rulesets/{ruleset}/rules"),
            Some(rule),
        )
        .await
    }

    /// `GET /zones/{zone}/rulesets/{ruleset}`.
    pub async fn ruleset(&self, zone: &str, ruleset: &str) -> ApiResult<Ruleset> {
        self.get(&format!("/zones/{zone}/rulesets/{ruleset}")).await
    }

    /// `DELETE /zones/{zone}/rulesets/{ruleset}/rules/{rule}`.
    pub async fn delete_rule(&self, zone: &str, ruleset: &str, rule: &str) -> ApiResult<()> {
        self.call::<Value>(
            reqwest::Method::DELETE,
            &format!("/zones/{zone}/rulesets/{ruleset}/rules/{rule}"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// `DELETE /zones/{zone}/rulesets/{ruleset}`. Cloudflare answers 204 with no body.
    pub async fn delete_ruleset(&self, zone: &str, ruleset: &str) -> ApiResult<()> {
        let url = format!("{}/zones/{zone}/rulesets/{ruleset}", self.base);
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.token.0)
            .send()
            .await
            .map_err(|e| ApiError {
                status: None,
                message: format!("Could not reach Cloudflare: {}", e.without_url()),
            })?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(ApiError {
            status: Some(status),
            message: format!("Cloudflare refused to delete the cache ruleset (HTTP {status})."),
        })
    }
}

/// Percent-encode a query value (domains are ASCII, but stay safe).
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
