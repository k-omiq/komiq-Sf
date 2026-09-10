//! HTTPS for the Cloudflare "public IP" mode.
//!
//! With a proxied DNS record pointing at the server, Cloudflare connects to the origin
//! on port 443. Setup asks Cloudflare's Origin CA for a certificate and the server
//! serves HTTPS with it, so the hop from Cloudflare to the server is encrypted and the
//! zone can use SSL mode "Full (strict)".
//!
//! The Origin CA API signs a certificate request (CSR); it never makes the key. The key
//! is an ECDSA P-256 key made here with `ring` and written to a 0600 file next to the
//! database; the CSR is a minimal PKCS#10 encoded by hand below (one subject CN plus the
//! same name as a SAN), which keeps a certificate-builder crate out of the tree.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

/// A fresh P-256 key as PKCS#8 DER.
pub fn generate_key() -> Result<Vec<u8>, String> {
    let rng = SystemRandom::new();
    EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .map(|doc| doc.as_ref().to_vec())
        .map_err(|_| "Could not generate a private key.".to_string())
}

/// A PEM certificate request for `hostname`, signed with `pkcs8`.
pub fn csr_pem(pkcs8: &[u8], hostname: &str) -> Result<String, String> {
    let der = csr_der(pkcs8, hostname)?;
    Ok(pem("CERTIFICATE REQUEST", &der))
}

// ---- DER, just enough for PKCS#10 -------------------------------------------------

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const OID_PRIME256V1: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
const OID_ECDSA_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
const OID_EXTENSION_REQUEST: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x0E];
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1D, 0x11];

/// `Name` with a single CN.
pub(crate) fn der_name(cn: &str) -> Vec<u8> {
    let atv = seq(&[&tlv(0x06, OID_COMMON_NAME), &tlv(0x0C, cn.as_bytes())]);
    seq(&[&tlv(0x31, &atv)])
}

/// `SubjectPublicKeyInfo` for an uncompressed P-256 point.
pub(crate) fn der_spki(point: &[u8]) -> Vec<u8> {
    let alg = seq(&[&tlv(0x06, OID_EC_PUBLIC_KEY), &tlv(0x06, OID_PRIME256V1)]);
    let mut bits = vec![0u8];
    bits.extend_from_slice(point);
    seq(&[&alg, &tlv(0x03, &bits)])
}

/// `subjectAltName` extension value: one dNSName.
pub(crate) fn der_san_extension(hostname: &str) -> Vec<u8> {
    let names = seq(&[&tlv(0x82, hostname.as_bytes())]);
    seq(&[&tlv(0x06, OID_SUBJECT_ALT_NAME), &tlv(0x04, &names)])
}

pub(crate) fn der_ecdsa_sha256() -> Vec<u8> {
    seq(&[&tlv(0x06, OID_ECDSA_SHA256)])
}

pub(crate) fn der_bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut bits = vec![0u8];
    bits.extend_from_slice(bytes);
    tlv(0x03, &bits)
}

fn csr_der(pkcs8: &[u8], hostname: &str) -> Result<Vec<u8>, String> {
    let rng = SystemRandom::new();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8, &rng)
        .map_err(|_| "The stored private key is not usable.".to_string())?;
    let extensions = seq(&[&der_san_extension(hostname)]);
    let attribute = seq(&[&tlv(0x06, OID_EXTENSION_REQUEST), &tlv(0x31, &extensions)]);
    let info = seq(&[
        &tlv(0x02, &[0]),
        &der_name(hostname),
        &der_spki(key.public_key().as_ref()),
        // [0] IMPLICIT SET OF Attribute
        &tlv(0xA0, &attribute),
    ]);
    let sig = key
        .sign(&rng, &info)
        .map_err(|_| "Could not sign the certificate request.".to_string())?;
    Ok(seq(&[
        &info,
        &der_ecdsa_sha256(),
        &der_bit_string(sig.as_ref()),
    ]))
}

