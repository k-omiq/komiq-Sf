//! Direct mode with HTTPS against a local mock of an ACME certificate authority.
//! Nothing here reaches Let's Encrypt. The mock checks what a real one checks: every
//! request's signature, nonce and URL, and the http-01 answer on the server's port-80
//! listener. It signs with the test CA, so a client that trusts that CA can verify the
//! served certificate for the IP address.

use std::collections::HashSet;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

use super::super::direct::{direct_host, DIRECT_CERT_FILE, DIRECT_KEY_FILE};
use super::super::origin_tls::{acme_csr_der, der_san_ip_extension, names, validity};
use super::*;

struct AcmeMock {
    base: String,
    ca: TestCa,
    challenge_port: u16,
    nonces: HashSet<String>,
    next_nonce: u32,
    account_jwk: Option<Value>,
    orders: u32,
    last_order: Value,
    /// `pending`, `valid` or `invalid`.
    authz: &'static str,
    /// `pending`, `ready` or `valid`.
    order: &'static str,
    chain: String,
    /// Answer the next signed request with `badNonce`, once.
    stale_nonce: bool,
    stale_nonces_sent: u32,
    /// The check fails with this detail.
    refuse: Option<String>,
    /// Days from now for notBefore and notAfter of what it signs.
    days: (i64, i64),
    /// The same, for the first certificate only.
    first_days: Option<(i64, i64)>,
}

type SharedAcme = Arc<Mutex<AcmeMock>>;

const TOKEN_VALUE: &str = "mock-token-abc123";

fn problem(kind: &str, detail: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "type": format!("urn:ietf:params:acme:error:{kind}"),
            "detail": detail,
        })),
    )
        .into_response()
}

fn thumbprint(jwk: &Value) -> String {
    let canonical = format!(
        r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
        jwk["x"].as_str().unwrap(),
        jwk["y"].as_str().unwrap()
    );
    URL_SAFE_NO_PAD.encode(ring::digest::digest(
        &ring::digest::SHA256,
        canonical.as_bytes(),
    ))
}

async fn acme_handle(
    State(m): State<SharedAcme>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let mut reply = acme_route(&m, &method, &path, &body).await;
    // Every answer carries a nonce for the next request.
    let nonce = {
        let mut m = m.lock().unwrap();
        m.next_nonce += 1;
        let n = format!("nonce-{}", m.next_nonce);
        m.nonces.insert(n.clone());
        n
    };
    reply
        .headers_mut()
        .insert("replay-nonce", nonce.parse().unwrap());
    reply
}

async fn acme_route(m: &SharedAcme, method: &Method, path: &str, body: &[u8]) -> Response {
    let base = m.lock().unwrap().base.clone();
    if method == Method::GET && path == "/directory" {
        return axum::Json(json!({
            "newNonce": format!("{base}/nonce"),
            "newAccount": format!("{base}/new-acct"),
            "newOrder": format!("{base}/new-order"),
            "meta": { "profiles": { "shortlived": "six days", "classic": "ninety days" } },
        }))
        .into_response();
    }
    if method == Method::HEAD && path == "/nonce" {
        return StatusCode::OK.into_response();
    }
    if method != Method::POST {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    // ---- the signed envelope ----
    let jws: Value = serde_json::from_slice(body).unwrap();
    let part = |name: &str| jws[name].as_str().unwrap().to_string();
    let protected: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part("protected")).unwrap()).unwrap();
    assert_eq!(protected["alg"], "ES256");
    assert_eq!(protected["url"], format!("{base}{path}"), "url is signed");
    let jwk = {
        let mut m = m.lock().unwrap();
        let known = m.nonces.remove(protected["nonce"].as_str().unwrap());
        if !known || std::mem::take(&mut m.stale_nonce) {
            m.stale_nonces_sent += 1;
            return problem("badNonce", "JWS has an invalid anti-replay nonce");
        }
        if path == "/new-acct" {
            assert!(protected["kid"].is_null());
            protected["jwk"].clone()
        } else {
            assert_eq!(protected["kid"], format!("{base}/acct/1"));
            assert!(protected["jwk"].is_null());
            m.account_jwk.clone().expect("account first")
        }
    };
    let mut point = vec![0x04];
    for c in ["x", "y"] {
        point.extend(URL_SAFE_NO_PAD.decode(jwk[c].as_str().unwrap()).unwrap());
    }
    let signed = format!("{}.{}", part("protected"), part("payload"));
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &point)
        .verify(
            signed.as_bytes(),
            &URL_SAFE_NO_PAD.decode(part("signature")).unwrap(),
        )
        .expect("the request is signed by the account key");
    let payload: Option<Value> = match part("payload").as_str() {
        "" => None,
        p => Some(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(p).unwrap()).unwrap()),
    };

    // ---- resources ----
    let order_json = |m: &AcmeMock| {
        let mut o = json!({
            "status": m.order,
            "identifiers": m.last_order["identifiers"],
            "authorizations": [format!("{base}/authz/1")],
            "finalize": format!("{base}/finalize/1"),
        });
        if m.order == "valid" {
            o["certificate"] = json!(format!("{base}/cert/1"));
        }
        o
    };
    match path {
        "/new-acct" => {
            assert_eq!(payload.unwrap()["termsOfServiceAgreed"], true);
            m.lock().unwrap().account_jwk = Some(jwk);
            (
                StatusCode::CREATED,
                [("location", format!("{base}/acct/1"))],
                axum::Json(json!({ "status": "valid" })),
            )
                .into_response()
        }
        "/new-order" => {
            let mut m = m.lock().unwrap();
            m.orders += 1;
            m.last_order = payload.unwrap();
            m.authz = "pending";
            m.order = "pending";
            (
                StatusCode::CREATED,
                [("location", format!("{base}/order/1"))],
                axum::Json(order_json(&m)),
            )
                .into_response()
        }
        "/authz/1" => {
            assert!(payload.is_none(), "POST-as-GET");
            let m = m.lock().unwrap();
            let mut challenge = json!({
                "type": "http-01",
                "url": format!("{base}/chall/1"),
                "token": TOKEN_VALUE,
            });
            if m.authz == "invalid" {
                challenge["error"] = json!({ "detail": m.refuse });
            }
            axum::Json(json!({
                "status": m.authz,
                "identifier": m.last_order["identifiers"][0],
                // A real CA offers more than one way; the client must pick http-01.
                "challenges": [
                    { "type": "tls-alpn-01", "url": format!("{base}/chall/9"), "token": "x" },
                    challenge,
                ],
            }))
            .into_response()
        }
        "/chall/1" => {
            assert_eq!(payload, Some(json!({})));
            let (port, refuse) = {
                let m = m.lock().unwrap();
                (m.challenge_port, m.refuse.clone())
            };
            // What Let's Encrypt does: fetch the answer from the server's port 80.
            let url = format!("http://127.0.0.1:{port}/.well-known/acme-challenge/{TOKEN_VALUE}");
            let answer = reqwest::get(url).await.unwrap().text().await.unwrap();
            let right = answer == format!("{TOKEN_VALUE}.{}", thumbprint(&jwk));
            let mut m = m.lock().unwrap();
            if right && refuse.is_none() {
                m.authz = "valid";
                m.order = "ready";
            } else {
                m.refuse
                    .get_or_insert("The key authorization is wrong".into());
                m.authz = "invalid";
            }
            axum::Json(json!({ "status": "processing" })).into_response()
        }
        "/order/1" => axum::Json(order_json(&m.lock().unwrap())).into_response(),
        "/finalize/1" => {
            let csr = URL_SAFE_NO_PAD
                .decode(payload.unwrap()["csr"].as_str().unwrap())
                .unwrap();
            let (outer, _) = read(&csr);
            let (info, _) = read(outer);
            let (_version, rest) = read_raw(info);
            let (subject, rest) = read_raw(rest);
            let (spki, _) = read_raw(rest);
            assert_eq!(
                subject,
                [0x30, 0x00],
                "no subject: the address is a SAN only"
            );
            let san_ip = [0x87, 0x04, 127, 0, 0, 1];
            assert!(csr.windows(6).any(|w| w == san_ip), "iPAddress SAN");
            let mut m = m.lock().unwrap();
            assert_eq!(m.order, "ready");
            let days = m.first_days.take().unwrap_or(m.days);
            let leaf = TestCa::sign_dated(
                &m.ca.key,
                &[0x10 + m.orders as u8],
                &der_name("komiq test CA"),
                &seq(&[]),
                spki,
                &der_san_ip_extension("127.0.0.1".parse().unwrap()),
                (&utc_time(days.0), &utc_time(days.1)),
            );
            m.chain = format!("{}{}", pem("CERTIFICATE", &leaf), m.ca.pem());
            m.order = "valid";
            axum::Json(order_json(&m)).into_response()
        }
        "/cert/1" => m.lock().unwrap().chain.clone().into_response(),
        other => panic!("no ACME route for {other}"),
    }
}

