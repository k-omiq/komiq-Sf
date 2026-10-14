//! Cloudflare setup against a local mock of the v4 API. Nothing here reaches
//! Cloudflare: every client is pointed at the mock below, and the connector is a
//! shell script standing in for `cloudflared`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::database::TestPoolOptions;
use crate::db::Database;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use serde_json::{json, Value};

use super::origin_tls::{der_bit_string, der_ecdsa_sha256, der_name, der_san_extension, pem};
use super::*;

mod direct;

const TOKEN: &str = "test-api-token-0123456789";
const RUN_TOKEN: &str = "test-tunnel-run-token-abcdef";

// ---- the mock ----------------------------------------------------------------

#[derive(Default)]
struct Mock {
    tunnels: HashMap<String, Value>,
    deleted_tunnels: Vec<String>,
    dns: HashMap<String, Value>,
    certs: HashMap<String, String>,
    revoked: Vec<String>,
    settings: HashMap<String, Value>,
    ruleset: Option<(String, Vec<Value>)>,
    ruleset_deleted: bool,
    /// Operation name → how many more calls fail.
    fail: HashMap<String, u32>,
    /// Operation name → calls let through before `fail` applies.
    fail_skip: HashMap<String, u32>,
    /// Operation name → its calls are on record (`calls`) and wait here for the test.
    gate: HashMap<String, Arc<tokio::sync::Notify>>,
    calls: Vec<String>,
    next_id: u32,
    ca: Option<Arc<TestCa>>,
    public_ip: Option<String>,
}

impl Mock {
    fn id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{:04}", self.next_id)
    }
}

type Shared = Arc<Mutex<Mock>>;

fn ok(result: Value) -> Response {
    (
        StatusCode::OK,
        axum::Json(json!({ "success": true, "errors": [], "messages": [], "result": result })),
    )
        .into_response()
}

fn err(status: StatusCode, code: i64, message: &str) -> Response {
    (
        status,
        axum::Json(json!({ "success": false, "errors": [{ "code": code, "message": message }], "messages": [], "result": null })),
    )
        .into_response()
}

async fn handle(
    State(m): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let op = match (&method, seg.as_slice()) {
        (&Method::GET, ["health"]) => "health",
        (&Method::GET, ["cdn-cgi", "trace"]) => "trace",
        (&Method::GET, ["user", "tokens", "verify"]) => "verify",
        (&Method::GET, ["zones"]) => "zones",
        (&Method::GET, ["ips"]) => "ips",
        (&Method::POST, ["accounts", _, "cfd_tunnel"]) => "create_tunnel",
        (&Method::PUT, ["accounts", _, "cfd_tunnel", _, "configurations"]) => "tunnel_config",
        (&Method::GET, ["accounts", _, "cfd_tunnel", _, "token"]) => "tunnel_token",
        (&Method::DELETE, ["accounts", _, "cfd_tunnel", _, "connections"]) => "tunnel_connections",
        (&Method::DELETE, ["accounts", _, "cfd_tunnel", _]) => "delete_tunnel",
        (&Method::GET, ["zones", _, "dns_records"]) => "dns_list",
        (&Method::POST, ["zones", _, "dns_records"]) => "dns_create",
        (&Method::PATCH, ["zones", _, "dns_records", _]) => "dns_update",
        (&Method::DELETE, ["zones", _, "dns_records", _]) => "dns_delete",
        (&Method::POST, ["certificates"]) => "cert_create",
        (&Method::DELETE, ["certificates", _]) => "cert_revoke",
        (&Method::GET, ["zones", _, "settings", _]) => "settings_get",
        (&Method::PATCH, ["zones", _, "settings", _]) => "settings_patch",
        (&Method::GET, ["zones", _, "rulesets", "phases", _, "entrypoint"]) => "ruleset_entry",
        (&Method::POST, ["zones", _, "rulesets"]) => "ruleset_create",
        (&Method::POST, ["zones", _, "rulesets", _, "rules"]) => "rule_add",
        (&Method::GET, ["zones", _, "rulesets", _]) => "ruleset_get",
        (&Method::DELETE, ["zones", _, "rulesets", _, "rules", _]) => "rule_delete",
        (&Method::DELETE, ["zones", _, "rulesets", _]) => "ruleset_delete",
        _ => return err(StatusCode::NOT_FOUND, 7003, "No route for that URI"),
    };
    let gate = {
        let mut m = m.lock().unwrap();
        m.calls.push(op.to_string());
        m.gate.get(op).cloned()
    };
    if let Some(gate) = gate {
        gate.notified().await;
    }
    let mut m = m.lock().unwrap();
    let skip = m.fail_skip.get_mut(op).filter(|n| **n > 0).map(|n| {
        *n -= 1;
    });
    if let Some(n) = m.fail.get_mut(op).filter(|n| **n > 0 && skip.is_none()) {
        *n -= 1;
        if op == "health" {
            return (StatusCode::SERVICE_UNAVAILABLE, "down").into_response();
        }
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            10000,
            &format!("injected {op} failure"),
        );
    }
    match op {
        "health" => return (StatusCode::OK, "ok").into_response(),
        "trace" => {
            return (
                StatusCode::OK,
                format!(
                    "fl=1\nip={}\nts=1\n",
                    m.public_ip.as_deref().unwrap_or("203.0.113.7")
                ),
            )
                .into_response()
        }
        _ => {}
    }
    let authed = headers.get("authorization").and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {TOKEN}"));
    if !authed {
        return err(StatusCode::UNAUTHORIZED, 1000, "Invalid API Token");
    }
    match op {
        "verify" => ok(json!({ "id": "tok", "status": "active" })),
        "zones" => {
            let name = query.strip_prefix("name=").unwrap_or_default();
            if name == "example.com" {
                ok(
                    json!([{ "id": "zone1", "name": "example.com", "status": "active",
                    "account": { "id": "acc1", "name": "Test account" } }]),
                )
            } else {
                ok(json!([]))
            }
        }
        "ips" => ok(
            json!({ "ipv4_cidrs": ["173.245.48.0/20", "103.21.244.0/22"],
            "ipv6_cidrs": ["2400:cb00::/32"], "etag": "x" }),
        ),
        "create_tunnel" => {
            assert_eq!(body["config_src"], "cloudflare");
            let id = m.id("tun-");
            m.tunnels
                .insert(id.clone(), json!({ "name": body["name"], "config": null }));
            ok(json!({ "id": id, "name": body["name"] }))
        }
        "tunnel_config" => {
            let id = seg[3].to_string();
            let Some(t) = m.tunnels.get_mut(&id) else {
                return err(StatusCode::NOT_FOUND, 1003, "tunnel not found");
            };
            t["config"] = body["config"].clone();
            ok(json!({ "tunnel_id": id }))
        }
        "tunnel_token" => {
            if !m.tunnels.contains_key(seg[3]) {
                return err(StatusCode::NOT_FOUND, 1003, "tunnel not found");
            }
            ok(json!(RUN_TOKEN))
        }
        "tunnel_connections" => {
            if !m.tunnels.contains_key(seg[3]) {
                return err(StatusCode::NOT_FOUND, 1003, "tunnel not found");
            }
            ok(json!({}))
        }
        "delete_tunnel" => match m.tunnels.remove(seg[3]) {
            Some(_) => {
                m.deleted_tunnels.push(seg[3].to_string());
                ok(json!({ "id": seg[3] }))
            }
            None => err(StatusCode::NOT_FOUND, 1003, "tunnel not found"),
        },
        "dns_list" => {
            let name = query.strip_prefix("name.exact=").unwrap_or_default();
            let list: Vec<Value> = m
                .dns
                .iter()
                .filter(|(_, r)| r["name"] == name)
                .map(|(id, r)| {
                    let mut r = r.clone();
                    r["id"] = json!(id);
                    r
                })
                .collect();
            ok(json!(list))
        }
        "dns_create" => {
            assert_eq!(body["proxied"], true, "records must be proxied");
            let id = m.id("dns-");
            m.dns.insert(id.clone(), body.clone());
            let mut r = body;
            r["id"] = json!(id);
            ok(r)
        }
        "dns_update" => {
            let Some(r) = m.dns.get_mut(seg[3]) else {
                return err(StatusCode::NOT_FOUND, 81044, "Record does not exist.");
            };
            for (key, value) in body.as_object().unwrap() {
                r[key] = value.clone();
            }
            let mut r = r.clone();
            r["id"] = json!(seg[3]);
            ok(r)
        }
        "dns_delete" => match m.dns.remove(seg[3]) {
            Some(_) => ok(json!({ "id": seg[3] })),
            None => err(StatusCode::NOT_FOUND, 81044, "Record does not exist."),
        },
        "cert_create" => {
            assert_eq!(body["request_type"], "origin-ecc");
            let ca = m.ca.clone().expect("mock CA");
            let host = body["hostnames"][0].as_str().unwrap().to_string();
            let cert = ca.sign_csr(body["csr"].as_str().unwrap(), &host);
            let id = m.id("cert-");
            m.certs.insert(id.clone(), cert.clone());
            ok(json!({ "id": id, "certificate": cert, "hostnames": [host] }))
        }
        "cert_revoke" => match m.certs.remove(seg[1]) {
            Some(_) => {
                m.revoked.push(seg[1].to_string());
                ok(json!({ "id": seg[1] }))
            }
            None => err(StatusCode::NOT_FOUND, 1001, "certificate not found"),
        },
        "settings_get" => {
            let v = m.settings.get(seg[3]).cloned().unwrap_or(Value::Null);
            ok(json!({ "id": seg[3], "value": v }))
        }
        "settings_patch" => {
            m.settings.insert(seg[3].to_string(), body["value"].clone());
            ok(json!({ "id": seg[3], "value": body["value"] }))
        }
        "ruleset_entry" => match &m.ruleset {
            Some((id, rules)) => ok(json!({ "id": id, "rules": rules })),
            None => err(
                StatusCode::NOT_FOUND,
                10003,
                "could not find entrypoint ruleset",
            ),
        },
        "ruleset_create" => {
            assert_eq!(body["phase"], "http_request_cache_settings");
            let id = m.id("rs-");
            let mut rules = vec![];
            for r in body["rules"].as_array().unwrap() {
                let mut r = r.clone();
                r["id"] = json!(m.id("rule-"));
                rules.push(r);
            }
            m.ruleset = Some((id.clone(), rules.clone()));
            ok(json!({ "id": id, "rules": rules }))
        }
        "rule_add" => {
            let rid = m.id("rule-");
            let Some((id, rules)) = m.ruleset.as_mut() else {
                return err(StatusCode::NOT_FOUND, 10003, "no ruleset");
            };
            let mut r = body.clone();
            r["id"] = json!(rid);
            rules.push(r);
            ok(json!({ "id": id, "rules": rules }))
        }
        "ruleset_get" => match &m.ruleset {
            Some((id, rules)) if id == seg[3] => ok(json!({ "id": id, "rules": rules })),
            _ => err(StatusCode::NOT_FOUND, 10003, "no ruleset"),
        },
        "rule_delete" => {
            let Some((id, rules)) = m.ruleset.as_mut() else {
                return err(StatusCode::NOT_FOUND, 10003, "no ruleset");
            };
            let before = rules.len();
            rules.retain(|r| r["id"] != seg[5]);
            if rules.len() == before {
                return err(StatusCode::NOT_FOUND, 10003, "no rule");
            }
            let out = json!({ "id": id, "rules": rules });
            ok(out)
        }
        "ruleset_delete" => {
            m.ruleset = None;
            m.ruleset_deleted = true;
            StatusCode::NO_CONTENT.into_response()
        }
        _ => unreachable!(),
    }
}

