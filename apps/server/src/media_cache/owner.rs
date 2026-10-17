//! The cache's view of its owner: an HTTP client for the owner's public media routes
//! and the signing keys pinned from it.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::media_verify::{verify_key_set, Envelope, KeySetBody, PinnedKeys, PublicKeyEntry};

/// Signed JSON (key sets, manifests, leases) is small; a larger answer is refused.
const MAX_SIGNED_BYTES: u64 = 8 << 20;

pub(crate) struct Owner {
    origin: String,
    http: reqwest::Client,
}

/// A failed owner request: unreachable, a non-200 answer, or too many bytes.
#[derive(Debug)]
pub(crate) enum FetchError {
    Status(reqwest::StatusCode),
    TooLarge,
    Network(reqwest::Error),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(status) => write!(f, "owner answered {status}"),
            Self::TooLarge => write!(f, "owner answer too large"),
            Self::Network(error) => write!(f, "owner unreachable: {error}"),
        }
    }
}
impl std::error::Error for FetchError {}

/// An owner origin: `http(s)://host[:port]`, no credentials, path, query or fragment.
pub(crate) fn parse_origin(raw: &str) -> Result<String> {
    let url = reqwest::Url::parse(raw).map_err(|_| anyhow!("--owner {raw:?} is not a URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        bail!("--owner must be an http(s) origin with no credentials, path, query or fragment");
    }
    Ok(url.origin().ascii_serialization())
}

impl Owner {
    pub(crate) fn new(origin: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("komika-media-cache/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            // A redirect could send the cache to a host it was not told to trust.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { origin, http })
    }

    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    /// GET a path on the owner. Returns the body (at most `max` bytes) and its
    /// `Content-Type`.
    pub(crate) async fn get(&self, path: &str, max: u64) -> Result<(Vec<u8>, String), FetchError> {
        let mut response = self
            .http
            .get(format!("{}{path}", self.origin))
            .send()
            .await
            .map_err(FetchError::Network)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(FetchError::Status(response.status()));
        }
        if response.content_length().is_some_and(|length| length > max) {
            return Err(FetchError::TooLarge);
        }
        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(FetchError::Network)? {
            if body.len() as u64 + chunk.len() as u64 > max {
                return Err(FetchError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((body, mime))
    }

    pub(crate) async fn signed(&self, path: &str) -> Result<Vec<u8>, FetchError> {
        self.get(path, MAX_SIGNED_BYTES).await.map(|(body, _)| body)
    }
}

/// The keys a cache trusts, kept in `pins.json` in its directory. They are set once,
/// from the owner's own key set, and change only by `media-cache repin`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Pins {
    pub owner: String,
    pub keys: Vec<PublicKeyEntry>,
}

impl Pins {
    pub(crate) fn pinned(&self) -> Result<PinnedKeys> {
        PinnedKeys::new(self.keys.iter().map(|key| (&key.kid, &key.public_key)))
            .map_err(|error| anyhow!("pins.json holds an invalid key: {error}"))
    }

    pub(crate) fn kids(&self) -> Vec<&str> {
        self.keys.iter().map(|key| key.kid.as_str()).collect()
    }

    pub(crate) fn load(dir: &Path) -> Result<Option<Self>> {
        match std::fs::read(dir.join(PINS_FILE)) {
            Ok(bytes) => {
                let pins: Self =
                    serde_json::from_slice(&bytes).context("pins.json is not valid")?;
                pins.pinned()?;
                Ok(Some(pins))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read pins.json"),
        }
    }

    /// Write `pins.json` atomically (temp file, sync, rename).
    pub(crate) fn save(&self, dir: &Path) -> Result<()> {
        let temp = dir.join(format!("{PINS_FILE}.{}.tmp", uuid::Uuid::new_v4().simple()));
        let written = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
            std::fs::rename(&temp, dir.join(PINS_FILE))?;
            std::fs::File::open(dir)?.sync_all()
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        written.context("write pins.json")
    }
}

pub(crate) const PINS_FILE: &str = "pins.json";

/// Read the owner's key set with no pins yet. When the operator names the owner's key
/// (`--expect-kid`), the set must be signed by that key; otherwise it must be signed by
/// one of the keys it lists (trust on first use).
pub(crate) fn first_pins(owner: &str, key_set: &[u8], expect_kid: Option<&str>) -> Result<Pins> {
    let envelope: Envelope =
        serde_json::from_slice(key_set).map_err(|_| anyhow!("owner key set is not valid"))?;
    let listed: KeySetBody = serde_json::from_str(&envelope.payload)
        .map_err(|_| anyhow!("owner key set is not valid"))?;
    let signers = listed
        .keys
        .iter()
        .filter(|key| expect_kid.is_none_or(|kid| key.kid == kid));
    let candidates = PinnedKeys::new(signers.map(|key| (&key.kid, &key.public_key)))
        .map_err(|error| anyhow!("owner key set is not valid: {error}"))?;
    let keys = verify_key_set(key_set, &candidates).map_err(|error| match expect_kid {
        Some(kid) => anyhow!("owner key set is not signed by the expected key {kid}: {error}"),
        None => anyhow!("owner key set is not self-signed: {error}"),
    })?;
    if keys.is_empty() {
        bail!("owner key set lists no keys");
    }
    Ok(Pins {
        owner: owner.to_string(),
        keys,
    })
}

/// Does the owner's current key set still verify under the pins and list only pinned
/// keys? A key set signed by an unpinned key, or one that adds a key, is a rotation the
/// operator must accept with `media-cache repin`.
pub(crate) fn key_set_matches(pins: &Pins, key_set: &[u8]) -> Result<(), String> {
    let pinned = pins.pinned().map_err(|error| error.to_string())?;
    let keys = verify_key_set(key_set, &pinned).map_err(|error| error.to_string())?;
    let known = pins.kids();
    match keys.iter().find(|key| !known.contains(&key.kid.as_str())) {
        Some(key) => Err(format!("unpinned key {}", key.kid)),
        None => Ok(()),
    }
}