/// The mock's directory URL and its state. `challenge_port` is the server's port 80.
async fn start_acme(challenge_port: u16) -> (String, SharedAcme) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let shared: SharedAcme = Arc::new(Mutex::new(AcmeMock {
        base: base.clone(),
        ca: TestCa::new(),
        challenge_port,
        nonces: HashSet::new(),
        next_nonce: 0,
        account_jwk: None,
        orders: 0,
        last_order: Value::Null,
        authz: "pending",
        order: "pending",
        chain: String::new(),
        stale_nonce: false,
        stale_nonces_sent: 0,
        refuse: None,
        days: (-1, 6),
        first_days: None,
    }));
    let app = Router::new()
        .fallback(acme_handle)
        .with_state(shared.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("{base}/directory"), shared)
}

struct Rig {
    mgr: Arc<Manager>,
    acme: SharedAcme,
    pool: Database,
    opts: Options,
    data: std::path::PathBuf,
}

async fn rig(name: &str) -> Rig {
    let dir = temp_dir(name);
    let mut opts = options("http://127.0.0.1:9", &dir, None);
    let (directory, acme) = start_acme(opts.acme_addr.port()).await;
    opts.acme_directory = directory;
    let pool = pool().await;
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    Rig {
        mgr: manager(&pool, opts.clone(), trusted),
        acme,
        pool,
        data: dir.join("data"),
        opts,
    }
}