async fn start_mock(mock: Mock) -> (String, Shared) {
    let shared: Shared = Arc::new(Mutex::new(mock));
    let app = Router::new().fallback(handle).with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), shared)
}

fn base_mock() -> Mock {
    let mut m = Mock::default();
    m.settings.insert("ssl".into(), json!("flexible"));
    m.settings.insert("browser_cache_ttl".into(), json!(14400));
    // Someone else's record and cache rule on the same zone: setup and remove must
    // leave both alone.
    m.dns.insert(
        "dns-other".into(),
        json!({ "type": "A", "name": "blog.example.com", "content": "198.51.100.1", "proxied": true }),
    );
    m.ruleset = Some((
        "rs-existing".into(),
        vec![json!({ "id": "rule-theirs", "ref": "their_rule", "expression": "true" })],
    ));
    m
}

// ---- a tiny CA so the mock can answer `POST /certificates` --------------------

struct TestCa {
    key: EcdsaKeyPair,
    cert_der: Vec<u8>,
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x100 {
        out.extend([0x81, len as u8]);
    } else {
        out.extend([0x82, (len >> 8) as u8, len as u8]);
    }
    out.extend_from_slice(content);
    out
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

/// (content, rest) of the TLV at the start of `b`.
fn read(b: &[u8]) -> (&[u8], &[u8]) {
    let (len, hdr) = if b[1] < 0x80 {
        (b[1] as usize, 2)
    } else {
        let n = (b[1] & 0x7F) as usize;
        (
            b[2..2 + n]
                .iter()
                .fold(0usize, |a, x| (a << 8) | *x as usize),
            2 + n,
        )
    };
    (&b[hdr..hdr + len], &b[hdr + len..])
}

/// (whole TLV, rest).
fn read_raw(b: &[u8]) -> (&[u8], &[u8]) {
    let (_, rest) = read(b);
    b.split_at(b.len() - rest.len())
}

impl TestCa {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let spki = super::origin_tls::der_spki(key.public_key().as_ref());
        // basicConstraints: critical, CA true.
        let bc = seq(&[
            &tlv(0x06, &[0x55, 0x1D, 0x13]),
            &[0x01, 0x01, 0xFF],
            &tlv(0x04, &seq(&[&[0x01, 0x01, 0xFF]])),
        ]);
        let name = der_name("komiq test CA");
        let cert_der = Self::sign(&key, &[0x01], &name, &name, &spki, &bc);
        Self { key, cert_der }
    }

    fn sign(
        key: &EcdsaKeyPair,
        serial: &[u8],
        issuer: &[u8],
        subject: &[u8],
        spki: &[u8],
        extension: &[u8],
    ) -> Vec<u8> {
        let dates = ("200101000000Z", "491231235959Z");
        Self::sign_dated(key, serial, issuer, subject, spki, extension, dates)
    }

    /// `dates`: notBefore and notAfter as UTCTime (`YYMMDDHHMMSSZ`).
    fn sign_dated(
        key: &EcdsaKeyPair,
        serial: &[u8],
        issuer: &[u8],
        subject: &[u8],
        spki: &[u8],
        extension: &[u8],
        dates: (&str, &str),
    ) -> Vec<u8> {
        let validity = seq(&[
            &tlv(0x17, dates.0.as_bytes()),
            &tlv(0x17, dates.1.as_bytes()),
        ]);
        let tbs = seq(&[
            &tlv(0xA0, &tlv(0x02, &[0x02])),
            &tlv(0x02, serial),
            &der_ecdsa_sha256(),
            issuer,
            &validity,
            subject,
            spki,
            &tlv(0xA3, &seq(&[extension])),
        ]);
        let sig = key.sign(&SystemRandom::new(), &tbs).unwrap();
        seq(&[&tbs, &der_ecdsa_sha256(), &der_bit_string(sig.as_ref())])
    }

    /// Issue a leaf for the public key inside `csr_pem` (what Origin CA does).
    fn sign_csr(&self, csr_pem: &str, host: &str) -> String {
        use base64::Engine;
        let b64: String = csr_pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let (outer, _) = read(&der);
        let (info, _) = read(outer);
        let (_version, rest) = read_raw(info);
        let (_subject, rest) = read_raw(rest);
        let (spki, _) = read_raw(rest);
        let (ca_subject_seq, _) = read(&self.cert_der);
        let _ = ca_subject_seq;
        let leaf = Self::sign(
            &self.key,
            &[0x02],
            &der_name("komiq test CA"),
            &der_name(host),
            spki,
            &der_san_extension(host),
        );
        pem("CERTIFICATE", &leaf)
    }

    fn pem(&self) -> String {
        pem("CERTIFICATE", &self.cert_der)
    }

    /// A certificate for the key in `pkcs8` (the one on disk), with the subject
    /// alternative name and the dates (days from now) a test needs.
    fn sign_for_key(&self, pkcs8: &[u8], san: &[u8], days: (i64, i64)) -> String {
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8, &SystemRandom::new())
                .unwrap();
        let leaf = Self::sign_dated(
            &self.key,
            &[0x7F],
            &der_name("komiq test CA"),
            &seq(&[]),
            &super::origin_tls::der_spki(key.public_key().as_ref()),
            san,
            (&utc_time(days.0), &utc_time(days.1)),
        );
        pem("CERTIFICATE", &leaf)
    }
}

fn utc_time(days_from_now: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::days(days_from_now))
        .format("%y%m%d%H%M%SZ")
        .to_string()
}

// ---- harness -----------------------------------------------------------------

