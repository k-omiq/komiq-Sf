use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use serde::Serialize;

use crate::media_verify::{key_id, Envelope, PublicKeyEntry};

pub(crate) struct Signer {
    key: Ed25519KeyPair,
    pub kid: String,
}

/// Writes a new key beside `path`, then links it into place, so the key file is
/// either absent or complete: a crash mid-write cannot leave a short file that
/// stops every later start, and of two starts racing, one key wins for both.
fn create(path: &Path) -> Result<()> {
    let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| anyhow!("generate media signing key"))?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let partial = path.with_file_name(name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&partial)
        .context("create media signing key")
        .and_then(|mut file| {
            file.write_all(document.as_ref())
                .context("write media signing key")?;
            file.sync_all().context("sync media signing key")
        })
        .and_then(|()| match std::fs::hard_link(&partial, path) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                Err(error).context("place media signing key")
            }
            _ => Ok(()),
        });
    let _ = std::fs::remove_file(&partial);
    written
}

impl Signer {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let mut file = match OpenOptions::new().read(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create(path)?;
                OpenOptions::new().read(true).open(path)?
            }
            Err(error) => return Err(error).context("open media signing key"),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file.metadata()?.permissions().mode() & 0o077 != 0 {
                bail!("media signing key permits group/other access");
            }
            // Persist the published name even if another startup linked it into place.
            let directory = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::File::open(directory)
                .and_then(|directory| directory.sync_all())
                .context("sync media signing key directory")?;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .context("read media signing key")?;
        let key = Ed25519KeyPair::from_pkcs8(&bytes)
            .map_err(|_| anyhow!("invalid media signing key PKCS#8"))?;
        let kid = key_id(key.public_key().as_ref());
        tracing::info!(%kid, path = %path.display(), "media signing key loaded");
        Ok(Self { key, kid })
    }

    pub(crate) fn public_entry(&self) -> PublicKeyEntry {
        PublicKeyEntry {
            kid: self.kid.clone(),
            alg: "Ed25519".into(),
            public_key: URL_SAFE_NO_PAD.encode(self.key.public_key().as_ref()),
        }
    }

    pub(crate) fn sign(&self, body: &impl Serialize) -> Result<Vec<u8>> {
        let payload = serde_json::to_vec(body)?;
        let sig = URL_SAFE_NO_PAD.encode(self.key.sign(&payload).as_ref());
        Ok(serde_json::to_vec(&Envelope {
            payload: String::from_utf8(payload)?,
            kid: self.kid.clone(),
            sig,
        })?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_file_creation_and_reload() {
        let dir = crate::media_manifest_tests::TempDir::new();
        let path = dir.0.join("signing.key");
        let first = Signer::load(&path).unwrap();
        let second = Signer::load(&path).unwrap();
        assert_eq!(first.kid, second.kid);
        assert_eq!(
            first.sign(&"deterministic").unwrap(),
            second.sign(&"deterministic").unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn concurrent_key_file_creation_keeps_one_key_and_no_temporary_files() {
        let dir = crate::media_manifest_tests::TempDir::new();
        let path = dir.0.join("signing.key");
        let start = std::sync::Barrier::new(8);
        let kids = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        Signer::load(&path).unwrap().kid
                    })
                })
                .collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(kids.iter().all(|kid| *kid == kids[0]));
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
        assert_eq!(Signer::load(&path).unwrap().kid, kids[0]);
    }

    #[test]
    #[cfg(unix)]
    fn key_file_refuses_group_readable_and_invalid_keys() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::media_manifest_tests::TempDir::new();
        let path = dir.0.join("signing.key");
        Signer::load(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(Signer::load(&path).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, b"invalid PKCS#8").unwrap();
        assert!(Signer::load(&path).is_err());
    }
}
