//! A small ACME client (RFC 8555): enough to get one certificate for one name or IP
//! address with the http-01 challenge. It is what gives the "public without a domain"
//! mode HTTPS: Let's Encrypt signs certificates for bare IP addresses (RFC 8738), on its
//! `shortlived` profile only (about six days), so the server renews them by itself
//! (see `direct.rs`).
//!
//! The account key is an ECDSA P-256 key; requests are signed with ES256 (`ring`).
//! Nothing here keeps state between runs: registering the same key again returns the
//! same account.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use ring::digest::{digest, SHA256};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};

/// Let's Encrypt's production directory.
pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// The profile that allows IP addresses at Let's Encrypt. Asked for only when the
/// directory lists it.
const PROFILE: &str = "shortlived";

/// Answers to the CA's http-01 checks: token → key authorization. The plain HTTP
/// listener on port 80 serves them at `/.well-known/acme-challenge/<token>`.
pub type Challenges = Arc<Mutex<HashMap<String, String>>>;

const BAD_NONCE: &str = "urn:ietf:params:acme:error:badNonce";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

struct Reply {
    location: Option<String>,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Result<Value, String> {
        serde_json::from_slice(&self.body)
            .map_err(|_| "The certificate authority sent an answer that is not JSON.".to_string())
    }
}

struct Client<'a> {
    http: &'a reqwest::Client,
    key: EcdsaKeyPair,
    jwk: Value,
    /// RFC 7638 thumbprint of the account key; half of every key authorization.
    thumbprint: String,
    new_nonce: String,
    /// The account URL, once registered.
    kid: Option<String>,
    nonce: Option<String>,
}

fn unreachable_ca(e: reqwest::Error) -> String {
    format!(
        "Could not reach the certificate authority ({}). Check this server's internet \
         connection.",
        e.without_url()
    )
}

/// The `detail` of an ACME problem document, or the HTTP status when there is none.
fn problem(status: reqwest::StatusCode, body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["detail"].as_str().map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {status}"))
}

impl<'a> Client<'a> {
    fn new(
        http: &'a reqwest::Client,
        account_pkcs8: &[u8],
        new_nonce: String,
    ) -> Result<Self, String> {
        let rng = SystemRandom::new();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, account_pkcs8, &rng)
            .map_err(|_| "The stored certificate account key is not usable.".to_string())?;
        // An uncompressed P-256 point: 0x04, then X and Y, 32 bytes each.
        let point = key.public_key().as_ref();
        let (x, y) = (b64(&point[1..33]), b64(&point[33..65]));
        // The thumbprint hashes exactly this spelling: members in order, no spaces.
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        Ok(Self {
            http,
            thumbprint: b64(digest(&SHA256, canonical.as_bytes()).as_ref()),
            jwk: json!({ "crv": "P-256", "kty": "EC", "x": x, "y": y }),
            key,
            new_nonce,
            kid: None,
            nonce: None,
        })
    }

    async fn fresh_nonce(&self) -> Result<String, String> {
        let r = self
            .http
            .head(&self.new_nonce)
            .send()
            .await
            .map_err(unreachable_ca)?;
        replay_nonce(&r).ok_or_else(|| "The certificate authority gave no nonce.".to_string())
    }

    /// A signed POST. `payload: None` is "POST-as-GET" (read a resource).
    async fn post(&mut self, url: &str, payload: Option<&Value>) -> Result<Reply, String> {
        let payload = match payload {
            Some(p) => b64(p.to_string().as_bytes()),
            None => String::new(),
        };
        // One retry: a nonce can go stale between two requests.
        for attempt in 0..2 {
            let nonce = match self.nonce.take() {
                Some(n) => n,
                None => self.fresh_nonce().await?,
            };
            let mut protected = json!({ "alg": "ES256", "nonce": nonce, "url": url });
            match &self.kid {
                Some(kid) => protected["kid"] = json!(kid),
                None => protected["jwk"] = self.jwk.clone(),
            }
            let protected = b64(protected.to_string().as_bytes());
            let signature = self
                .key
                .sign(
                    &SystemRandom::new(),
                    format!("{protected}.{payload}").as_bytes(),
                )
                .map_err(|_| "Could not sign the certificate request.".to_string())?;
            let body = json!({
                "protected": protected,
                "payload": payload,
                "signature": b64(signature.as_ref()),
            });
            let r = self
                .http
                .post(url)
                .header("content-type", "application/jose+json")
                .body(body.to_string())
                .send()
                .await
                .map_err(unreachable_ca)?;
            self.nonce = replay_nonce(&r);
            let status = r.status();
            let location = r
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = r.bytes().await.map_err(unreachable_ca)?.to_vec();
            if status.is_success() {
                return Ok(Reply { location, body });
            }
            let stale = serde_json::from_slice::<Value>(&body)
                .is_ok_and(|v| v["type"].as_str() == Some(BAD_NONCE));
            if !(stale && attempt == 0) {
                return Err(problem(status, &body));
            }
        }
        unreachable!("the loop returns on its second pass")
    }

    /// Read `url` until its `status` is one of `done`, or fail on `invalid`.
    async fn poll(&mut self, url: &str, done: &[&str]) -> Result<Value, String> {
        for _ in 0..60 {
            let v = self.post(url, None).await?.json()?;
            let status = v["status"].as_str().unwrap_or_default();
            if done.contains(&status) {
                return Ok(v);
            }
            if status == "invalid" {
                return Err(invalid_reason(&v));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err("The certificate authority took too long to answer.".into())
    }
}

fn replay_nonce(r: &reqwest::Response) -> Option<String> {
    r.headers()
        .get("replay-nonce")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Why an authorization or an order became invalid, in the CA's words.
fn invalid_reason(v: &Value) -> String {
    let from_challenge = v["challenges"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|c| c["error"]["detail"].as_str());
    from_challenge
        .or_else(|| v["error"]["detail"].as_str())
        .unwrap_or("The certificate authority refused the request.")
        .to_string()
}

fn str_field<'v>(v: &'v Value, name: &str) -> Result<&'v str, String> {
    v[name]
        .as_str()
        .ok_or_else(|| format!("The certificate authority's answer has no \"{name}\"."))
}