async fn pool() -> Database {
    let pool = TestPoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations_pg")
        .run(pool.write())
        .await
        .unwrap();
    pool
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "komika-cf-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A stand-in for `cloudflared`: records what it was given, reports a connection,
/// and idles until killed.
fn fake_cloudflared(dir: &std::path::Path) -> String {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("cloudflared");
    std::fs::write(
        &p,
        "#!/bin/sh\n\
         d=$(dirname \"$0\")\n\
         printf '%s' \"$TUNNEL_TOKEN\" > \"$d/seen-token\"\n\
         printf '%s' \"$*\" > \"$d/seen-args\"\n\
         echo 'INF Starting tunnel' >&2\n\
         echo 'INF Registered tunnel connection connIndex=0' >&2\n\
         exec sleep 600\n",
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.to_string_lossy().into_owned()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn options(api: &str, dir: &std::path::Path, bin: Option<String>) -> Options {
    options_with_ports(api, dir, bin, (free_port(), free_port()))
}

fn options_with_ports(
    api: &str,
    dir: &std::path::Path,
    bin: Option<String>,
    ports: (u16, u16),
) -> Options {
    Options {
        api_base: api.to_string(),
        download_base: api.to_string(),
        trace_url: format!("{api}/cdn-cgi/trace"),
        health_base: Some(api.to_string()),
        cloudflared_bin: bin,
        // Never the machine's own cloudflared: a test must not find or run it.
        search_path: Default::default(),
        data_dir: dir.join("data"),
        origin_service: "http://127.0.0.1:18080".into(),
        listen: SocketAddr::from(([127, 0, 0, 1], 18080)),
        published_port: 18080,
        https_addr: SocketAddr::from(([127, 0, 0, 1], ports.0)),
        acme_addr: SocketAddr::from(([127, 0, 0, 1], ports.1)),
        // Never Let's Encrypt from a test: a closed port unless a test sets a mock.
        acme_directory: "http://127.0.0.1:9/directory".into(),
        acme_ca_file: None,
        renew_check: Duration::from_millis(50),
        https_retry: Duration::from_millis(20),
        health_attempts: 2,
        health_delay: Duration::from_millis(10),
        connector_timeout: Duration::from_secs(5),
    }
}

fn manager(pool: &Database, opts: Options, trusted: Arc<crate::TrustedProxies>) -> Arc<Manager> {
    let m = Arc::new(Manager::new(pool.clone(), opts, trusted));
    m.set_router(Router::new().route("/health", axum::routing::get(|| async { "ok" })));
    m
}

fn input(domain: &str, connection: Connection) -> CloudflareSetupInput {
    CloudflareSetupInput {
        token: TOKEN.into(),
        domain: domain.into(),
        connection,
        public_ip: None,
    }
}

async fn wait_job(m: &Manager) -> CloudflareJob {
    for _ in 0..400 {
        if let Some(j) = m.status().await.job {
            if !j.running {
                return j;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("job did not finish");
}

fn states(j: &CloudflareJob) -> Vec<(String, String)> {
    j.steps
        .iter()
        .map(|s| (s.id.clone(), s.state.clone()))
        .collect()
}

fn all_done(j: &CloudflareJob) {
    assert!(
        !j.failed && j.steps.iter().all(|s| s.state == "done"),
        "not all done: {:?}",
        j.steps
            .iter()
            .map(|s| (s.id.as_str(), s.state.as_str(), s.message.as_deref()))
            .collect::<Vec<_>>()
    );
}

/// Nothing setup made is left in the mock; what was there before is intact.
fn assert_clean(m: &Mock, ruleset_existed: bool) {
    assert!(m.tunnels.is_empty(), "tunnels left: {:?}", m.tunnels);
    assert_eq!(m.dns.keys().collect::<Vec<_>>(), vec!["dns-other"]);
    assert!(m.certs.is_empty(), "certificates left");
    if ruleset_existed {
        let (id, rules) = m.ruleset.as_ref().expect("their ruleset kept");
        assert_eq!(id, "rs-existing");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["ref"], "their_rule");
    } else {
        assert!(m.ruleset.is_none());
    }
    assert_eq!(m.settings["ssl"], json!("flexible"));
    assert_eq!(m.settings["browser_cache_ttl"], json!(14400));
}

fn mode(p: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

// ---- tests -------------------------------------------------------------------

#[tokio::test]
async fn completed_remove_save_failure_is_reported_and_preserves_the_checkpoint() {
    let pool = pool().await;
    let state = Stored {
        domain: "read.example.com".into(),
        ..Default::default()
    };
    save_stored(&pool, Some(&state)).await.unwrap();
    instance::store(&pool, &instance::PUBLIC_DOMAIN, &state.domain)
        .await
        .unwrap();
    instance::store(&pool, &instance::ACCESS_MODE, "internet")
        .await
        .unwrap();
    sqlx::raw_sql("CREATE FUNCTION reject_final_domain() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.key='public_domain' AND NEW.value='' THEN RAISE EXCEPTION 'save unavailable'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_final_domain BEFORE UPDATE ON instance_setting FOR EACH ROW EXECUTE FUNCTION reject_final_domain()")
        .execute(&pool).await.unwrap();
    let dir = temp_dir("terminal-save-failure");
    let mgr = manager(
        &pool,
        options("http://127.0.0.1:1", &dir, None),
        Arc::new(crate::TrustedProxies::for_tests(vec![], false)),
    );
    mgr.begin(JobKind::Remove, &[Step::ResetClientIp], &state.domain)
        .unwrap();
    // All external steps have finished; exercise only terminal persistence.
    mgr.clone()
        .run(
            JobKind::Remove,
            vec![],
            state.clone(),
            ApiToken::new(TOKEN).unwrap(),
        )
        .await;
    let job = mgr.status().await.job.unwrap();
    assert!(job.failed);
    assert!(!job.running);
    assert_eq!(load_stored(&pool).await, Some(state));
    assert_eq!(
        instance::resolve(&pool, &instance::ACCESS_MODE).await.value,
        "internet"
    );
    sqlx::query("DROP TRIGGER reject_final_domain ON instance_setting")
        .execute(&pool)
        .await
        .unwrap();
    save_completed(&pool, JobKind::Remove, &Stored::default())
        .await
        .unwrap();
    assert!(load_stored(&pool).await.is_none());
    assert_eq!(
        instance::resolve(&pool, &instance::ACCESS_MODE).await.value,
        "local"
    );
    pool.close().await;
}

#[tokio::test]
async fn tunnel_setup_then_remove_deletes_exactly_what_it_created() {
    let (api, mock) = start_mock(base_mock()).await;
    let dir = temp_dir("tunnel");
    let pool = pool().await;
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let mgr = manager(
        &pool,
        options(&api, &dir, Some(fake_cloudflared(&dir))),
        trusted.clone(),
    );

    let check = mgr
        .check_token(TOKEN, "https://Read.Example.com/", None)
        .await
        .unwrap();
    assert_eq!(check.zone_name, "example.com");
    assert_eq!(check.account_name, "Test account");

    mgr.start_setup(input("read.example.com", Connection::Tunnel))
        .await
        .unwrap();
    let job = wait_job(&mgr).await;
    all_done(&job);
    assert_eq!(
        job.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        [
            "token",
            "zone",
            "get_connector",
            "tunnel",
            "connector",
            "client_ip",
            "tunnel_dns",
            "cache_rules",
            "cache_headers",
            "health"
        ]
    );

    let stored = load_stored(&pool).await.unwrap();
    let tunnel = stored.tunnel_id.clone().unwrap();
    {
        let m = mock.lock().unwrap();
        let t = &m.tunnels[&tunnel];
        assert_eq!(
            t["config"]["ingress"],
            json!([
                { "hostname": "read.example.com", "service": "http://127.0.0.1:18080", "originRequest": {} },
                { "service": "http_status:404" }
            ])
        );
        let ours: Vec<_> = m.dns.iter().filter(|(id, _)| *id != "dns-other").collect();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0].1["type"], "CNAME");
        assert_eq!(ours[0].1["name"], "read.example.com");
        assert_eq!(ours[0].1["content"], format!("{tunnel}.cfargotunnel.com"));
        let (rs, rules) = m.ruleset.as_ref().unwrap();
        assert_eq!(rs, "rs-existing");
        assert_eq!(rules.len(), 5);
        for r in &rules[1..] {
            assert!(r["ref"].as_str().unwrap().starts_with("komiq_"));
            assert!(r["expression"]
                .as_str()
                .unwrap()
                .contains("http.host eq \"read.example.com\""));
            assert_eq!(r["action"], "set_cache_settings");
            assert_eq!(r["action_parameters"]["edge_ttl"]["mode"], "respect_origin");
        }
        assert_eq!(m.settings["browser_cache_ttl"], json!(0));
        assert_eq!(
            m.settings["ssl"],
            json!("flexible"),
            "a tunnel leaves SSL mode alone"
        );
        assert!(m.certs.is_empty());
    }
    assert_eq!(stored.rule_ids.len(), 4);
    assert!(!stored.ruleset_created);

    // Visitor addresses: loopback (the connector), stored and live.
    assert_eq!(trusted.raw(), vec!["127.0.0.1/32", "::1/128"]);
    let r = instance::resolve(&pool, &instance::TRUSTED_PROXY_CIDRS).await;
    assert_eq!(r.value, "127.0.0.1/32,::1/128");

    // The connector runs with the run token in its environment, not its arguments,
    // and the token file is private.
    let st = mgr.status().await;
    assert_eq!(st.connector, "running");
    assert_eq!(st.mode, "internet");
    assert_eq!(st.address.as_deref(), Some("https://read.example.com"));
    assert_eq!(
        std::fs::read_to_string(dir.join("seen-token")).unwrap(),
        RUN_TOKEN
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("seen-args")).unwrap(),
        "tunnel --no-autoupdate run"
    );
    assert_eq!(mode(&dir.join("data").join(TUNNEL_TOKEN_FILE)), 0o600);

    // No credential is kept in the database or shown in the checklist.
    let raw = instance::resolve(&pool, &instance::CLOUDFLARE_STATE)
        .await
        .value;
    assert!(!raw.contains(TOKEN) && !raw.contains(RUN_TOKEN));
    for s in &job.steps {
        let m = s.message.clone().unwrap_or_default();
        assert!(!m.contains(TOKEN) && !m.contains(RUN_TOKEN));
    }

    // Remove.
    mgr.start_remove(TOKEN).await.unwrap();
    let job = wait_job(&mgr).await;
    all_done(&job);
    {
        let m = mock.lock().unwrap();
        assert_clean(&m, true);
        assert_eq!(m.deleted_tunnels, vec![tunnel]);
        assert!(!m.ruleset_deleted, "the ruleset was there before setup");
    }
    assert!(trusted.raw().is_empty());
    assert_eq!(
        instance::resolve(&pool, &instance::TRUSTED_PROXY_CIDRS)
            .await
            .value,
        ""
    );
    assert!(!dir.join("data").join(TUNNEL_TOKEN_FILE).exists());
    let st = mgr.status().await;
    assert_eq!(st.connector, "off");
    assert_eq!(st.mode, "local");
    assert!(st.cloudflare.is_none());
    assert!(load_stored(&pool).await.is_none());
}

#[tokio::test]
async fn public_ip_setup_serves_https_and_remove_restores_the_zone() {
    let mut m = base_mock();
    m.ruleset = None;
    m.ca = Some(Arc::new(TestCa::new()));
    let ca_pem = m.ca.as_ref().unwrap().pem();
    let (api, mock) = start_mock(m).await;
    let dir = temp_dir("public");
    let pool = pool().await;
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let opts = options(&api, &dir, None);
    let https = opts.https_addr;
    let mgr = manager(&pool, opts, trusted.clone());

    mgr.start_setup(input("read.example.com", Connection::PublicIp))
        .await
        .unwrap();
    let job = wait_job(&mgr).await;
    all_done(&job);
    assert_eq!(
        job.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        [
            "token",
            "zone",
            "certificate",
            "ssl",
            "client_ip",
            "dns",
            "cache_rules",
            "cache_headers",
            "health"
        ]
    );
    let stored = load_stored(&pool).await.unwrap();
    assert_eq!(stored.public_ip.as_deref(), Some("203.0.113.7"), "detected");
    assert!(stored.ruleset_created);
    {
        let m = mock.lock().unwrap();
        let ours: Vec<_> = m.dns.iter().filter(|(id, _)| *id != "dns-other").collect();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0].1["type"], "A");
        assert_eq!(ours[0].1["content"], "203.0.113.7");
        assert_eq!(m.settings["ssl"], json!("strict"));
        assert_eq!(m.certs.len(), 1);
        assert_eq!(m.ruleset.as_ref().unwrap().1.len(), 4);
    }
    assert_eq!(
        trusted.raw(),
        vec!["173.245.48.0/20", "103.21.244.0/22", "2400:cb00::/32"]
    );
    assert_eq!(mode(&dir.join("data").join(ORIGIN_KEY_FILE)), 0o600);

    // The origin answers HTTPS with the Origin CA certificate for the domain.
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .resolve("read.example.com", https)
        // The sandbox may route outbound HTTPS through a proxy; this is local.
        .no_proxy()
        .build()
        .unwrap();
    let url = format!("https://read.example.com:{}/health", https.port());
    let body = client.get(&url).send().await.unwrap().text().await.unwrap();
    assert_eq!(body, "ok");
    assert!(mgr.status().await.https);

    mgr.start_remove(TOKEN).await.unwrap();
    let job = wait_job(&mgr).await;
    all_done(&job);
    {
        let m = mock.lock().unwrap();
        assert_clean(&m, false);
        assert!(
            m.ruleset_deleted,
            "setup created the ruleset, so remove deletes it"
        );
        assert_eq!(m.revoked.len(), 1);
    }
    assert!(trusted.raw().is_empty());
    assert!(!dir.join("data").join(ORIGIN_KEY_FILE).exists());
    assert!(!dir.join("data").join(ORIGIN_CERT_FILE).exists());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!mgr.status().await.https);
    assert!(
        tokio::net::TcpStream::connect(https).await.is_err(),
        "the HTTPS port is released"
    );
}