/// A client that trusts only the mock's CA, so a request proves the served
/// certificate is valid for the address in the URL.
fn client(acme: &SharedAcme) -> reqwest::Client {
    let ca = reqwest::Certificate::from_pem(acme.lock().unwrap().ca.pem().as_bytes()).unwrap();
    reqwest::Client::builder()
        .tls_built_in_root_certs(false)
        .add_root_certificate(ca)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// HTTPS answers with a certificate the mock's CA signed. Waits a moment: after a
/// renewal the listener comes back just after the new files are on disk.
async fn https_answers(r: &Rig) -> bool {
    let http = client(&r.acme);
    let url = format!("https://127.0.0.1:{}/health", r.opts.https_addr.port());
    for _ in 0..200 {
        if http.get(&url).send().await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

fn port_is_free(addr: SocketAddr) -> bool {
    std::net::TcpListener::bind(addr).is_ok()
}

#[tokio::test]
async fn direct_mode_gets_a_certificate_and_serves_https() {
    let r = rig("direct").await;
    let (https, plain) = (r.opts.https_addr, r.opts.acme_addr);
    r.acme.lock().unwrap().stale_nonce = true;

    // A typed port is dropped: HTTPS has its own.
    r.mgr
        .set_access("direct", Some(" 127.0.0.1:8080 "), true)
        .await
        .unwrap();
    let st = r.mgr.status().await;
    assert_eq!(st.mode, "direct");
    assert_eq!(
        st.address,
        Some(format!("https://127.0.0.1:{}", https.port()))
    );
    assert!(st.https);
    assert!(st.certificate_error.is_none());
    let (_, until) = validity(&std::fs::read_to_string(r.data.join(DIRECT_CERT_FILE)).unwrap())
        .expect("dates are readable");
    assert_eq!(st.certificate_expires, Some(until.to_rfc3339()));
    {
        let m = r.acme.lock().unwrap();
        assert_eq!(m.orders, 1);
        assert_eq!(
            m.last_order,
            json!({
                "identifiers": [{ "type": "ip", "value": "127.0.0.1" }],
                "profile": "shortlived",
            })
        );
        assert_eq!(m.stale_nonces_sent, 1, "a stale nonce is retried");
    }
    for file in [DIRECT_KEY_FILE, DIRECT_CERT_FILE, "acme-account.der"] {
        assert_eq!(mode(&r.data.join(file)), 0o600, "{file}");
    }

    // A browser offers HTTP/2 first. The listener speaks HTTP/1.1, so the handshake
    // must settle on that: a client left on HTTP/2 gets its connection dropped.
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_rustls::rustls::{self, pki_types::pem::PemObject};
        let mut roots = rustls::RootCertStore::empty();
        let ca_pem = r.acme.lock().unwrap().ca.pem();
        for cert in rustls::pki_types::CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
            roots.add(cert.unwrap()).unwrap();
        }
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", https.port()))
            .await
            .unwrap();
        let mut browser = tokio_rustls::TlsConnector::from(Arc::new(tls))
            .connect("127.0.0.1".try_into().unwrap(), tcp)
            .await
            .unwrap();
        assert_eq!(browser.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
        browser
            .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut answer = [0u8; 12];
        browser.read_exact(&mut answer).await.unwrap();
        assert_eq!(&answer, b"HTTP/1.1 200");
    }

    // HTTPS answers with a certificate that is valid for the IP address.
    let http = client(&r.acme);
    let health = http
        .get(format!("https://127.0.0.1:{}/health", https.port()))
        .send()
        .await
        .expect("the certificate verifies for 127.0.0.1");
    assert_eq!(health.text().await.unwrap(), "ok");
    // Port 80 sends everything else to HTTPS, and keeps no answer after the check.
    let moved = http
        .get(format!("http://127.0.0.1:{}/library?tab=1", plain.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(moved.status(), 308);
    assert_eq!(
        moved.headers()["location"],
        format!("https://127.0.0.1:{}/library?tab=1", https.port())
    );
    let gone = http
        .get(format!(
            "http://127.0.0.1:{}/.well-known/acme-challenge/{TOKEN_VALUE}",
            plain.port()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 404);

    // The same address again keeps the certificate: no second request to the CA.
    r.mgr
        .set_access("direct", Some("https://127.0.0.1"), true)
        .await
        .unwrap();
    assert_eq!(r.acme.lock().unwrap().orders, 1);
    // Cloudflare's public-IP mode needs the same HTTPS port.
    let taken = r
        .mgr
        .start_setup(input("read.example.com", Connection::PublicIp))
        .await
        .unwrap_err();
    assert!(taken.contains("HTTPS at its own address"), "{taken}");

    // A restart serves the stored certificate again.
    r.mgr.stop_direct_tasks().await;
    r.mgr.stop_https().await;
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let again = manager(&r.pool, r.opts.clone(), trusted);
    again.resume().await;
    http.get(format!("https://127.0.0.1:{}/health", https.port()))
        .send()
        .await
        .expect("HTTPS is back after a restart");
    assert!(!port_is_free(plain));
    assert_eq!(r.acme.lock().unwrap().orders, 1);

    // Without encryption: plain HTTP as before, and both listeners are gone.
    again
        .set_access("direct", Some("203.0.113.5:8080"), false)
        .await
        .unwrap();
    let st = again.status().await;
    assert_eq!(st.address.as_deref(), Some("http://203.0.113.5:8080"));
    assert!(!st.https);
    assert!(st.certificate_expires.is_none());
    assert!(port_is_free(plain) && port_is_free(https));
    assert!(again.direct_tls_host().await.is_none());
}

#[tokio::test]
async fn a_certificate_that_is_half_gone_is_renewed() {
    let r = rig("renew").await;
    // Five of the first certificate's six days are gone: still valid, but due.
    r.acme.lock().unwrap().first_days = Some((-5, 1));
    r.mgr
        .set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    let first = (chrono::Utc::now() + chrono::Duration::days(2)).to_rfc3339();

    // The renewal loop looks every 50 ms here (every hour in production).
    let mut renewed = None;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let now = r.mgr.status().await.certificate_expires.unwrap();
        if now > first {
            renewed = Some(now);
            break;
        }
    }
    assert!(renewed.is_some(), "a new certificate replaced the old one");
    assert_eq!(r.acme.lock().unwrap().orders, 2);
    // The listener serves the new one, and a fresh certificate is left alone.
    assert!(https_answers(&r).await, "HTTPS serves the new certificate");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(r.acme.lock().unwrap().orders, 2);
    assert!(r.mgr.status().await.certificate_error.is_none());
}

#[tokio::test]
async fn a_refused_check_changes_nothing() {
    let r = rig("refused").await;
    let detail = "127.0.0.1: Fetching http://127.0.0.1/.well-known/acme-challenge/x: Timeout \
                  during connect (likely firewall problem)";
    r.acme.lock().unwrap().refuse = Some(detail.into());

    let e = r
        .mgr
        .set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap_err();
    assert!(e.contains(detail), "the CA's reason is shown: {e}");
    assert!(e.contains("port 80"), "{e}");
    let st = r.mgr.status().await;
    assert_eq!(st.mode, "", "the access choice is as it was");
    assert!(st.address.is_none() && !st.https);
    assert!(!r.data.join(DIRECT_CERT_FILE).exists());
    assert!(port_is_free(r.opts.acme_addr) && port_is_free(r.opts.https_addr));

    // The reason gone, the same call works.
    r.acme.lock().unwrap().refuse = None;
    r.mgr
        .set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    assert!(r.mgr.status().await.https);
}

#[test]
fn direct_hosts_are_public_addresses_or_names() {
    for (raw, host) in [
        ("203.0.113.5", "203.0.113.5"),
        (" 2606:4700:4700::1111 ", "2606:4700:4700::1111"),
        (" 203.0.113.5:8080/ ", "203.0.113.5"),
        ("https://203.0.113.5", "203.0.113.5"),
        ("[2001:db8::1]:8080", "2001:db8::1"),
        ("VPS.Example.net:7580", "vps.example.net"),
    ] {
        assert_eq!(direct_host(raw, false).as_deref(), Ok(host), "{raw}");
    }
    for bad in [
        "",
        "localhost",
        "127.0.0.1",
        "10.8.0.2:8080",
        "192.168.1.20",
        "[::1]",
        "::1",
        "fd00::1",
        "ftp://203.0.113.5",
        "not an address",
    ] {
        assert!(direct_host(bad, false).is_err(), "{bad}");
    }
    // A private certificate authority may sign for any address.
    assert_eq!(
        direct_host("10.8.0.2:8080", true).as_deref(),
        Ok("10.8.0.2")
    );
}

#[test]
fn a_request_for_an_ipv6_address_carries_its_16_bytes() {
    let key = super::super::origin_tls::generate_key().unwrap();
    let csr = acme_csr_der(&key, "2001:db8::1").unwrap();
    let ip: std::net::Ipv6Addr = "2001:db8::1".parse().unwrap();
    let mut san = vec![0x87, 0x10];
    san.extend(ip.octets());
    assert!(csr.windows(san.len()).any(|w| w == san));
    // A name is a dNSName instead.
    let csr = acme_csr_der(&key, "vps.example.net").unwrap();
    let mut san = vec![0x82, 15];
    san.extend(b"vps.example.net");
    assert!(csr.windows(san.len()).any(|w| w == san));
}

/// Stands in for the socket address: a test client is always on loopback.
async fn fake_peer(mut request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let peer = request
        .headers()
        .get("x-test-peer")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<std::net::IpAddr>().ok());
    if let Some(ip) = peer {
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(SocketAddr::new(ip, 50000)));
    }
    next.run(request).await
}

/// The public router as `main` builds it, in small: two routes behind the guard.
fn guarded(mgr: &Arc<Manager>) -> Router {
    Router::new()
        .route("/health", axum::routing::get(|| async { "ok" }))
        .route("/graphql", axum::routing::post(|| async { "handled" }))
        .layer(axum::middleware::from_fn_with_state(
            mgr.clone(),
            super::super::plain_http_guard,
        ))
        .layer(axum::middleware::from_fn(fake_peer))
}

#[tokio::test]
async fn plain_http_closes_to_the_internet_once_https_was_used() {
    use tower::ServiceExt;

    const STRANGER: &str = "198.51.100.7";
    let dir = temp_dir("guard");
    let mut opts = options("http://127.0.0.1:9", &dir, None);
    let (directory, acme) = start_acme(opts.acme_addr.port()).await;
    opts.acme_directory = directory;
    let https = format!("https://127.0.0.1:{}", opts.https_addr.port());
    let pool = pool().await;
    let build = || {
        let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
        let mgr = Arc::new(Manager::new(pool.clone(), opts.clone(), trusted));
        let router = guarded(&mgr);
        mgr.set_router(router.clone());
        (mgr, router)
    };
    let (mgr, router) = build();
    // A request over plain HTTP, as the plain listener would hand it to the router.
    let plain = |router: &Router, method: Method, path: &str, peer: &str, token: Option<&str>| {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("x-test-peer", peer);
        if let Some(t) = token {
            request = request.header("authorization", format!("Bearer {t}"));
        }
        router
            .clone()
            .oneshot(request.body(axum::body::Body::empty()).unwrap())
    };
    let text = |r: Response| async {
        let bytes = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    };

    mgr.set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    // HTTPS is on, but nobody on the internet has used it: port 443 may be closed in
    // a firewall, so the plain port still answers and nobody is locked out.
    let r = plain(&router, Method::GET, "/health", STRANGER, None)
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!mgr.status().await.plain_http_closed);
    // A visit over HTTPS from the home network proves nothing about the internet.
    let http = client(&acme);
    for (peer, closes) in [("192.168.1.9", false), (STRANGER, true)] {
        http.get(format!("{https}/health"))
            .header("x-test-peer", peer)
            .send()
            .await
            .unwrap();
        assert_eq!(mgr.status().await.plain_http_closed, closes, "{peer}");
    }

    // From the internet, plain HTTP is now never handled.
    let r = plain(&router, Method::GET, "/health?x=1", STRANGER, None)
        .await
        .unwrap();
    assert_eq!(r.status(), 308);
    assert_eq!(r.headers()["location"], format!("{https}/health?x=1"));
    // A request that carries a session token is not sent along: it is refused.
    let r = plain(&router, Method::GET, "/health", STRANGER, Some("tok"))
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    assert!(text(r).await.contains(&https));
    // Sign-in and every other GraphQL call: an error the apps show, not the handler.
    let r = plain(&router, Method::POST, "/graphql", STRANGER, None)
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = serde_json::from_str(&text(r).await).unwrap();
    assert!(body["data"].is_null());
    assert_eq!(
        body["errors"][0]["message"],
        format!("This server uses HTTPS. Use {https} instead of this address.")
    );
    // An IPv4 client seen through an IPv6 socket is the same client.
    let r = plain(
        &router,
        Method::POST,
        "/graphql",
        "::ffff:198.51.100.7",
        None,
    )
    .await
    .unwrap();
    assert_ne!(text(r).await, "handled");
    // The home network, a VPN and this computer keep the plain port.
    for peer in [
        "192.168.1.9",
        "10.8.0.2",
        "100.100.4.4",
        "127.0.0.1",
        "fe80::1",
        "fd00::9",
    ] {
        let r = plain(&router, Method::POST, "/graphql", peer, None)
            .await
            .unwrap();
        assert_eq!(text(r).await, "handled", "{peer}");
    }

    // A restart keeps it closed.
    for _ in 0..100 {
        if mgr.https_seen_stored().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    mgr.stop_direct_tasks().await;
    mgr.stop_https().await;
    let (mgr, router) = build();
    mgr.resume().await;
    assert!(mgr.status().await.plain_http_closed);
    let r = plain(&router, Method::POST, "/graphql", STRANGER, None)
        .await
        .unwrap();
    assert_ne!(text(r).await, "handled");

    // Public without HTTPS opens the plain port again, and a later return starts over.
    // ("Only my devices" keeps it closed to the internet: see
    // `only_my_devices_refuses_plain_http_from_the_internet`.)
    mgr.set_access("direct", Some("203.0.113.5:8080"), false)
        .await
        .unwrap();
    let r = plain(&router, Method::POST, "/graphql", STRANGER, None)
        .await
        .unwrap();
    assert_eq!(text(r).await, "handled");
    mgr.set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    assert!(!mgr.status().await.plain_http_closed);

    // Behind Docker Desktop or rootless Docker every peer is Docker's own private
    // address. There the address the client typed says who is on the internet.
    const DOCKER: &str = "172.17.0.1";
    let typed = |host: &str, over_tls: bool| {
        let mut request = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/graphql")
            .header("x-test-peer", DOCKER)
            .header("host", host)
            .body(axum::body::Body::empty())
            .unwrap();
        if over_tls {
            request
                .extensions_mut()
                .insert(super::super::origin_tls::OverTls);
        }
        router.clone().oneshot(request)
    };
    // HTTPS to a home address proves nothing about the internet. To a public one it does.
    typed("192.168.1.20", true).await.unwrap();
    assert!(!mgr.status().await.plain_http_closed);
    typed("203.0.113.5", true).await.unwrap();
    assert!(mgr.status().await.plain_http_closed);
    let r = typed("203.0.113.5:8080", false).await.unwrap();
    assert_ne!(text(r).await, "handled");
    for home in ["192.168.1.20:8080", "localhost:8080", "komiq.local:8080"] {
        let r = typed(home, false).await.unwrap();
        assert_eq!(text(r).await, "handled", "{home}");
    }
}

/// `Host` as a browser sends it: with a port, with brackets, in any case.
#[test]
fn the_typed_address_says_where_a_client_is() {
    use super::super::{typed_host, TypedHost};
    for local in [
        "localhost",
        "localhost:8080",
        "127.0.0.1:8080",
        "192.168.1.20:8080",
        "10.0.0.4",
        "100.64.0.9:8080",
        "[::1]:8080",
        "[fd00::9]",
        "server",
        "server:8080",
        "komiq.local",
        "komiq.lan:8080",
        "Box.Home.Arpa",
        "nas.internal",
        "",
    ] {
        assert_eq!(typed_host(local), TypedHost::Local, "{local}");
    }
    for public in ["203.0.113.5", "203.0.113.5:8080", "[2001:db8::1]:8080"] {
        assert_eq!(typed_host(public), TypedHost::Public, "{public}");
    }
    for name in ["read.example.com", "Read.Example.COM.:8080"] {
        assert_eq!(
            typed_host(name),
            TypedHost::Name("read.example.com".into()),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_typed_name_is_public_unless_it_leads_to_a_private_address() {
    use super::super::name_on_internet;
    assert!(!name_on_internet("localhost").await);
    // `.invalid` never resolves: the server cannot tell, so it does not trust it.
    assert!(name_on_internet("komiq-setup-test.invalid").await);
}

#[test]
fn https_in_front_is_a_proxy_header_or_a_same_origin_https_page() {
    use super::super::https_in_front;
    let judge = |pairs: &[(&'static str, &str)]| {
        let mut map = axum::http::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        https_in_front(&map)
    };
    assert!(judge(&[("x-forwarded-proto", "https")]));
    assert!(judge(&[("x-forwarded-proto", "HTTPS, http")]));
    assert!(judge(&[
        ("host", "read.example.com:443"),
        ("origin", "https://read.example.com")
    ]));
    assert!(!judge(&[("x-forwarded-proto", "http")]));
    assert!(!judge(&[
        ("host", "read.example.com"),
        ("origin", "http://read.example.com")
    ]));
    // Another origin's page says nothing about this connection.
    assert!(!judge(&[
        ("host", "203.0.113.5:8080"),
        ("origin", "https://tauri.localhost")
    ]));
    assert!(!judge(&[]));
}

#[tokio::test]
async fn a_key_that_does_not_fit_the_certificate_is_replaced() {
    let r = rig("mismatch").await;
    r.mgr
        .set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    assert_eq!(r.acme.lock().unwrap().orders, 1);
    // A crash between the two writes: a new key next to the old certificate. The
    // certificate's dates still look fine, but the pair cannot be served.
    let stray = super::super::origin_tls::generate_key().unwrap();
    std::fs::write(r.data.join(DIRECT_KEY_FILE), stray).unwrap();
    for _ in 0..200 {
        if r.acme.lock().unwrap().orders == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(r.acme.lock().unwrap().orders, 2, "a new pair was requested");
    let served = https_answers(&r).await;
    assert!(served, "HTTPS serves the new pair");
}

#[test]
fn two_digit_years_follow_the_x509_rule() {
    let ca = TestCa::new();
    let spki = super::super::origin_tls::der_spki(ca.key.public_key().as_ref());
    let name = der_name("dates");
    let ext = der_san_ip_extension("127.0.0.1".parse().unwrap());
    let cert = TestCa::sign_dated(
        &ca.key,
        &[0x05],
        &name,
        &name,
        &spki,
        &ext,
        ("500101000000Z", "491231235959Z"),
    );
    let (from, until) = validity(&pem("CERTIFICATE", &cert)).unwrap();
    assert_eq!(from.to_rfc3339(), "1950-01-01T00:00:00+00:00");
    assert_eq!(until.to_rfc3339(), "2049-12-31T23:59:59+00:00");
}

#[tokio::test]
async fn direct_https_can_take_over_a_paused_origin_listener() {
    let r = rig("origin-to-direct").await;
    let mut mock = base_mock();
    mock.ca = Some(Arc::new(TestCa::new()));
    let ca_pem = mock.ca.as_ref().unwrap().pem();
    let (api, mock) = start_mock(mock).await;
    let mut opts = r.opts.clone();
    opts.api_base = api.clone();
    opts.trace_url = format!("{api}/cdn-cgi/trace");
    opts.health_base = Some(api);
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let mgr = manager(&r.pool, opts.clone(), trusted.clone());
    mgr.start_setup(input("read.example.com", Connection::PublicIp))
        .await
        .unwrap();
    all_done(&wait_job(&mgr).await);
    let stored = load_stored(&r.pool).await.unwrap();
    let origin_cert = std::fs::read(r.data.join(ORIGIN_CERT_FILE)).unwrap();
    let proxies = trusted.raw();

    mgr.set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    let direct_client = client(&r.acme);
    let url = format!("https://127.0.0.1:{}/health", opts.https_addr.port());
    assert_eq!(direct_client.get(&url).send().await.unwrap().status(), 200);
    assert_eq!(load_stored(&r.pool).await.unwrap(), stored);
    assert_eq!(
        std::fs::read(r.data.join(ORIGIN_CERT_FILE)).unwrap(),
        origin_cert
    );
    assert_eq!(trusted.raw(), proxies);

    mgr.stop_direct_tasks().await;
    mgr.stop_https().await;
    let again = manager(&r.pool, opts.clone(), trusted);
    again.resume().await;
    assert_eq!(direct_client.get(&url).send().await.unwrap().status(), 200);
    again
        .set_access("internet", Some("read.example.com"), false)
        .await
        .unwrap();
    assert!(port_is_free(opts.acme_addr));
    let origin_client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .resolve("read.example.com", opts.https_addr)
        .no_proxy()
        .build()
        .unwrap();
    let url = format!("https://read.example.com:{}/health", opts.https_addr.port());
    assert_eq!(origin_client.get(url).send().await.unwrap().status(), 200);
    again.set_access("local", None, false).await.unwrap();
    assert!(port_is_free(opts.https_addr));
    again.start_remove(TOKEN).await.unwrap();
    all_done(&wait_job(&again).await);
    assert_clean(&mock.lock().unwrap(), true);
}

/// The mock's certificate for the key on disk, but for another address: what a crash
/// in the middle of an address change leaves next to settings that name the old one.
fn write_certificate_for_another_address(r: &Rig) {
    let key = std::fs::read(r.data.join(DIRECT_KEY_FILE)).unwrap();
    let acme = r.acme.lock().unwrap();
    let other = der_san_ip_extension("203.0.113.9".parse().unwrap());
    let chain = format!(
        "{}{}",
        acme.ca.sign_for_key(&key, &other, (-1, 6)),
        acme.ca.pem()
    );
    std::fs::write(r.data.join(DIRECT_CERT_FILE), chain).unwrap();
}

fn names_on_disk(r: &Rig) -> Vec<String> {
    names(&std::fs::read_to_string(r.data.join(DIRECT_CERT_FILE)).unwrap())
}

async fn wait_for_orders(r: &Rig, orders: u32) {
    for _ in 0..200 {
        if r.acme.lock().unwrap().orders >= orders {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(r.acme.lock().unwrap().orders, orders);
}

#[test]
fn a_certificate_says_what_it_is_for() {
    let ca = TestCa::new();
    let key = super::super::origin_tls::generate_key().unwrap();
    for (host, san) in [
        (
            "203.0.113.5",
            der_san_ip_extension("203.0.113.5".parse().unwrap()),
        ),
        (
            "2001:db8::1",
            der_san_ip_extension("2001:db8::1".parse().unwrap()),
        ),
        ("read.example.com", der_san_extension("Read.Example.com")),
    ] {
        let cert = ca.sign_for_key(&key, &san, (-1, 6));
        assert_eq!(names(&cert), [host]);
        assert!(super::super::origin_tls::is_for(&cert, host));
        assert!(!super::super::origin_tls::is_for(&cert, "203.0.113.9"));
    }
    // Other extensions come first, and the names can be marked critical.
    let basic = seq(&[&tlv(0x06, &[0x55, 0x1D, 0x13]), &tlv(0x04, &seq(&[]))]);
    let critical = seq(&[
        &tlv(0x06, &[0x55, 0x1D, 0x11]),
        &[0x01, 0x01, 0xFF],
        &tlv(0x04, &seq(&[&tlv(0x87, &[203, 0, 113, 5])])),
    ]);
    let cert = ca.sign_for_key(&key, &[basic, critical].concat(), (-1, 6));
    assert_eq!(names(&cert), ["203.0.113.5"]);
    // No names, or no certificate: for nothing.
    let none = ca.sign_for_key(
        &key,
        &seq(&[&tlv(0x06, &[0x55, 0x1D, 0x13]), &tlv(0x04, &seq(&[]))]),
        (-1, 6),
    );
    assert!(names(&none).is_empty());
    assert!(names("not a certificate").is_empty());
}

#[tokio::test]
async fn a_certificate_for_another_address_is_replaced() {
    let r = rig("other-address").await;
    // Only the first look of the renewal loop runs here: a change is seen by the code
    // under test, not by a later look.
    let mut opts = r.opts.clone();
    opts.renew_check = Duration::from_secs(3600);
    let trusted = || Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let mgr = manager(&r.pool, opts.clone(), trusted());
    mgr.set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    assert_eq!(r.acme.lock().unwrap().orders, 1);
    assert_eq!(names_on_disk(&r), ["127.0.0.1"]);

    // Its key fits and its dates are fine, so only its name tells it is the wrong one.
    // Saving the same address does not keep it.
    write_certificate_for_another_address(&r);
    mgr.set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    assert_eq!(r.acme.lock().unwrap().orders, 2, "a new one was requested");
    assert_eq!(names_on_disk(&r), ["127.0.0.1"]);

    // A restart finds it: the server gets one for the address in its settings, and
    // serves that.
    mgr.stop_direct_tasks().await;
    mgr.stop_https().await;
    write_certificate_for_another_address(&r);
    let again = manager(&r.pool, opts, trusted());
    // It is never served, not even until the new one is there.
    let refused = again.serve_direct("127.0.0.1").await.unwrap_err();
    assert!(refused.contains("is not for 127.0.0.1"), "{refused}");
    assert!(!again.https_up());
    again.resume().await;
    wait_for_orders(&r, 3).await;
    assert!(https_answers(&r).await, "HTTPS serves one for 127.0.0.1");
    assert_eq!(names_on_disk(&r), ["127.0.0.1"]);
    let st = again.status().await;
    assert!(st.https && st.https_error.is_none() && st.certificate_error.is_none());
}

#[tokio::test]
async fn the_address_names_the_https_port_of_this_run() {
    let r = rig("port-change").await;
    r.mgr
        .set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    let st = r.mgr.status().await;
    assert_eq!(
        st.address,
        Some(format!("https://127.0.0.1:{}", r.opts.https_addr.port()))
    );
    r.mgr.stop_direct_tasks().await;
    r.mgr.stop_https().await;

    // A restart with another KOMIKA_HTTPS_PORT: readers are sent to the new port.
    let mut opts = r.opts.clone();
    opts.https_addr.set_port(free_port());
    let moved = format!("https://127.0.0.1:{}", opts.https_addr.port());
    let trusted = Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let again = manager(&r.pool, opts.clone(), trusted);
    again.resume().await;
    assert_eq!(again.status().await.address, Some(moved.clone()));
    let http = client(&r.acme);
    http.get(format!("{moved}/health"))
        .send()
        .await
        .expect("HTTPS answers on the new port");
    let sent = http
        .get(format!(
            "http://127.0.0.1:{}/library",
            opts.acme_addr.port()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(sent.headers()["location"], format!("{moved}/library"));
}

#[tokio::test]
async fn direct_https_comes_back_after_a_busy_port_at_boot() {
    let r = rig("busy-direct").await;
    // The renewal loop also restarts a listener that is down, once an hour. Not here:
    // what brings HTTPS back is the retry.
    let mut opts = r.opts.clone();
    opts.renew_check = Duration::from_secs(3600);
    let trusted = || Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let mgr = manager(&r.pool, opts.clone(), trusted());
    mgr.set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    // This also lets the renewal loop finish its first look. Stopped in the middle of
    // a query, it would take the test's one in-memory database connection with it.
    assert!(mgr.status().await.https);
    mgr.stop_direct_tasks().await;
    mgr.stop_https().await;

    let other_program = std::net::TcpListener::bind(opts.https_addr).unwrap();
    let again = manager(&r.pool, opts.clone(), trusted());
    again.resume().await;
    let st = again.status().await;
    assert!(!st.https);
    let why = st.https_error.expect("the console can show why");
    assert!(
        why.contains(&format!("port {}", opts.https_addr.port())),
        "{why}"
    );
    assert!(st.certificate_error.is_none(), "the certificate is fine");

    drop(other_program);
    assert!(https_answers(&r).await, "HTTPS is back by itself");
    let st = again.status().await;
    assert!(st.https && st.https_error.is_none() && st.certificate_error.is_none());
    assert_eq!(
        r.acme.lock().unwrap().orders,
        1,
        "with the same certificate"
    );
}

#[tokio::test]
async fn only_my_devices_refuses_plain_http_from_the_internet() {
    use tower::ServiceExt;

    const STRANGER: &str = "198.51.100.7";
    const DOCKER: &str = "172.17.0.1";
    let dir = temp_dir("local-only");
    let opts = options("http://127.0.0.1:9", &dir, None);
    let pool = pool().await;
    // One proxy of the owner's is trusted: what it passes on is its business.
    let trusted = || {
        Arc::new(crate::TrustedProxies::for_tests(
            vec!["203.0.113.80/32".into()],
            true,
        ))
    };
    let mgr = Arc::new(Manager::new(pool.clone(), opts.clone(), trusted()));
    let router = guarded(&mgr);
    mgr.set_router(router.clone());
    // (method, path, peer, extra headers) as the plain listener hands it to the router.
    let send =
        |router: &Router, method: Method, path: &str, peer: &str, headers: &[(&str, &str)]| {
            let mut request = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("x-test-peer", peer);
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            router
                .clone()
                .oneshot(request.body(axum::body::Body::empty()).unwrap())
        };
    let status = |router: &Router, peer: &str, headers: &[(&str, &str)]| {
        let sent = send(router, Method::GET, "/health", peer, headers);
        async { sent.await.unwrap().status().as_u16() }
    };

    // Nobody chose yet: the first-run screens must open from anywhere.
    assert_eq!(status(&router, STRANGER, &[]).await, 200);
    // The public modes without HTTPS of the server's own are left alone too.
    mgr.set_access("direct", Some("203.0.113.5:8080"), false)
        .await
        .unwrap();
    assert_eq!(status(&router, STRANGER, &[]).await, 200);

    mgr.set_access("local", None, false).await.unwrap();
    // A public peer is the internet, whatever it says about itself.
    for headers in [
        &[][..],
        &[("host", "192.168.1.20:8080")],
        &[("x-forwarded-proto", "https")],
    ] {
        assert_eq!(status(&router, STRANGER, headers).await, 403, "{headers:?}");
    }
    let r = send(&router, Method::GET, "/library", STRANGER, &[])
        .await
        .unwrap();
    let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        body.contains("Only my devices") && body.contains("komika-server setup"),
        "{body}"
    );
    // Sign-in and every other GraphQL call: an error the apps show, not the handler.
    let r = send(&router, Method::POST, "/graphql", STRANGER, &[])
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert!(body["errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("Only my devices"));

    // This computer, the home network and a VPN are the owner's devices.
    for peer in [
        "127.0.0.1",
        "192.168.1.9",
        "10.0.0.4",
        "100.101.102.103",
        "fd00::9",
    ] {
        assert_eq!(
            status(&router, peer, &[("host", "192.168.1.20:8080")]).await,
            200,
            "{peer}"
        );
    }
    // Behind Docker Desktop every peer is Docker's: the address the client typed says
    // where it is. A public one is the internet, unless HTTPS ended in front.
    for (headers, answer) in [
        (&[("host", "komiq.lan:8080")][..], 200),
        (&[("host", "192.168.1.20:8080")], 200),
        (&[("host", "203.0.113.5:8080")], 403),
        (
            &[("host", "203.0.113.5"), ("x-forwarded-proto", "https")],
            200,
        ),
        // A name that leads nowhere counts as public; `localhost` is this computer.
        (&[("host", "no-such-host.invalid:8080")], 403),
    ] {
        assert_eq!(
            status(&router, DOCKER, headers).await,
            answer,
            "{headers:?}"
        );
    }
    // The owner's own proxy.
    assert_eq!(status(&router, "203.0.113.80", &[]).await, 200);

    // A restart keeps the choice.
    let again = Arc::new(Manager::new(pool.clone(), opts.clone(), trusted()));
    let router = guarded(&again);
    again.set_router(router.clone());
    again.resume().await;
    assert_eq!(status(&router, STRANGER, &[]).await, 403);
    assert_eq!(status(&router, "192.168.1.9", &[]).await, 200);
    // And a public mode opens the port again.
    again
        .set_access("direct", Some("203.0.113.5:8080"), false)
        .await
        .unwrap();
    assert_eq!(status(&router, STRANGER, &[]).await, 200);
}

#[tokio::test]
async fn a_renewal_that_waited_for_a_change_of_address_follows_it() {
    let r = rig("renew-after-change").await;
    r.mgr
        .set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    assert!(r.mgr.status().await.https);

    // A change of address holds the lock, as `set_access` does, and the renewal loop
    // comes for its look meanwhile (every 50 ms here).
    let change = r.mgr.issuing.lock().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    // What the change leaves: another address in the settings, with its certificate.
    write_certificate_for_another_address(&r);
    instance::store(&r.pool, &instance::DIRECT_TLS_HOST, "203.0.113.9")
        .await
        .unwrap();
    drop(change);

    // Several looks later, the certificate is still the one for that address.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        r.acme.lock().unwrap().orders,
        1,
        "none for the address that was left"
    );
    assert_eq!(names_on_disk(&r), ["203.0.113.9"]);
}

#[tokio::test]
async fn a_restart_finishes_a_change_of_mode_that_a_crash_cut_short() {
    use tower::ServiceExt;

    let r = rig("crash-to-local").await;
    let mut opts = r.opts.clone();
    opts.renew_check = Duration::from_secs(3600);
    let trusted = || Arc::new(crate::TrustedProxies::for_tests(vec![], false));
    let mgr = manager(&r.pool, opts.clone(), trusted());
    mgr.set_access("direct", Some("127.0.0.1"), true)
        .await
        .unwrap();
    // (Why this line: see `direct_https_comes_back_after_a_busy_port_at_boot`.)
    assert!(mgr.status().await.https);
    mgr.stop_direct_tasks().await;
    mgr.stop_https().await;
    // The owner chose "Only my devices". The server stopped after it stored the choice
    // and before it turned HTTPS off.
    instance::store(&r.pool, &instance::ACCESS_MODE, "local")
        .await
        .unwrap();

    let again = Arc::new(Manager::new(r.pool.clone(), opts.clone(), trusted()));
    let router = guarded(&again);
    again.set_router(router.clone());
    again.resume().await;
    let st = again.status().await;
    assert_eq!(st.mode, "local");
    assert!(!st.https && st.https_error.is_none());
    assert!(again.direct_tls_host().await.is_none());
    assert!(port_is_free(opts.https_addr) && port_is_free(opts.acme_addr));
    // The stored choice acts: plain HTTP refuses the internet.
    let stranger = axum::http::Request::builder()
        .uri("/health")
        .header("x-test-peer", "198.51.100.7")
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(router.oneshot(stranger).await.unwrap().status(), 403);
}
