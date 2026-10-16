use super::*;
use axum::http::{header, Request as HttpRequest};
use axum::routing::get;
use futures::stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tower::ServiceExt;

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("komika-fallback-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn replace(path: &std::path::Path, bytes: impl AsRef<[u8]>) {
    let modified = std::fs::metadata(path).unwrap().modified().unwrap();
    let replacement = path.with_extension("next");
    std::fs::write(&replacement, bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    std::fs::rename(replacement, path).unwrap();
}

fn reserve(limit: u64) -> (Arc<Reserve>, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(10 * 86_400 + 42));
    let clock = now.clone();
    (
        Arc::new(Reserve::new(limit, Arc::new(move || clock.load(Relaxed)))),
        now,
    )
}

fn used(reserve: &Reserve) -> u64 {
    reserve.usage().1.bytes.load(Relaxed)
}

fn stub(calls: Arc<AtomicU64>) -> Router {
    Router::new().fallback(move || {
        calls.fetch_add(1, Relaxed);
        async {
            Body::from_stream(stream::iter(
                ["abc", "defg", "hi"]
                    .map(|s| Ok::<_, std::io::Error>(Bytes::from_static(s.as_bytes()))),
            ))
        }
    })
}

fn app(reserve: Arc<Reserve>, calls: Arc<AtomicU64>) -> Router {
    protect(
        stub(calls),
        &[
            "reader.example".into(),
            "second.example".into(),
            "[::1]".into(),
            "explicit.example:9443".into(),
            "https.example:443".into(),
        ],
        8443,
        reserve,
    )
}

fn request(method: &str, uri: &str, host: Option<&str>) -> Request {
    let mut req = HttpRequest::builder().method(method).uri(uri);
    if let Some(host) = host {
        req = req.header(HOST, host);
    }
    req.body(Body::empty()).unwrap()
}

async fn bytes(response: Response) -> Bytes {
    axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap()
}

#[tokio::test]
async fn fallback_marker_reaches_inner_layers() {
    let (reserve, _) = reserve(100);
    let inner = Router::new().route(
        "/health",
        get(|axum::Extension(_): axum::Extension<FallbackListener>| async { "ok" }),
    );
    let app = protect(inner, &["reader.example".into()], 8443, reserve);
    let response = app
        .oneshot(request("GET", "/health", Some("reader.example")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, "ok");
}

#[tokio::test]
async fn authority_guard_table_and_inner_is_never_called_on_rejection() {
    let (reserve, _) = reserve(100);
    let calls = Arc::new(AtomicU64::new(0));
    let app = app(reserve.clone(), calls.clone());
    for (host, uri, allowed) in [
        (Some("reader.example"), "/", true),
        (Some("reader.example:8443"), "/", true),
        (Some("reader.example:443"), "/", true),
        (Some("reader.example:444"), "/", false),
        (Some("reader.example."), "/", true),
        (Some("READER.EXAMPLE.:8443"), "/", true),
        (Some("evil.example"), "/media/test", false),
        (None, "/", false),
        (Some("reader.example"), "https://evil.example/", false),
        (Some("reader.example"), "https://second.example/", false),
        (Some("reader.example"), "https://READER.EXAMPLE.:443/", true),
        (
            Some("reader.example:8443"),
            "https://reader.example:8443/",
            true,
        ),
        (
            Some("reader.example:8443"),
            "https://reader.example:443/",
            false,
        ),
        (Some("[::1]"), "/", true),
        (Some("[0:0:0:0:0:0:0:1]:8443"), "/", true),
        (Some("[::1]:443"), "/", true),
        (Some("[::1]:444"), "/", false),
        (Some("explicit.example:9443"), "/", true),
        (Some("explicit.example"), "/", false),
        (Some("explicit.example:443"), "/", false),
        (Some("https.example"), "/", true),
        (Some("https.example:443"), "/", true),
        (Some("https.example:8443"), "/", false),
        (Some("https.example"), "https://https.example:443/", true),
        (Some("https.example:443"), "https://https.example/", true),
        (Some("reader.example/path"), "/", false),
        (Some("reader.example:bad"), "/", false),
        (Some("user@reader.example"), "/", false),
        (Some("reader .example"), "/", false),
    ] {
        let before = calls.load(Relaxed);
        let response = app
            .clone()
            .oneshot(request("GET", uri, host))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if allowed {
                StatusCode::OK
            } else {
                StatusCode::MISDIRECTED_REQUEST
            },
            "{host:?} {uri}"
        );
        assert_eq!(calls.load(Relaxed), before + u64::from(allowed));
        if !allowed {
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
            assert!(response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/plain"));
        }
    }
    let mut req = request("GET", "/media/test", Some("reader.example"));
    req.headers_mut()
        .append(HOST, "second.example".parse().unwrap());
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::MISDIRECTED_REQUEST
    );
    assert_eq!(used(&reserve), 0);
}