#[tokio::test]
async fn a_failure_at_any_step_is_retried_and_remove_leaves_nothing() {
    let cases: &[(Connection, &str, u32, &str)] = &[
        (Connection::Tunnel, "verify", 1, "token"),
        (Connection::Tunnel, "zones", 1, "zone"),
        (Connection::Tunnel, "create_tunnel", 1, "tunnel"),
        (Connection::Tunnel, "tunnel_config", 1, "tunnel"),
        (Connection::Tunnel, "tunnel_token", 1, "tunnel"),
        (Connection::Tunnel, "dns_list", 1, "tunnel_dns"),
        (Connection::Tunnel, "dns_create", 1, "tunnel_dns"),
        (Connection::Tunnel, "ruleset_entry", 1, "cache_rules"),
        (Connection::Tunnel, "rule_add", 1, "cache_rules"),
        (Connection::Tunnel, "settings_get", 1, "cache_headers"),
        (Connection::Tunnel, "settings_patch", 1, "cache_headers"),
        (Connection::Tunnel, "health", 2, "health"),
        (Connection::PublicIp, "cert_create", 1, "certificate"),
        (Connection::PublicIp, "settings_patch", 1, "ssl"),
        (Connection::PublicIp, "ips", 1, "client_ip"),
        (Connection::PublicIp, "trace", 1, "dns"),
        (Connection::PublicIp, "ruleset_create", 1, "cache_rules"),
    ];
    for (conn, op, times, step) in cases {
        let mut m = base_mock();
        m.ca = Some(Arc::new(TestCa::new()));
        // "rule_add" fails on the second rule: the first is recorded, then retry
        // must add the rest without duplicating it.
        if *op == "rule_add" {
            m.fail_skip.insert("rule_add".into(), 1);
        }
        m.fail.insert(op.to_string(), *times);
        let had_ruleset = *op != "ruleset_create";
        if !had_ruleset {
            m.ruleset = None;
        }
        let (api, mock) = start_mock(m).await;
        let dir = temp_dir("retry");
        let pool = pool().await;
        let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
        let mgr = manager(
            &pool,
            options(&api, &dir, Some(fake_cloudflared(&dir))),
            trusted,
        );
        mgr.start_setup(input("read.example.com", *conn))
            .await
            .unwrap();
        let job = wait_job(&mgr).await;
        assert!(job.failed, "{op}: expected a failure");
        let failed: Vec<_> = job.steps.iter().filter(|s| s.state == "failed").collect();
        assert_eq!(failed.len(), 1, "{op}");
        assert_eq!(failed[0].id, *step, "{op}: {:?}", states(&job));
        assert!(failed[0].message.as_deref().is_some_and(|m| !m.is_empty()));
        let at = job.steps.iter().position(|s| s.state == "failed").unwrap();
        assert!(job.steps[..at].iter().all(|s| s.state == "done"), "{op}");
        assert!(
            job.steps[at + 1..].iter().all(|s| s.state == "waiting"),
            "{op}"
        );

        mgr.retry_setup(TOKEN).await.unwrap();
        let job = wait_job(&mgr).await;
        all_done(&job);
        {
            let m = mock.lock().unwrap();
            if *conn == Connection::Tunnel {
                assert_eq!(m.tunnels.len(), 1, "{op}: one tunnel, never two");
            }
            assert_eq!(m.dns.len(), 2, "{op}: one record of ours");
            let rules = &m.ruleset.as_ref().unwrap().1;
            let theirs = usize::from(had_ruleset);
            assert_eq!(
                rules.len(),
                4 + theirs,
                "{op}: four rules of ours, no duplicates"
            );
        }

        mgr.start_remove(TOKEN).await.unwrap();
        all_done(&wait_job(&mgr).await);
        assert_clean(&mock.lock().unwrap(), had_ruleset);
        assert!(load_stored(&pool).await.is_none(), "{op}");
    }
}

#[tokio::test]
async fn a_failed_remove_keeps_what_is_left_and_retries() {
    for op in [
        "dns_delete",
        "delete_tunnel",
        "rule_delete",
        "settings_patch",
    ] {
        let (api, mock) = start_mock(base_mock()).await;
        let dir = temp_dir("remove");
        let pool = pool().await;
        let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
        let mgr = manager(
            &pool,
            options(&api, &dir, Some(fake_cloudflared(&dir))),
            trusted,
        );
        mgr.start_setup(input("read.example.com", Connection::Tunnel))
            .await
            .unwrap();
        all_done(&wait_job(&mgr).await);

        mock.lock().unwrap().fail.insert(op.into(), 1);
        mgr.start_remove(TOKEN).await.unwrap();
        let job = wait_job(&mgr).await;
        assert!(job.failed, "{op}");
        let s = load_stored(&pool).await.expect("still stored");
        assert!(s.has_resources(), "{op}: what is left is still recorded");
        let st = mgr.status().await;
        assert!(st.cloudflare.as_ref().unwrap().removable);

        mgr.start_remove(TOKEN).await.unwrap();
        all_done(&wait_job(&mgr).await);
        assert_clean(&mock.lock().unwrap(), true);
        assert!(load_stored(&pool).await.is_none());
    }
}

#[tokio::test]
async fn bad_tokens_and_domains_are_refused_without_echoing_the_token() {
    let (api, _mock) = start_mock(base_mock()).await;
    let dir = temp_dir("bad");
    let pool = pool().await;
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let mgr = manager(&pool, options(&api, &dir, None), trusted);

    let e = mgr
        .check_token("wrong-token-secret-value", "read.example.com", None)
        .await
        .unwrap_err();
    assert!(e.contains("did not accept"), "{e}");
    assert!(!e.contains("wrong-token-secret-value"));
    let e = mgr
        .check_token(TOKEN, "read.other.org", None)
        .await
        .unwrap_err();
    assert!(e.contains("cannot see a zone"), "{e}");
    let e = mgr
        .check_token(TOKEN, "not a domain", None)
        .await
        .unwrap_err();
    assert!(e.contains("domain name"), "{e}");
    let e = mgr
        .check_token("  ", "read.example.com", None)
        .await
        .unwrap_err();
    assert!(e.contains("token"), "{e}");
    assert!(mgr
        .start_setup(CloudflareSetupInput {
            public_ip: Some("192.168.1.10".into()),
            ..input("read.example.com", Connection::PublicIp)
        })
        .await
        .unwrap_err()
        .contains("not a public address"));
    assert!(
        load_stored(&pool).await.is_none(),
        "nothing stored on refusal"
    );
    assert!(mgr.retry_setup(TOKEN).await.is_err());
    assert!(mgr.start_remove(TOKEN).await.is_err());
}

#[tokio::test]
async fn pinned_proxies_are_left_alone_and_a_second_domain_is_refused() {
    let (api, _mock) = start_mock(base_mock()).await;
    let dir = temp_dir("pinned");
    let pool = pool().await;
    let trusted = Arc::new(crate::TrustedProxies::for_tests(
        vec!["10.0.0.0/8".into()],
        true,
    ));
    let mgr = manager(
        &pool,
        options(&api, &dir, Some(fake_cloudflared(&dir))),
        trusted.clone(),
    );
    mgr.start_setup(input("read.example.com", Connection::Tunnel))
        .await
        .unwrap();
    let job = wait_job(&mgr).await;
    all_done(&job);
    let line = job.steps.iter().find(|s| s.id == "client_ip").unwrap();
    assert!(line
        .message
        .as_deref()
        .unwrap()
        .contains("TRUSTED_PROXY_CIDRS"));
    assert_eq!(trusted.raw(), vec!["10.0.0.0/8"]);
    assert_eq!(
        instance::resolve(&pool, &instance::TRUSTED_PROXY_CIDRS)
            .await
            .source,
        instance::Source::Default,
        "nothing written"
    );
    let e = mgr
        .start_setup(input("other.example.com", Connection::Tunnel))
        .await
        .unwrap_err();
    assert!(e.contains("already set up for read.example.com"), "{e}");
    mgr.start_remove(TOKEN).await.unwrap();
    all_done(&wait_job(&mgr).await);
    assert_eq!(trusted.raw(), vec!["10.0.0.0/8"], "still pinned");
}