/// Get a certificate for `host` (an IP address or a DNS name) and return its PEM chain.
///
/// `csr` is the DER certificate request for the key the certificate is for. While this
/// runs, the CA fetches `http://<host>/.well-known/acme-challenge/<token>` on port 80;
/// the answers are put in `challenges` for the listener that serves them. The caller
/// empties `challenges` afterwards.
pub async fn issue(
    http: &reqwest::Client,
    directory: &str,
    account_pkcs8: &[u8],
    host: &str,
    csr: &[u8],
    challenges: &Challenges,
) -> Result<String, String> {
    let dir: Value = http
        .get(directory)
        .send()
        .await
        .map_err(unreachable_ca)?
        .json()
        .await
        .map_err(|_| "The certificate authority's directory is not readable.".to_string())?;
    let mut acme = Client::new(
        http,
        account_pkcs8,
        str_field(&dir, "newNonce")?.to_string(),
    )?;

    let account = acme
        .post(
            str_field(&dir, "newAccount")?,
            Some(&json!({ "termsOfServiceAgreed": true })),
        )
        .await?;
    acme.kid = Some(
        account
            .location
            .ok_or("The certificate authority gave no account address.")?,
    );

    let kind = if host.parse::<IpAddr>().is_ok() {
        "ip"
    } else {
        "dns"
    };
    let mut order = json!({ "identifiers": [{ "type": kind, "value": host }] });
    if dir["meta"]["profiles"][PROFILE].is_string() {
        order["profile"] = json!(PROFILE);
    }
    let created = acme
        .post(str_field(&dir, "newOrder")?, Some(&order))
        .await?;
    let order_url = created
        .location
        .clone()
        .ok_or("The certificate authority gave no order address.")?;
    let order = created.json()?;

    authorize(&mut acme, &order, challenges).await?;

    acme.poll(&order_url, &["ready", "valid"]).await?;
    acme.post(
        str_field(&order, "finalize")?,
        Some(&json!({ "csr": b64(csr) })),
    )
    .await?;
    let order = acme.poll(&order_url, &["valid"]).await?;
    let chain = acme.post(str_field(&order, "certificate")?, None).await?;
    String::from_utf8(chain.body)
        .map_err(|_| "The certificate authority sent a certificate that is not text.".to_string())
}

/// Answer the http-01 challenge of every authorization in `order`.
async fn authorize(
    acme: &mut Client<'_>,
    order: &Value,
    challenges: &Challenges,
) -> Result<(), String> {
    let urls: Vec<String> = order["authorizations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u.as_str().map(str::to_string))
        .collect();
    for url in urls {
        let authz = acme.post(&url, None).await?.json()?;
        if authz["status"].as_str() == Some("valid") {
            continue;
        }
        let challenge = authz["challenges"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["type"].as_str() == Some("http-01"))
            .ok_or("The certificate authority offers no check over HTTP for this address.")?;
        let token = str_field(challenge, "token")?.to_string();
        let answer = format!("{token}.{}", acme.thumbprint);
        challenges
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(token, answer);
        acme.post(str_field(challenge, "url")?, Some(&json!({})))
            .await?;
        acme.poll(&url, &["valid"]).await?;
    }
    Ok(())
}