#[tokio::test]
async fn reserve_counts_streamed_frames_and_refuses_before_calling_inner() {
    let (reserve, _) = reserve(9);
    let calls = Arc::new(AtomicU64::new(0));
    let app = app(reserve.clone(), calls.clone());
    let response = app
        .clone()
        .oneshot(request("GET", "/media/test", Some("reader.example")))
        .await
        .unwrap();
    assert_eq!(used(&reserve), 0, "only polled bytes count");
    assert_eq!(bytes(response).await, "abcdefghi");
    assert_eq!(used(&reserve), 9);
    let response = app
        .oneshot(request("GET", "/covers/test", Some("reader.example")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[RETRY_AFTER], (86_400 - 42).to_string());
    assert_eq!(calls.load(Relaxed), 1);
    bytes(response).await;
    assert_eq!(used(&reserve), 9, "503 body is not media");
    assert!(reserve.usage().1.warned.load(Relaxed));
}

#[tokio::test]
async fn reserve_counts_zero_for_head_and_a_dropped_body() {
    let (reserve, _) = reserve(100);
    let calls = Arc::new(AtomicU64::new(0));
    let app = app(reserve.clone(), calls);
    let response = app
        .clone()
        .oneshot(request("HEAD", "/img", Some("reader.example")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(bytes(response).await.is_empty());
    drop(
        app.oneshot(request("GET", "/img", Some("reader.example")))
            .await
            .unwrap(),
    );
    assert_eq!(used(&reserve), 0);
}

#[tokio::test]
async fn reserve_counts_after_compression() {
    use tower_http::compression::CompressionLayer;
    let (reserve, _) = reserve(100_000);
    let inner = Router::new()
        .fallback(|| async {
            (
                [(header::CONTENT_TYPE, "text/plain")],
                "compressible ".repeat(4096),
            )
        })
        .layer(CompressionLayer::new());
    let app = protect(inner, &["reader.example".into()], 8443, reserve.clone());
    let mut req = request("GET", "/media/test", Some("reader.example"));
    req.headers_mut()
        .insert(header::ACCEPT_ENCODING, "gzip".parse().unwrap());
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    let body = bytes(response).await;
    assert!(body.len() < 4096);
    assert_eq!(used(&reserve), body.len() as u64);
}

#[tokio::test]
async fn reserve_is_shared_across_claimed_hosts_and_authority_guard_is_outermost() {
    let (reserve, _) = reserve(9);
    let calls = Arc::new(AtomicU64::new(0));
    let app = app(reserve.clone(), calls.clone());
    bytes(
        app.clone()
            .oneshot(request("GET", "/img", Some("reader.example")))
            .await
            .unwrap(),
    )
    .await;
    let response = app
        .clone()
        .oneshot(request("GET", "/img", Some("second.example")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(calls.load(Relaxed), 1);
    assert_eq!(
        app.oneshot(request("GET", "/img", Some("evil.example")))
            .await
            .unwrap()
            .status(),
        StatusCode::MISDIRECTED_REQUEST
    );
    assert_eq!(used(&reserve), 9);
}

#[tokio::test]
async fn reserve_media_classification_and_html_api_assets_survive_zero_limit() {
    let (reserve, _) = reserve(0);
    let calls = Arc::new(AtomicU64::new(0));
    let app = app(reserve, calls.clone());
    for path in [
        "/covers/a",
        "/avatars/a",
        "/comment-media/a",
        "/ext-icons/a",
        "/api/v1/manga/1/thumbnail",
        "/api/v1/manga/1/chapter/2/page/3",
        "/media/v1/c/a/1",
        "/media/v1/o/a/hash",
        "/media/v1/manifest/a",
        "/media/v1/keys",
        "/media/future",
        "/img?url=x",
    ] {
        assert_eq!(
            app.clone()
                .oneshot(request("GET", path, Some("reader.example")))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{path}"
        );
    }
    assert_eq!(calls.load(Relaxed), 0);
    for path in [
        "/",
        "/graphql",
        "/_app/x.js",
        "/health",
        "/img/other",
        "/avatar",
        "/comment-media",
    ] {
        assert_eq!(
            app.clone()
                .oneshot(request("GET", path, Some("reader.example")))
                .await
                .unwrap()
                .status(),
            StatusCode::OK,
            "{path}"
        );
    }
    assert_eq!(calls.load(Relaxed), 7);
}

#[tokio::test]
async fn reserve_utc_day_rollover_and_streaming_overshoot() {
    let (reserve, now) = reserve(1);
    let app = app(reserve.clone(), Arc::new(AtomicU64::new(0)));
    let first = app
        .clone()
        .oneshot(request("GET", "/img", Some("reader.example")))
        .await
        .unwrap();
    let second = app
        .clone()
        .oneshot(request("GET", "/img", Some("reader.example")))
        .await
        .unwrap();
    bytes(first).await;
    bytes(second).await;
    assert_eq!(
        used(&reserve),
        18,
        "in-flight responses finish past the cap"
    );
    assert!(reserve.retry_after().is_some());
    now.store(11 * 86_400, Relaxed);
    assert_eq!(used(&reserve), 0);
    assert!(!reserve.usage().1.warned.load(Relaxed));
    assert!(reserve.retry_after().is_none());
    let response = app
        .clone()
        .oneshot(request("GET", "/img", Some("reader.example")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    bytes(response).await;
    let response = app
        .oneshot(request("GET", "/img", Some("reader.example")))
        .await
        .unwrap();
    assert_eq!(response.headers()[RETRY_AFTER], "86400");
}

fn fallback_config(dir: &TempDir) -> FallbackConfig {
    FallbackConfig {
        cert_file: dir.0.join("cert.pem"),
        key_file: dir.0.join("key.pem"),
        hosts: vec!["localhost".into(), "reader.example".into()],
        port: 8443,
        bind_addr: "127.0.0.1".parse().unwrap(),
        reader_dir: dir.0.join("reader"),
        media_bytes_per_day: 9,
    }
}

#[tokio::test]
async fn reader_only_mount_preserves_api_and_never_serves_admin_index() {
    let dir = TempDir::new();
    for sub in ["reader", "admin"] {
        std::fs::create_dir_all(dir.0.join(sub)).unwrap();
        std::fs::write(dir.0.join(sub).join("index.html"), format!("{sub}-index")).unwrap();
    }
    let cfg = fallback_config(&dir);
    validate_reader(&cfg).unwrap();
    let (reserve, _) = reserve(0);
    let inner = crate::web_dir::mount_reader(
        Router::new().route("/graphql", get(|| async { "api" })),
        &cfg.reader_dir,
    );
    let app = protect(inner, &cfg.hosts, cfg.port, reserve);
    for path in ["/", "/admin/", "/admin/x"] {
        let response = app
            .clone()
            .oneshot(request("GET", path, Some("localhost")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(bytes(response).await, "reader-index", "{path}");
    }
    assert_eq!(
        bytes(
            app.oneshot(request("GET", "/graphql", Some("localhost")))
                .await
                .unwrap()
        )
        .await,
        "api"
    );
    std::fs::remove_file(cfg.reader_dir.join("index.html")).unwrap();
    assert!(validate_reader(&cfg)
        .unwrap_err()
        .to_string()
        .contains("KOMIKA_FALLBACK_READER_DIR"));
}

#[cfg(unix)]
#[test]
fn unreadable_reader_index_fails_startup() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let cfg = fallback_config(&dir);
    std::fs::create_dir_all(&cfg.reader_dir).unwrap();
    let index = cfg.reader_dir.join("index.html");
    std::fs::write(&index, "reader-index").unwrap();
    let permissions = std::fs::metadata(&index).unwrap().permissions();
    std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = validate_reader(&cfg);
    std::fs::set_permissions(&index, permissions).unwrap();
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("KOMIKA_FALLBACK_READER_DIR"));
}

// rcgen is not cached offline. Build a short-lived self-signed X.509 leaf using
// the existing Origin TLS DER/key helpers, without a committed certificate/key.
fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if contents.len() < 128 {
        out.push(contents.len() as u8);
    } else {
        let len = contents.len().to_be_bytes();
        let first = len.iter().position(|v| *v != 0).unwrap();
        out.push(0x80 | (len.len() - first) as u8);
        out.extend_from_slice(&len[first..]);
    }
    out.extend_from_slice(contents);
    out
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

fn certificate() -> (String, String, Vec<u8>) {
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
    let pkcs8 = origin_tls::generate_key().unwrap();
    let rng = ring::rand::SystemRandom::new();
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8, &rng).unwrap();
    let name = origin_tls::der_name("localhost");
    let algorithm = origin_tls::der_ecdsa_sha256();
    let now = chrono::Utc::now();
    let validity = seq(&[
        &tlv(
            0x17,
            (now - chrono::Duration::days(1))
                .format("%y%m%d%H%M%SZ")
                .to_string()
                .as_bytes(),
        ),
        &tlv(
            0x17,
            (now + chrono::Duration::days(1))
                .format("%y%m%d%H%M%SZ")
                .to_string()
                .as_bytes(),
        ),
    ]);
    let tbs = seq(&[
        &tlv(0xA0, &tlv(0x02, &[2])),
        &tlv(0x02, &[1]),
        &algorithm,
        &name,
        &validity,
        &name,
        &origin_tls::der_spki(pair.public_key().as_ref()),
        &tlv(0xA3, &seq(&[&origin_tls::der_san_extension("localhost")])),
    ]);
    let signature = pair.sign(&rng, &tbs).unwrap();
    let der = seq(&[
        &tbs,
        &algorithm,
        &origin_tls::der_bit_string(signature.as_ref()),
    ]);
    (
        origin_tls::pem("CERTIFICATE", &der),
        origin_tls::pem("PRIVATE KEY", &pkcs8),
        der,
    )
}

#[test]
fn tls_accepts_pkcs8_and_sec1_pem_without_changing_legacy_der_config() {
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
    use tokio_rustls::rustls::pki_types::{pem::PemObject, PrivateKeyDer};
    fn element(bytes: &[u8]) -> (&[u8], &[u8]) {
        let (len, header) = if bytes[1] < 128 {
            (bytes[1] as usize, 2)
        } else {
            let n = (bytes[1] & 0x7f) as usize;
            (
                bytes[2..2 + n]
                    .iter()
                    .fold(0usize, |len, byte| (len << 8) | *byte as usize),
                2 + n,
            )
        };
        (&bytes[header..header + len], &bytes[header + len..])
    }
    let (cert, key_pem, cert_der) = certificate();
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();
    let pkcs8 = key.secret_der();
    let pair = EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        pkcs8,
        &ring::rand::SystemRandom::new(),
    )
    .unwrap();
    let (outer, _) = element(pkcs8);
    let (_, after_version) = element(outer);
    let (_, after_algorithm) = element(after_version);
    let (sec1, _) = element(after_algorithm);
    let (sec1, _) = element(sec1);
    let (_, after_version) = element(sec1);
    let (scalar, _) = element(after_version);
    let sec1 = seq(&[
        &tlv(0x02, &[1]),
        &tlv(0x04, scalar),
        &tlv(0xa0, &tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 3, 1, 7])),
        &tlv(
            0xa1,
            &origin_tls::der_bit_string(pair.public_key().as_ref()),
        ),
    ]);
    for config in [
        origin_tls::server_config(&cert, pkcs8),
        origin_tls::server_config_pem(&cert, key_pem.as_bytes()),
        origin_tls::server_config_pem(&cert, origin_tls::pem("EC PRIVATE KEY", &sec1).as_bytes()),
    ] {
        assert_eq!(memory_handshake(config.unwrap(), &cert_der), cert_der);
    }
}

#[test]
fn certificate_reload_validates_pairs_and_retains_old_on_bad_renewal() {
    fn padded(mut pem: String) -> String {
        pem.extend(std::iter::repeat_n('\n', 1024 - pem.len()));
        pem
    }
    let dir = TempDir::new();
    let cfg = fallback_config(&dir);
    let (cert, key, first_der) = certificate();
    let (cert, key) = (padded(cert), padded(key));
    std::fs::write(&cfg.cert_file, &cert).unwrap();
    std::fs::write(&cfg.key_file, &key).unwrap();
    let (tls, resolver) = tls::ReloadingCert::load(&cfg.cert_file, &cfg.key_file).unwrap();
    assert_eq!(tls.alpn_protocols, vec![b"http/1.1".to_vec()]);
    assert_eq!(memory_handshake(tls.clone(), &first_der), first_der);
    assert!(!resolver.reload_now().unwrap());
    let (next_cert, next_key, next_der) = certificate();
    let (next_cert, next_key) = (padded(next_cert), padded(next_key));
    let modified = [&cfg.cert_file, &cfg.key_file]
        .map(|path| std::fs::metadata(path).unwrap().modified().unwrap());
    // A cert/key mismatch and malformed replacements must retain the old pair.
    replace(&cfg.cert_file, &next_cert);
    assert!(resolver.reload_now().is_err());
    assert_eq!(resolver.current_certificate(), first_der);
    assert_eq!(memory_handshake(tls.clone(), &first_der), first_der);
    assert!(tls::ReloadingCert::load(&cfg.cert_file, &cfg.key_file).is_err());
    replace(&cfg.key_file, &next_key);
    // Both replacement files have the old length and mtime.
    for (path, modified) in [&cfg.cert_file, &cfg.key_file].into_iter().zip(modified) {
        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!(metadata.len(), 1024);
        assert_eq!(metadata.modified().unwrap(), modified);
    }
    assert!(resolver.reload_now().unwrap());
    assert_eq!(resolver.current_certificate(), next_der);
    assert_eq!(memory_handshake(tls.clone(), &next_der), next_der);
    assert!(!resolver.reload_now().unwrap());
    replace(&cfg.key_file, "invalid PEM");
    assert!(resolver.reload_now().is_err());
    assert_eq!(resolver.current_certificate(), next_der);
    assert!(tls::ReloadingCert::load(&cfg.cert_file, &cfg.key_file).is_err());
}

fn client_config(trusted: &[CertificateDer<'static>]) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in trusted {
        roots.add(cert.clone()).unwrap();
    }
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}

// Exercise real certificate verification/reload even in a sandbox that denies
// sockets; the separate e2e test below still uses the production TLS listener.
fn memory_handshake(config: Arc<rustls::ServerConfig>, trusted: &[u8]) -> Vec<u8> {
    let config_client = client_config(&[CertificateDer::from(trusted.to_vec())]);
    let mut client =
        rustls::ClientConnection::new(config_client, ServerName::try_from("localhost").unwrap())
            .unwrap();
    let mut server = rustls::ServerConnection::new(config).unwrap();
    for _ in 0..16 {
        let mut buf = Vec::new();
        client.write_tls(&mut buf).unwrap();
        if !buf.is_empty() {
            server.read_tls(&mut &buf[..]).unwrap();
            server.process_new_packets().unwrap();
        }
        buf.clear();
        server.write_tls(&mut buf).unwrap();
        if !buf.is_empty() {
            client.read_tls(&mut &buf[..]).unwrap();
            client.process_new_packets().unwrap();
        }
        if !client.is_handshaking() && !server.is_handshaking() {
            assert_eq!(client.alpn_protocol(), Some(&b"http/1.1"[..]));
            return client.peer_certificates().unwrap()[0].to_vec();
        }
    }
    panic!("in-memory TLS handshake did not complete");
}

async fn tls_request(
    addr: SocketAddr,
    trusted: &[CertificateDer<'static>],
    host: &str,
    path: &str,
) -> (String, Vec<u8>) {
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    // Deliberately independent of HTTP Host: media sent with reader.example
    // below uses SNI localhost and must consume exactly the same reserve.
    let mut stream = tokio_rustls::TlsConnector::from(client_config(trusted))
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap();
    let peer = stream.get_ref().1.peer_certificates().unwrap()[0].to_vec();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    (String::from_utf8(response).unwrap(), peer)
}

struct Tasks(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

#[tokio::test]
async fn tls_e2e_authority_reserve_sni_and_certificate_reload() {
    let dir = TempDir::new();
    let cfg = fallback_config(&dir);
    let (cert, key, first_der) = certificate();
    std::fs::write(&cfg.cert_file, &cert).unwrap();
    std::fs::write(&cfg.key_file, &key).unwrap();
    let (config, resolver) = tls::ReloadingCert::load(&cfg.cert_file, &cfg.key_file).unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fallback TLS e2e test requires permission to bind a localhost socket");
    let addr = tcp.local_addr().unwrap();
    let (listener, accept) = origin_tls::TlsListener::new(tcp, config).unwrap();
    let (reserve, _) = reserve(9);
    let app = protect(
        stub(Arc::new(AtomicU64::new(0))),
        &cfg.hosts,
        addr.port(),
        reserve.clone(),
    );
    let _tasks = Tasks(vec![accept, tokio::spawn(origin_tls::serve(listener, app))]);
    let trust = vec![CertificateDer::from(first_der.clone())];
    let (response, peer) = tls_request(addr, &trust, "localhost", "/").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(peer, first_der);
    assert!(tls_request(addr, &trust, "evil.example", "/img")
        .await
        .0
        .starts_with("HTTP/1.1 421"));
    assert_eq!(used(&reserve), 0);
    assert!(tls_request(addr, &trust, "reader.example", "/img")
        .await
        .0
        .starts_with("HTTP/1.1 200"));
    assert_eq!(
        used(&reserve),
        9,
        "SNI localhost with a different Host still counts"
    );
    let response = tls_request(addr, &trust, "localhost", "/media/test")
        .await
        .0;
    assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    assert!(response
        .to_ascii_lowercase()
        .contains("cache-control: no-store"));
    assert!(tls_request(addr, &trust, "localhost", "/graphql")
        .await
        .0
        .starts_with("HTTP/1.1 200"));
    let (next_cert, next_key, next_der) = certificate();
    replace(&cfg.cert_file, &next_cert);
    // The invalid intermediate pair leaves the first certificate serving.
    assert!(resolver.reload_now().is_err());
    assert_eq!(
        tls_request(addr, &trust, "localhost", "/").await.1,
        first_der
    );
    replace(&cfg.key_file, &next_key);
    assert!(resolver.reload_now().unwrap());
    assert_eq!(
        tls_request(
            addr,
            &[CertificateDer::from(next_der.clone())],
            "localhost",
            "/"
        )
        .await
        .1,
        next_der
    );
}