#[tokio::test]
async fn a_missing_connector_fails_clearly_and_a_restart_resumes() {
    let (api, mock) = start_mock(base_mock()).await;
    let dir = temp_dir("missing");
    let pool = pool().await;
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let mut opts = options(&api, &dir, Some("/nonexistent/cloudflared".into()));
    let mgr = manager(&pool, opts.clone(), trusted.clone());
    mgr.start_setup(input("read.example.com", Connection::Tunnel))
        .await
        .unwrap();
    let job = wait_job(&mgr).await;
    let failed = job.steps.iter().find(|s| s.state == "failed").unwrap();
    assert_eq!(failed.id, "get_connector");
    assert!(failed
        .message
        .as_deref()
        .unwrap()
        .contains("CLOUDFLARED_BIN"));
    assert_eq!(mgr.status().await.connector, "off");
    assert!(mock.lock().unwrap().tunnels.is_empty());
    assert!(!load_stored(&pool).await.unwrap().has_resources());
    assert_eq!(mgr.preflight().await.cloudflared, "unsupported");

    // The operator supplies the connector in the tool folder and restarts.
    let bin = fake_cloudflared(&dir);
    std::fs::create_dir_all(opts.data_dir.join("bin")).unwrap();
    std::fs::copy(bin, opts.data_dir.join("bin/cloudflared")).unwrap();
    opts.cloudflared_bin = None;
    let mgr2 = manager(&pool, opts.clone(), trusted.clone());
    mgr2.retry_setup(TOKEN).await.unwrap();
    all_done(&wait_job(&mgr2).await);
    assert_eq!(mock.lock().unwrap().tunnels.len(), 1);
    mgr2.connector.stop().await;

    // Another restart: the stored setup brings the connector back by itself.
    let mgr3 = manager(&pool, opts, trusted);
    mgr3.resume().await;
    mgr3.connector
        .wait_running(Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(mgr3.status().await.connector, "running");
    mgr3.connector.stop().await;
}

#[tokio::test]
async fn nothing_runs_at_boot_without_a_stored_setup() {
    let pool = pool().await;
    let dir = temp_dir("boot");
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let mgr = manager(&pool, options("http://127.0.0.1:1", &dir, None), trusted);
    mgr.resume().await;
    let st = mgr.status().await;
    assert_eq!(st.connector, "off");
    assert!(!st.https);
    assert!(st.cloudflare.is_none() && st.job.is_none());
    assert_eq!(st.mode, "");
}

#[test]
fn domains_are_normalized_and_checked() {
    assert_eq!(
        normalize_domain(" HTTPS://Read.Example.com/admin ").unwrap(),
        "read.example.com"
    );
    assert_eq!(normalize_domain("example.com.").unwrap(), "example.com");
    for bad in [
        "",
        "localhost",
        "-a.example.com",
        "a..b",
        "exa mple.com",
        "a_b.example.com",
    ] {
        assert!(normalize_domain(bad).is_err(), "{bad}");
    }
    assert!(check_public_ip("203.0.113.7").is_ok());
    assert!(check_public_ip("2001:db8::1").is_ok());
    for bad in [
        "10.0.0.1",
        "127.0.0.1",
        "192.168.0.2",
        "::1",
        "fd00::1",
        "nope",
    ] {
        assert!(check_public_ip(bad).is_err(), "{bad}");
    }
    assert_eq!(
        data_dir_for("sqlite:///data/komika.sqlite3?mode=rwc"),
        std::path::PathBuf::from("/data/cloudflare")
    );
    assert_eq!(
        data_dir_for("sqlite:komika.sqlite3"),
        std::path::PathBuf::from("./cloudflare")
    );
}

#[test]
fn cache_rules_cover_images_and_static_files_on_this_host_only() {
    let rules = cache_rules("read.example.com");
    let refs: Vec<_> = rules.iter().map(|r| r["ref"].as_str().unwrap()).collect();
    assert_eq!(
        refs,
        ["komiq_covers", "komiq_pages", "komiq_img", "komiq_static"]
            .map(|reference| format!("{reference}_097e34e8"))
    );
    let all: String = rules
        .iter()
        .map(|r| r["expression"].as_str().unwrap())
        .collect();
    for path in [
        "/covers/",
        "/api/v1/manga/",
        "\"/img\"",
        "/_app/immutable/",
        "/admin/_app/immutable/",
    ] {
        assert!(all.contains(path), "{path}");
    }
    assert!(rules.iter().all(|r| r["expression"]
        .as_str()
        .unwrap()
        .starts_with("(http.host eq \"read.example.com\" and (")));
}

#[tokio::test]
async fn graphql_is_admin_only_and_drives_a_setup() {
    use crate::graphql::{AppState, KeyedLocks, RateLimiter, RequestAuth, ScanHealth};
    let (api, mock) = start_mock(base_mock()).await;
    let dir = temp_dir("graphql");
    let pool = pool().await;
    for (id, name, admin) in [("a", "owner", 1), ("b", "reader", 0)] {
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, is_admin, created_at) \
             VALUES ($1, $2, $3, 'h', $4, '2020-01-01T00:00:00Z')",
        )
        .bind(crate::db_types::test_uuid(id))
        .bind(name)
        .bind(format!("{name}@e"))
        .bind(admin != 0)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions (token, user_id, created_at, expires_at) \
             VALUES ($1, $2, '2020-01-01T00:00:00Z', '2999-01-01T00:00:00Z')",
        )
        .bind(crate::auth::hash_token(&format!("{name}-session")))
        .bind(crate::db_types::test_uuid(id))
        .execute(&pool)
        .await
        .unwrap();
    }
    let state = Arc::new(AppState {
        pool: pool.clone(),
        cover_pool: crate::database::SqliteDatabase::connect("sqlite::memory:")
            .await
            .unwrap(),
        suwayomi: crate::suwayomi::SuwayomiClient::new("http://127.0.0.1:1".into(), None, None),
        mangadex: Arc::new(crate::mangadex::MangaDexClient::new("test-ua", 5.0, 40.0)),
        admin_users: vec![],
        scan_health: Mutex::new(ScanHealth::default()),
        auth_limiter: RateLimiter::new(100, 60),
        comment_limiter: crate::graphql::comment_limiter(),
        rating_limiter: crate::graphql::rating_limiter(),
        password_work: Arc::new(tokio::sync::Semaphore::new(4)),
        federated_limiter: RateLimiter::new(100, 60),
        session_ttl_secs: 3600,
        series_inflight: KeyedLocks::default(),
        chapters_inflight: KeyedLocks::default(),
        cover_crawl_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        catalogue_cover_phash: false,
        ext_icons_dir: std::path::PathBuf::new(),
        setup_code: crate::instance::SetupCode::default(),
    });
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let mgr = manager(
        &pool,
        options(&api, &dir, Some(fake_cloudflared(&dir))),
        trusted,
    );
    let schema = crate::graphql::schema_builder(state, false)
        .data(mgr.clone())
        .finish();
    let exec = |q: String, who: &str| {
        let req = async_graphql::Request::new(q)
            .data(RequestAuth(Some(format!("{who}-session"))))
            .data(crate::graphql::ClientIp(Some("127.0.0.1".into())));
        let schema = schema.clone();
        async move { schema.execute(req).await }
    };

    let ops = [
        "{ accessStatus { mode } }".to_string(),
        format!("mutation {{ checkCloudflareToken(token:\"{TOKEN}\", domain:\"read.example.com\") {{ zoneName }} }}"),
        format!("mutation {{ startCloudflareSetup(input:{{token:\"{TOKEN}\", domain:\"read.example.com\", connection:TUNNEL}}) {{ mode }} }}"),
        format!("mutation {{ retryCloudflareSetup(token:\"{TOKEN}\") {{ mode }} }}"),
        format!("mutation {{ removeCloudflareSetup(token:\"{TOKEN}\") {{ mode }} }}"),
        "mutation { setAccessMode(mode:\"local\") { mode } }".to_string(),
    ];
    for op in ops.iter().chain(std::iter::once(
        &"{ accessPreflight { httpPort } }".to_string(),
    )) {
        let r = exec(op.clone(), "reader").await;
        assert_eq!(r.errors.len(), 1, "{op}");
        assert_eq!(r.errors[0].message, "Admin access required", "{op}");
    }
    assert!(
        mock.lock().unwrap().calls.is_empty(),
        "nothing reached Cloudflare"
    );

    let r = exec(ops[1].clone(), "owner").await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(
        r.data.into_json().unwrap()["checkCloudflareToken"]["zoneName"],
        "example.com"
    );
    let r = exec(ops[2].clone(), "owner").await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    all_done(&wait_job(&mgr).await);
    let r = exec(
        "{ accessStatus { mode address connector cloudflare { domain connection complete removable } job { kind running failed steps { id state } } } }".into(),
        "owner",
    )
    .await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let d = r.data.into_json().unwrap();
    let s = &d["accessStatus"];
    assert_eq!(s["mode"], "internet");
    assert_eq!(s["address"], "https://read.example.com");
    assert_eq!(s["connector"], "running");
    assert_eq!(s["cloudflare"]["connection"], "TUNNEL");
    assert_eq!(s["cloudflare"]["complete"], true);
    assert_eq!(s["job"]["steps"].as_array().unwrap().len(), 10);
    assert!(!d.to_string().contains(TOKEN));

    let r = exec(ops[4].clone(), "owner").await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    all_done(&wait_job(&mgr).await);
    assert_clean(&mock.lock().unwrap(), true);

    let r = exec(
        "mutation { setAccessMode(mode:\"internet\", domain:\"Manual.Example.com\") { mode domain address } }".into(),
        "owner",
    )
    .await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let d = r.data.into_json().unwrap();
    assert_eq!(d["setAccessMode"]["address"], "https://manual.example.com");
    let r = exec(
        "mutation { setAccessMode(mode:\"public\") { mode } }".into(),
        "owner",
    )
    .await;
    assert_eq!(r.errors.len(), 1);

    // Direct: public at the server's own address, no domain and no Cloudflare.
    let r = exec(
        "mutation { setAccessMode(mode:\"direct\", domain:\" 203.0.113.5:8080/ \") { mode domain address } }".into(),
        "owner",
    )
    .await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let d = r.data.into_json().unwrap();
    assert_eq!(d["setAccessMode"]["mode"], "direct");
    assert_eq!(d["setAccessMode"]["address"], "http://203.0.113.5:8080");
    // The domain set up by hand before is kept for a later return to internet mode.
    assert_eq!(d["setAccessMode"]["domain"], "manual.example.com");
    // With `encrypt`, the address is saved only once a certificate is in place. No
    // certificate authority answers here, so nothing changes.
    let r = exec(
        "mutation { setAccessMode(mode:\"direct\", domain:\"203.0.113.9\", encrypt:true) { mode } }".into(),
        "owner",
    )
    .await;
    assert_eq!(r.errors.len(), 1);
    assert!(
        r.errors[0]
            .message
            .contains("Could not get a certificate for 203.0.113.9"),
        "{:?}",
        r.errors
    );
    assert_eq!(
        mgr.status().await.address.as_deref(),
        Some("http://203.0.113.5:8080")
    );
    // It needs an address, and the mode stays as it was when there is none.
    for bad in [
        "mode:\"direct\"",
        "mode:\"direct\", domain:\"localhost:8080\"",
    ] {
        let r = exec(
            format!("mutation {{ setAccessMode({bad}) {{ mode }} }}"),
            "owner",
        )
        .await;
        assert_eq!(r.errors.len(), 1, "{bad}");
    }
    // Back to the home network: no public address is shown.
    let r = exec(
        "mutation { setAccessMode(mode:\"local\") { mode address } }".into(),
        "owner",
    )
    .await;
    let d = r.data.into_json().unwrap();
    assert_eq!(d["setAccessMode"]["mode"], "local");
    assert!(d["setAccessMode"]["address"].is_null());
}