pub(crate) fn pem(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

// ---- serving ---------------------------------------------------------------------

/// A rustls server config from a PEM certificate chain and a PKCS#8 key.
pub fn server_config(cert_pem: &str, pkcs8: &[u8]) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|_| "The origin certificate is not valid PEM.".to_string())?;
    if certs.is_empty() {
        return Err("The origin certificate file is empty.".to_string());
    }
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8.to_vec()));
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("TLS setup failed: {e}"))?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| format!("The origin certificate does not match its key: {e}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// An `axum::serve` listener that hands over connections after the TLS handshake.
/// Handshakes run in their own tasks (bounded by a timeout), so one slow client
/// cannot hold up `accept` for everyone else.
pub struct TlsListener {
    rx: tokio::sync::mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
}

impl TlsListener {
    /// The listener, plus the accept loop's task: abort and await it to release the
    /// port at once (dropping the listener alone frees it only when the loop next runs).
    pub fn new(
        tcp: TcpListener,
        config: Arc<rustls::ServerConfig>,
    ) -> std::io::Result<(Self, tokio::task::JoinHandle<()>)> {
        let local = tcp.local_addr()?;
        let acceptor = TlsAcceptor::from(config);
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let accept = tokio::spawn(async move {
            loop {
                // Stop (and release the port) as soon as the listener is dropped,
                // without waiting for one more connection.
                let accepted = tokio::select! {
                    a = tcp.accept() => a,
                    _ = tx.closed() => return,
                };
                let (stream, peer) = match accepted {
                    Ok(c) => c,
                    Err(e) => {
                        // Out of file descriptors and similar: back off, don't spin.
                        tracing::warn!(error = %e, "https accept failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let hs = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream));
                    match hs.await {
                        Ok(Ok(tls)) => {
                            let _ = tx.send((tls, peer)).await;
                        }
                        Ok(Err(e)) => tracing::debug!(%peer, error = %e, "tls handshake failed"),
                        Err(_) => tracing::debug!(%peer, "tls handshake timed out"),
                    }
                });
            }
        });
        Ok((Self { rx, local }, accept))
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(c) => c,
            // The accept loop only ends once this listener is dropped.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// Serve `app` over TLS until the task is aborted. The handlers read the client
/// address from `ConnectInfo<SocketAddr>`; axum provides that for any listener whose
/// `Addr` is `SocketAddr` once it is wrapped in `tap_io`, so the no-op tap is what
/// keeps the HTTPS port answering exactly like the plain one.
pub async fn serve(listener: TlsListener, app: axum::Router) {
    use axum::serve::ListenerExt;
    let listener = listener.tap_io(|_| {});
    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    {
        tracing::warn!(error = %e, "https listener stopped");
    }
}

/// Write `bytes` to `path` readable by the server's user only (0600 on unix).
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_lengths_use_long_form_past_127() {
        assert_eq!(tlv(0x04, &[1; 3])[..2], [0x04, 3]);
        let long = tlv(0x04, &[0; 200]);
        assert_eq!(long[..3], [0x04, 0x81, 200]);
        let longer = tlv(0x04, &[0; 300]);
        assert_eq!(longer[..4], [0x04, 0x82, 0x01, 0x2C]);
    }

    #[test]
    fn csr_is_signed_by_the_key_it_names() {
        let key = generate_key().unwrap();
        let pem_text = csr_pem(&key, "read.example.com").unwrap();
        assert!(pem_text.starts_with("-----BEGIN CERTIFICATE REQUEST-----\n"));
        let b64: String = pem_text
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(der[0], 0x30);
        // Split the outer SEQUENCE into info / algorithm / signature and verify the
        // signature over the info bytes with the key's public half.
        let (body, _) = read_tlv(&der);
        let (info, rest) = read_tlv_raw(body);
        let (_alg, rest) = read_tlv_raw(rest);
        let (sig_bits, _) = read_tlv(rest);
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &key, &rng).unwrap();
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_ASN1,
            pair.public_key().as_ref(),
        );
        public.verify(info, &sig_bits[1..]).unwrap();
        // The hostname is in the request (CN and SAN).
        let needle = b"read.example.com";
        assert!(der.windows(needle.len()).filter(|w| w == needle).count() >= 2);
    }

    /// (content, rest) of the TLV at the start of `b`.
    fn read_tlv(b: &[u8]) -> (&[u8], &[u8]) {
        let (len, hdr) = header(b);
        (&b[hdr..hdr + len], &b[hdr + len..])
    }

    /// (whole TLV, rest).
    fn read_tlv_raw(b: &[u8]) -> (&[u8], &[u8]) {
        let (len, hdr) = header(b);
        (&b[..hdr + len], &b[hdr + len..])
    }

    fn header(b: &[u8]) -> (usize, usize) {
        if b[1] < 0x80 {
            (b[1] as usize, 2)
        } else {
            let n = (b[1] & 0x7F) as usize;
            let len = b[2..2 + n]
                .iter()
                .fold(0usize, |a, x| (a << 8) | *x as usize);
            (len, 2 + n)
        }
    }
}
