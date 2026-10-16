//! Operator-owned PEM reload. Handshakes only clone the current certified key.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Context;
use sha2::{Digest, Sha256};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;

use crate::cloudflare::origin_tls;

const RELOAD_INTERVAL: Duration = Duration::from_secs(5 * 60);

type Stamp = [u8; 32];

fn read(path: &Path) -> anyhow::Result<(Vec<u8>, Stamp)> {
    let bytes = std::fs::read(path)?;
    let stamp = Sha256::digest(&bytes).into();
    Ok((bytes, stamp))
}

fn certified_key(cert: &str, key: &[u8]) -> anyhow::Result<Arc<CertifiedKey>> {
    let certs = CertificateDer::pem_slice_iter(cert.as_bytes()).collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(key)?;
    let key = CertifiedKey::from_der(certs, key, &rustls::crypto::ring::default_provider())?;
    // Require a positive match (from_der also permits unknown consistency).
    key.keys_match()?;
    Ok(Arc::new(key))
}

#[derive(Debug)]
pub(super) struct ReloadingCert {
    cert_file: PathBuf,
    key_file: PathBuf,
    current: RwLock<Arc<CertifiedKey>>,
    // Only the reload worker takes this lock; handshake threads never do I/O or
    // wait for parsing. Swap the current key only once the whole pair is valid.
    stamps: Mutex<(Stamp, Stamp)>,
}

impl ReloadingCert {
    pub(super) fn load(
        cert_file: &Path,
        key_file: &Path,
    ) -> anyhow::Result<(Arc<rustls::ServerConfig>, Arc<Self>)> {
        let (cert, cert_stamp) = read(cert_file).context("reading KOMIKA_FALLBACK_CERT_FILE")?;
        let (key, key_stamp) = read(key_file).context("reading KOMIKA_FALLBACK_KEY_FILE")?;
        let cert = std::str::from_utf8(&cert).context("KOMIKA_FALLBACK_CERT_FILE must be PEM")?;
        let mut config = origin_tls::server_config_pem(cert, &key)
            .map_err(anyhow::Error::msg)
            .context("KOMIKA_FALLBACK_CERT_FILE / KOMIKA_FALLBACK_KEY_FILE TLS setup")?;
        let resolver = Arc::new(Self {
            cert_file: cert_file.to_path_buf(),
            key_file: key_file.to_path_buf(),
            current: RwLock::new(
                certified_key(cert, &key).context("fallback certificate/key mismatch")?,
            ),
            stamps: Mutex::new((cert_stamp, key_stamp)),
        });
        Arc::get_mut(&mut config)
            .expect("unshared TLS config")
            .cert_resolver = resolver.clone();
        Ok((config, resolver))
    }

    // Also lets tests force a check without sleeping for the production interval.
    pub(super) fn reload_now(&self) -> anyhow::Result<bool> {
        let mut stamps = self.stamps.lock().unwrap_or_else(|e| e.into_inner());
        let (cert, cert_stamp) = read(&self.cert_file)?;
        let (key, key_stamp) = read(&self.key_file)?;
        if *stamps == (cert_stamp, key_stamp) {
            return Ok(false);
        }
        let next = certified_key(std::str::from_utf8(&cert)?, &key)?;
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = next;
        *stamps = (cert_stamp, key_stamp);
        tracing::info!("fallback TLS certificate reloaded");
        Ok(true)
    }

    #[cfg(test)]
    pub(super) fn current_certificate(&self) -> Vec<u8> {
        self.current.read().unwrap().cert[0].to_vec()
    }

    pub(super) fn spawn_reload(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RELOAD_INTERVAL).await;
                let Some(resolver) = weak.upgrade() else {
                    return;
                };
                // No filesystem work on runtime/handshake threads.
                let result = tokio::task::spawn_blocking(move || resolver.reload_now()).await;
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "fallback TLS reload failed; keeping previous certificate")
                    }
                    Err(error) => {
                        tracing::warn!(%error, "fallback TLS reload task failed; keeping previous certificate")
                    }
                }
            }
        });
    }
}

impl ResolvesServerCert for ReloadingCert {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(
            self.current
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        )
    }
}