#[test]
fn direct_addresses_become_origins() {
    for (raw, origin) in [
        ("203.0.113.5:8080", "http://203.0.113.5:8080"),
        (" http://203.0.113.5:8080/ ", "http://203.0.113.5:8080"),
        ("203.0.113.5", "http://203.0.113.5"),
        (" 2606:4700:4700::1111 ", "http://[2606:4700:4700::1111]"),
        ("HTTP://VPS.Example.net:7580", "http://vps.example.net:7580"),
        ("https://read.example.com", "https://read.example.com"),
        ("[2001:db8::1]:8080", "http://[2001:db8::1]:8080"),
        ("10.8.0.2:8080", "http://10.8.0.2:8080"),
    ] {
        assert_eq!(normalize_address(raw).as_deref(), Ok(origin), "{raw}");
    }
    for bad in [
        "",
        "   ",
        "localhost:8080",
        "127.0.0.1:8080",
        "0.0.0.0:8080",
        "[::1]:8080",
        "::1",
        "::",
        "ftp://203.0.113.5",
        "203.0.113.5:8080/admin",
        "http://user:pw@203.0.113.5:8080",
        "203.0.113.5:99999",
        "not an address",
    ] {
        assert!(normalize_address(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn preflight_port_states_use_the_real_listener() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert_eq!(port_state(addr, false).await, "in_use");
    assert_eq!(port_state(addr, true).await, "serving");
    drop(listener);
    assert_eq!(port_state(addr, false).await, "free");
    assert_eq!(
        port_error_state(std::io::ErrorKind::PermissionDenied),
        "denied"
    );
    assert_eq!(
        port_error_state(std::io::ErrorKind::AddrNotAvailable),
        "unknown"
    );
}

#[tokio::test]
async fn token_permission_advice_is_read_only_and_connection_specific() {
    for connection in [None, Some(Connection::Tunnel), Some(Connection::PublicIp)] {
        for status in [
            StatusCode::OK,
            StatusCode::FORBIDDEN,
            StatusCode::UNAUTHORIZED,
            StatusCode::NOT_FOUND,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let seen = calls.clone();
            let router = Router::new().fallback(move |method: Method, uri: Uri| {
                let seen = seen.clone();
                async move {
                    assert_eq!(method, Method::GET);
                    seen.lock().unwrap().push(uri.to_string());
                    if uri.path() == "/user/tokens/verify" {
                        return ok(json!({ "status": "active" }));
                    }
                    if uri.path() == "/zones" {
                        return ok(json!([{ "id": "zone1", "name": "example.com", "status": "active", "account": { "id": "acc1", "name": "Test" } }]));
                    }
                    if uri.path().ends_with("dns_records") { return (status, "probe").into_response(); }
                    ok(json!([]))
                }
            });
            let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", tcp.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(tcp, router).await.unwrap();
            });
            let dir = temp_dir("probes");
            let mgr = manager(
                &pool().await,
                options(&base, &dir, None),
                Arc::new(crate::TrustedProxies::for_tests(vec![], false)),
            );
            let check = mgr
                .check_token(TOKEN, "example.com", connection)
                .await
                .unwrap();
            assert_eq!(
                check.missing,
                if matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED) {
                    vec!["Zone › DNS"]
                } else {
                    vec![]
                }
            );
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), if connection.is_some() { 6 } else { 5 });
            assert_eq!(
                calls.iter().any(|p| p.starts_with("/accounts/")),
                connection == Some(Connection::Tunnel)
            );
            assert_eq!(
                calls.iter().any(|p| p.starts_with("/certificates?")),
                connection == Some(Connection::PublicIp)
            );
            server.abort();
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[tokio::test]
async fn preflight_finds_downloaded_connector_and_detects_public_ip() {
    let (api, _) = start_mock(base_mock()).await;
    let dir = temp_dir("preflight");
    let opts = options(&api, &dir, None);
    std::fs::create_dir_all(opts.data_dir.join("bin")).unwrap();
    let bin = opts.data_dir.join("bin").join(if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    });
    std::fs::write(&bin, "binary").unwrap();
    let mgr = manager(
        &pool().await,
        opts,
        Arc::new(crate::TrustedProxies::for_tests(vec![], false)),
    );
    assert_eq!(mgr.connector.resolve_bin().unwrap(), bin);
    let preflight = mgr.preflight().await;
    assert_eq!(preflight.cloudflared, "found");
    assert_eq!(preflight.public_ip.as_deref(), Some("203.0.113.7"));
    assert_eq!(preflight.http_port, 18080);
    assert_eq!(preflight.https_port_state, "free");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn lan_visit_uses_socket_peer_and_ignores_headers_and_containers() {
    use tower::ServiceExt;
    let dir = temp_dir("lan-peer");
    let mut mgr = Manager::new(
        pool().await,
        options_with_ports("http://127.0.0.1:9", &dir, None, (443, 80)),
        Arc::new(crate::TrustedProxies::for_tests(vec![], false)),
    );
    mgr.in_container = false;
    let mgr = Arc::new(mgr);
    let router = Router::new()
        .route("/", axum::routing::get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            mgr.clone(),
            plain_http_guard,
        ));
    let request = |peer: &str| {
        let mut request = axum::http::Request::builder()
            .uri("/")
            .header("x-forwarded-for", "192.168.1.5")
            .header("cf-connecting-ip", "192.168.1.5")
            .body(axum::body::Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(SocketAddr::new(
                peer.parse().unwrap(),
                50000,
            )));
        request
    };
    router.clone().oneshot(request("127.0.0.1")).await.unwrap();
    router
        .clone()
        .oneshot(request("203.0.113.1"))
        .await
        .unwrap();
    assert!(mgr.status().await.lan_client_seen_at.is_none());
    router.oneshot(request("192.168.1.5")).await.unwrap();
    let at = mgr.status().await.lan_client_seen_at.unwrap();
    chrono::DateTime::parse_from_rfc3339(&at).unwrap();
    let mut container = Manager::new(
        pool().await,
        options_with_ports("http://127.0.0.1:9", &dir, None, (443, 80)),
        Arc::new(crate::TrustedProxies::for_tests(vec![], false)),
    );
    container.in_container = true;
    container.record_lan_peer("192.168.1.5".parse().unwrap());
    assert!(container.status().await.lan_client_seen_at.is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn access_modes_pause_and_resume_cloudflare_without_losing_resources() {
    for connection in [Connection::Tunnel, Connection::PublicIp] {
        let mut mock = base_mock();
        mock.ca = Some(Arc::new(TestCa::new()));
        let (api, mock) = start_mock(mock).await;
        let dir = temp_dir("pause");
        let pool = pool().await;
        let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
        let opts = options(&api, &dir, Some(fake_cloudflared(&dir)));
        let mgr = manager(&pool, opts.clone(), trusted.clone());
        mgr.start_setup(input("read.example.com", connection))
            .await
            .unwrap();
        all_done(&wait_job(&mgr).await);
        let stored = load_stored(&pool).await.unwrap();
        let proxies = trusted.raw();

        mgr.set_access("local", None, false).await.unwrap();
        assert_eq!(mgr.status().await.connector, "off");
        assert!(!mgr.https_up());
        assert_eq!(load_stored(&pool).await.unwrap(), stored);
        assert_eq!(trusted.raw(), proxies);
        if connection == Connection::Tunnel {
            assert!(mgr.token_file().is_file());
        } else {
            assert!(opts.data_dir.join(ORIGIN_KEY_FILE).is_file());
            assert!(opts.data_dir.join(ORIGIN_CERT_FILE).is_file());
        }
        let again = manager(&pool, opts.clone(), trusted.clone());
        again.resume().await;
        assert_eq!(again.status().await.connector, "off");
        assert!(!again.https_up());

        again
            .set_access("internet", Some("read.example.com"), false)
            .await
            .unwrap();
        if connection == Connection::Tunnel {
            again
                .connector
                .wait_running(opts.connector_timeout)
                .await
                .unwrap();
        } else {
            assert!(again.https_up());
        }
        assert_eq!(load_stored(&pool).await.unwrap(), stored);

        again
            .set_access("direct", Some("203.0.113.7:8080"), false)
            .await
            .unwrap();
        assert_eq!(again.status().await.connector, "off");
        assert!(!again.https_up());
        let direct_boot = manager(&pool, opts.clone(), trusted.clone());
        direct_boot.resume().await;
        assert_eq!(direct_boot.status().await.connector, "off");
        assert!(!direct_boot.https_up());

        // A repair while paused still runs, and records internet mode on success.
        again.set_access("local", None, false).await.unwrap();
        again.retry_setup(TOKEN).await.unwrap();
        all_done(&wait_job(&again).await);
        assert_eq!(again.status().await.mode, "internet");
        if connection == Connection::Tunnel {
            assert_eq!(again.status().await.connector, "running");
        } else {
            assert!(again.https_up());
        }

        // Old installs have no access choice, and still resume.
        again.set_access("local", None, false).await.unwrap();
        instance::store(&pool, &instance::ACCESS_MODE, "")
            .await
            .unwrap();
        let legacy = manager(&pool, opts, trusted.clone());
        legacy.resume().await;
        if connection == Connection::Tunnel {
            legacy
                .connector
                .wait_running(Duration::from_secs(5))
                .await
                .unwrap();
        } else {
            assert!(legacy.https_up());
        }
        legacy.set_access("local", None, false).await.unwrap();
        legacy.start_remove(TOKEN).await.unwrap();
        all_done(&wait_job(&legacy).await);
        assert_clean(&mock.lock().unwrap(), true);
        assert!(load_stored(&pool).await.is_none());
    }
}

#[tokio::test]
async fn cache_rules_for_two_servers_are_owned_separately() {
    let (api, mock) = start_mock(base_mock()).await;
    let mut servers = vec![];
    for domain in ["read1.example.com", "read2.example.com"] {
        let dir = temp_dir("rules-host");
        let pool = pool().await;
        let mgr = manager(
            &pool,
            options(&api, &dir, Some(fake_cloudflared(&dir))),
            Arc::new(crate::TrustedProxies::for_tests(vec![], false)),
        );
        mgr.start_setup(input(domain, Connection::Tunnel))
            .await
            .unwrap();
        all_done(&wait_job(&mgr).await);
        let stored = load_stored(&pool).await.unwrap();
        servers.push((mgr, stored));
    }
    let first = &servers[0].1;
    let second = &servers[1].1;
    assert_eq!(mock.lock().unwrap().ruleset.as_ref().unwrap().1.len(), 9);
    assert!(first
        .rule_ids
        .iter()
        .all(|id| !second.rule_ids.contains(id)));
    servers[0].0.start_remove(TOKEN).await.unwrap();
    all_done(&wait_job(&servers[0].0).await);
    {
        let m = mock.lock().unwrap();
        let rules = &m.ruleset.as_ref().unwrap().1;
        assert_eq!(rules.len(), 5);
        for id in &second.rule_ids {
            assert!(rules.iter().any(|r| r["id"] == *id));
        }
    }
    servers[1].0.start_remove(TOKEN).await.unwrap();
    all_done(&wait_job(&servers[1].0).await);
    assert_clean(&mock.lock().unwrap(), true);
}

#[tokio::test]
async fn legacy_cache_rules_are_adopted_only_for_their_host() {
    let mut mock = base_mock();
    mock.ruleset.as_mut().unwrap().1.extend([
        json!({"id": "old-ours", "ref": "komiq_covers",
            "expression": "(http.host eq \"read.example.com\" and true)"}),
        json!({"id": "old-theirs", "ref": "komiq_pages",
            "expression": "(http.host eq \"other.example.com\" and true)"}),
    ]);
    let (api, mock) = start_mock(mock).await;
    let dir = temp_dir("legacy-rules");
    let pool = pool().await;
    let mgr = manager(
        &pool,
        options(&api, &dir, None),
        Arc::new(crate::TrustedProxies::for_tests(vec![], false)),
    );
    let client = Client::new(&api, ApiToken::new(TOKEN).unwrap());
    let mut stored = Stored {
        domain: "read.example.com".into(),
        zone_id: Some("zone1".into()),
        ..Stored::default()
    };
    mgr.step_cache_rules(&client, &mut stored).await.unwrap();
    assert_eq!(stored.rule_ids.len(), 4);
    assert!(stored.rule_ids.contains(&"old-ours".into()));
    assert!(!stored.rule_ids.contains(&"old-theirs".into()));
    mgr.step_cache_rules(&client, &mut stored).await.unwrap();
    assert_eq!(
        mock.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|op| *op == "rule_add")
            .count(),
        3
    );
    mgr.step(Step::DeleteCacheRules, &client, &mut stored)
        .await
        .unwrap();
    let m = mock.lock().unwrap();
    let rules = &m.ruleset.as_ref().unwrap().1;
    assert_eq!(rules.len(), 2);
    assert!(rules.iter().any(|r| r["id"] == "old-theirs"));
}

#[tokio::test]
async fn dns_repairs_owned_records_and_leaves_matching_records_alone() {
    let (api, mock) = start_mock(base_mock()).await;
    let dir = temp_dir("dns-repair");
    let pool = pool().await;
    let mgr = manager(
        &pool,
        options(&api, &dir, None),
        Arc::new(crate::TrustedProxies::for_tests(vec![], false)),
    );
    let client = Client::new(&api, ApiToken::new(TOKEN).unwrap());
    let mut stored = Stored {
        domain: "read.example.com".into(),
        zone_id: Some("zone1".into()),
        public_ip: Some("203.0.113.7".into()),
        ..Stored::default()
    };
    mgr.step(Step::Dns, &client, &mut stored).await.unwrap();
    let id = stored.dns_record_ids[0].clone();
    stored.public_ip = Some("203.0.113.8".into());
    mgr.step(Step::Dns, &client, &mut stored).await.unwrap();
    assert_eq!(stored.dns_record_ids.as_slice(), std::slice::from_ref(&id));
    assert_eq!(mock.lock().unwrap().dns[&id]["content"], "203.0.113.8");
    mock.lock().unwrap().calls.clear();
    mgr.step(Step::Dns, &client, &mut stored).await.unwrap();
    assert_eq!(mock.lock().unwrap().calls, ["dns_list"]);

    mock.lock().unwrap().dns.get_mut(&id).unwrap()["proxied"] = json!(false);
    mgr.step(Step::Dns, &client, &mut stored).await.unwrap();
    assert_eq!(mock.lock().unwrap().dns[&id]["proxied"], true);
    mock.lock().unwrap().dns.remove(&id);
    mgr.step(Step::Dns, &client, &mut stored).await.unwrap();
    assert_ne!(stored.dns_record_ids[0], id);

    // A -> AAAA -> tunnel CNAME replaces the id rather than patching its type.
    for (step, kind, content) in [
        (Step::Dns, "AAAA", "2606:4700:4700::1111"),
        (Step::TunnelDns, "CNAME", "tun-existing.cfargotunnel.com"),
    ] {
        let previous = stored.dns_record_ids[0].clone();
        stored.public_ip = Some(content.into());
        stored.tunnel_id = Some("tun-existing".into());
        mgr.step(step, &client, &mut stored).await.unwrap();
        let m = mock.lock().unwrap();
        assert!(!m.dns.contains_key(&previous));
        let record = &m.dns[&stored.dns_record_ids[0]];
        assert_eq!(record["type"], kind);
        assert_eq!(record["content"], content);
    }
    let id = stored.dns_record_ids[0].clone();
    mock.lock().unwrap().dns.get_mut(&id).unwrap()["content"] = json!("wrong.cfargotunnel.com");
    mgr.step(Step::TunnelDns, &client, &mut stored)
        .await
        .unwrap();
    assert_eq!(
        mock.lock().unwrap().dns[&id]["content"],
        "tun-existing.cfargotunnel.com"
    );
    assert_eq!(
        load_stored(&pool).await.unwrap().dns_record_ids,
        stored.dns_record_ids
    );
}

#[tokio::test]
async fn tunnel_id_is_saved_before_configuring_the_tunnel() {
    let mut mock = base_mock();
    mock.fail.insert("tunnel_config".into(), 1);
    let (api, mock) = start_mock(mock).await;
    let dir = temp_dir("tunnel-id");
    let pool = pool().await;
    let mgr = manager(
        &pool,
        options(&api, &dir, Some(fake_cloudflared(&dir))),
        Arc::new(crate::TrustedProxies::for_tests(vec![], false)),
    );
    let client = Client::new(&api, ApiToken::new(TOKEN).unwrap());
    let mut stored = Stored {
        domain: "read.example.com".into(),
        connection: Some(Connection::Tunnel),
        zone_id: Some("zone1".into()),
        account_id: Some("acc1".into()),
        ..Stored::default()
    };
    // Calling the step directly prevents run's final save from hiding a lost id.
    assert!(mgr.step_tunnel(&client, &mut stored).await.is_err());
    let saved = load_stored(&pool).await.unwrap();
    assert!(saved.tunnel_id.is_some());
    assert_eq!(saved.tunnel_id, stored.tunnel_id);
    mgr.retry_setup(TOKEN).await.unwrap();
    all_done(&wait_job(&mgr).await);
    assert_eq!(
        mock.lock()
            .unwrap()
            .calls
            .iter()
            .filter(|op| *op == "create_tunnel")
            .count(),
        1
    );
    mgr.start_remove(TOKEN).await.unwrap();
    all_done(&wait_job(&mgr).await);
    assert_clean(&mock.lock().unwrap(), true);
}

#[tokio::test]
async fn retry_repairs_changed_public_ips_and_keeps_entered_addresses() {
    for detect in [true, false] {
        let mut mock = base_mock();
        mock.ca = Some(Arc::new(TestCa::new()));
        let (api, mock) = start_mock(mock).await;
        let dir = temp_dir("changed-ip");
        let pool = pool().await;
        let mgr = manager(
            &pool,
            options(&api, &dir, None),
            Arc::new(crate::TrustedProxies::for_tests(vec![], false)),
        );
        let mut setup = input("read.example.com", Connection::PublicIp);
        if !detect {
            setup.public_ip = Some("203.0.113.9".into());
        }
        mgr.start_setup(setup).await.unwrap();
        all_done(&wait_job(&mgr).await);
        let before = load_stored(&pool).await.unwrap();
        let id = before.dns_record_ids[0].clone();
        {
            let mut m = mock.lock().unwrap();
            m.public_ip = Some("203.0.113.8".into());
            m.calls.clear();
        }
        mgr.retry_setup(TOKEN).await.unwrap();
        all_done(&wait_job(&mgr).await);
        let after = load_stored(&pool).await.unwrap();
        assert_eq!(before.dns_record_ids, after.dns_record_ids);
        let desired = if detect { "203.0.113.8" } else { "203.0.113.9" };
        assert_eq!(after.public_ip.as_deref(), Some(desired));
        {
            let m = mock.lock().unwrap();
            assert_eq!(m.dns[&id]["content"], desired);
            assert_eq!(
                m.calls.iter().filter(|op| *op == "dns_update").count(),
                usize::from(detect)
            );
        }
        mgr.start_remove(TOKEN).await.unwrap();
        all_done(&wait_job(&mgr).await);
        assert_clean(&mock.lock().unwrap(), true);
    }
}

/// A finished public-IP setup for `read.example.com`, its HTTPS listener stopped: what
/// a restart finds.
async fn public_ip_setup(
    name: &str,
) -> (
    Shared,
    Database,
    Options,
    Arc<crate::TrustedProxies>,
    Arc<TestCa>,
) {
    let ca = Arc::new(TestCa::new());
    let mut m = base_mock();
    m.ca = Some(ca.clone());
    let (api, mock) = start_mock(m).await;
    let pool = pool().await;
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let opts = options(&api, &temp_dir(name), None);
    let mgr = manager(&pool, opts.clone(), trusted.clone());
    mgr.start_setup(input("read.example.com", Connection::PublicIp))
        .await
        .unwrap();
    all_done(&wait_job(&mgr).await);
    mgr.stop_https().await;
    (mock, pool, opts, trusted, ca)
}

async fn wait_https(m: &Manager) -> AccessStatus {
    for _ in 0..200 {
        let st = m.status().await;
        if st.https {
            return st;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("HTTPS did not come up");
}

#[tokio::test]
async fn https_starts_at_boot_despite_a_dead_connector_or_a_busy_port() {
    let (_mock, pool, mut opts, trusted, _ca) = public_ip_setup("boot-https").await;

    // A tunnel is stored too, and its connector cannot start: HTTPS starts anyway.
    let mut s = load_stored(&pool).await.unwrap();
    s.tunnel_id = Some("tunnel-0001".into());
    save_stored(&pool, Some(&s)).await.unwrap();
    std::fs::write(opts.data_dir.join(TUNNEL_TOKEN_FILE), RUN_TOKEN).unwrap();
    opts.cloudflared_bin = Some("/nonexistent/cloudflared".into());
    let boot = manager(&pool, opts.clone(), trusted.clone());
    boot.resume().await;
    let st = boot.status().await;
    assert_eq!(st.connector, "missing");
    assert!(st.https && st.https_error.is_none());
    boot.stop_https().await;

    // Another program holds the HTTPS port at boot: the reason shows, and HTTPS comes
    // up by itself once the port is free.
    let other_program = std::net::TcpListener::bind(opts.https_addr).unwrap();
    let boot = manager(&pool, opts.clone(), trusted.clone());
    boot.resume().await;
    let st = boot.status().await;
    assert!(!st.https);
    let why = st.https_error.expect("the console can show why");
    assert!(
        why.contains(&format!("port {}", opts.https_addr.port())),
        "{why}"
    );
    drop(other_program);
    let st = wait_https(&boot).await;
    assert!(st.https_error.is_none());

    // "Only my devices" while the port is still busy: nothing is left to try.
    boot.stop_https().await;
    let other_program = std::net::TcpListener::bind(opts.https_addr).unwrap();
    let boot = manager(&pool, opts.clone(), trusted);
    boot.resume().await;
    assert!(boot.status().await.https_error.is_some());
    boot.set_access("local", None, false).await.unwrap();
    assert!(boot.status().await.https_error.is_none());
    drop(other_program);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!boot.status().await.https, "the retry stopped");
}

#[tokio::test]
async fn run_setup_again_replaces_an_origin_certificate_that_is_due() {
    let (mock, pool, opts, trusted, ca) = public_ip_setup("origin-renew").await;
    let cert_file = opts.data_dir.join(ORIGIN_CERT_FILE);
    let key_file = opts.data_dir.join(ORIGIN_KEY_FILE);
    let domain = der_san_extension("read.example.com");
    let on_disk = |days| {
        let key = std::fs::read(&key_file).unwrap();
        std::fs::write(&cert_file, ca.sign_for_key(&key, &domain, days)).unwrap();
    };
    let mgr = manager(&pool, opts.clone(), trusted);
    let first = load_stored(&pool).await.unwrap().certificate_id.unwrap();

    // A certificate with years left: its end date shows, and a re-run keeps it.
    let st = mgr.status().await;
    let until = origin_tls::validity(&std::fs::read_to_string(&cert_file).unwrap())
        .unwrap()
        .1;
    assert_eq!(
        st.certificate_expires,
        Some(crate::db_types::format_date(until))
    );
    assert!(st.certificate_error.is_none());
    mgr.retry_setup(TOKEN).await.unwrap();
    all_done(&wait_job(&mgr).await);
    assert_eq!(
        load_stored(&pool).await.unwrap().certificate_id,
        Some(first.clone())
    );
    assert!(mock.lock().unwrap().revoked.is_empty());

    // Close to its end, past it, or for another domain: the console says so, and a
    // re-run gets a new one and revokes the old one.
    let other = der_san_extension("old.example.com");
    let cases: [(&dyn Fn(), &str); 3] = [
        (&|| on_disk((-400, 10)), "ends on"),
        (&|| on_disk((-400, -1)), "ended on"),
        (
            &|| {
                let key = std::fs::read(&key_file).unwrap();
                std::fs::write(&cert_file, ca.sign_for_key(&key, &other, (-1, 400))).unwrap();
            },
            "is not for read.example.com",
        ),
    ];
    let mut current = first;
    for (n, (make_due, why)) in cases.iter().enumerate() {
        make_due();
        let due = mgr.status().await.certificate_error.unwrap_or_default();
        assert!(
            due.contains(why) && due.contains("Run setup again"),
            "{due}"
        );

        // A refused request leaves the certificate on disk as it was.
        let before = std::fs::read(&cert_file).unwrap();
        let key_before = std::fs::read(&key_file).unwrap();
        mock.lock().unwrap().fail.insert("cert_create".into(), 1);
        mgr.retry_setup(TOKEN).await.unwrap();
        let job = wait_job(&mgr).await;
        assert!(job.failed);
        assert_eq!(std::fs::read(&cert_file).unwrap(), before);
        assert_eq!(std::fs::read(&key_file).unwrap(), key_before);

        mgr.retry_setup(TOKEN).await.unwrap();
        all_done(&wait_job(&mgr).await);
        let now = load_stored(&pool).await.unwrap().certificate_id.unwrap();
        assert_ne!(now, current, "a new certificate");
        {
            let m = mock.lock().unwrap();
            assert_eq!(m.revoked.len(), n + 1);
            assert_eq!(m.revoked[n], current, "the old one is revoked");
            assert_eq!(m.certs.keys().collect::<Vec<_>>(), [&now]);
        }
        let st = mgr.status().await;
        assert!(st.https && st.certificate_error.is_none());
        assert!(origin_tls::is_for(
            &std::fs::read_to_string(&cert_file).unwrap(),
            "read.example.com"
        ));
        current = now;
    }

    // The new certificate is the one that serves, not the one of the run before.
    use tokio_rustls::rustls::pki_types::{pem::PemObject, CertificateDer};
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(ca.pem().as_bytes()).unwrap())
        .resolve("read.example.com", opts.https_addr)
        .no_proxy()
        .tls_info(true)
        .build()
        .unwrap();
    let url = format!("https://read.example.com:{}/health", opts.https_addr.port());
    let answer = client.get(url).send().await.unwrap();
    assert_eq!(answer.status(), 200);
    let served = answer
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(|tls| tls.peer_certificate())
        .unwrap();
    let on_disk = CertificateDer::pem_file_iter(&cert_file)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(served, on_disk.as_ref());
}

#[tokio::test]
async fn remove_leaves_no_https_listener_when_the_retry_comes_in_the_middle() {
    let (mock, pool, opts, trusted, _ca) = public_ip_setup("retry-remove").await;
    // Another program holds the HTTPS port at boot, so the server keeps trying.
    let other_program = std::net::TcpListener::bind(opts.https_addr).unwrap();
    let boot = manager(&pool, opts.clone(), trusted);
    let started = std::time::Instant::now();
    boot.resume().await;
    assert!(boot.status().await.https_error.is_some());

    // "Remove" is at Cloudflare, revoking the certificate, when the port gets free.
    let gate = Arc::new(tokio::sync::Notify::new());
    mock.lock()
        .unwrap()
        .gate
        .insert("cert_revoke".into(), gate.clone());
    boot.start_remove(TOKEN).await.unwrap();
    let revoking = || {
        mock.lock()
            .unwrap()
            .calls
            .iter()
            .any(|c| c == "cert_revoke")
    };
    for _ in 0..400 {
        if revoking() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(revoking());
    drop(other_program);
    // Long enough for the next try: each wait is twice the one before.
    tokio::time::sleep(started.elapsed() * 2 + Duration::from_millis(250)).await;
    gate.notify_one();

    all_done(&wait_job(&boot).await);
    assert!(!boot.status().await.https);
    assert!(
        std::net::TcpListener::bind(opts.https_addr).is_ok(),
        "nothing serves the revoked certificate"
    );
}

#[tokio::test]
async fn preflight_reports_the_port_other_machines_use() {
    let pool = pool().await;
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    // Docker with KOMIQ_PORT=9000: the host publishes 9000, the server listens on 18080.
    let mut opts = options("http://127.0.0.1:9", &temp_dir("published"), None);
    opts.published_port = 9000;
    let mgr = manager(&pool, opts, trusted);
    assert_eq!(mgr.preflight().await.http_port, 9000);

    // The compose file hands it over in KOMIKA_PUBLISHED_PORT. Without it, the port
    // is the listener's own.
    let cfg = crate::config::Config::from_env();
    std::env::set_var("KOMIKA_PUBLISHED_PORT", "9000");
    assert_eq!(Options::from_env(&cfg).published_port, 9000);
    std::env::remove_var("KOMIKA_PUBLISHED_PORT");
    assert_eq!(Options::from_env(&cfg).published_port, cfg.port);
}
