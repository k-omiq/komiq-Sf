//! The cache's view of its owner: an HTTP client for the owner's public media routes
//! and the signing keys pinned from it.

use std::{path::Path, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::media_verify::{verify_key_set, Envelope, KeySetBody, PinnedKeys, PublicKeyEntry};

/// Signed JSON (key sets, manifests, leases) is small; a larger answer is refused.
const MAX_SIGNED_BYTES: u64 = 8 << 20;

/// Small owner answers (signed JSON, single objects) must finish within this.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
/// Streamed downloads (packs) have no total deadline, since a large pack on a slow link
/// may take longer; instead each read of the connection must make progress within this.
const STREAM_READ_IDLE: Duration = Duration::from_secs(30);

pub(crate) struct Owner {
    origin: String,
    /// For bounded requests: a total deadline covering the body as well.
    http: reqwest::Client,
    /// For `stream`: no total deadline, only a per-read idle deadline.
    stream_http: reqwest::Client,
    #[cfg(test)]
    router: Option<axum::Router>,
}

enum Source {
    Http(reqwest::Response),
    #[cfg(test)]
    Router(axum::body::BodyDataStream),
}

/// Pulling this stream drives the HTTP download; dropping it cancels the request.
pub(crate) struct Download {
    source: Source,
    remaining: u64,
    pub(crate) mime: String,
    pub(crate) fetched: u64,
}

impl Download {
    pub(crate) async fn chunk(&mut self) -> Result<Option<axum::body::Bytes>, FetchError> {
        let chunk = match &mut self.source {
            Source::Http(response) => response.chunk().await.map_err(FetchError::Network)?,
            #[cfg(test)]
            Source::Router(body) => {
                use futures::StreamExt;
                body.next()
                    .await
                    .transpose()
                    .map_err(|_| FetchError::Body)?
            }
        };
        if let Some(bytes) = &chunk {
            self.fetched = self.fetched.saturating_add(bytes.len() as u64);
            if bytes.len() as u64 > self.remaining {
                return Err(FetchError::TooLarge);
            }
            self.remaining -= bytes.len() as u64;
        }
        Ok(chunk)
    }
}

/// A failed owner request: unreachable, a non-200 answer, or too many bytes.
#[derive(Debug)]
pub(crate) enum FetchError {
    Status(reqwest::StatusCode),
    TooLarge,
    Network(reqwest::Error),
    #[cfg(test)]
    Body,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(status) => write!(f, "owner answered {status}"),
            Self::TooLarge => write!(f, "owner answer too large"),
            Self::Network(error) => write!(f, "owner unreachable: {error}"),
            #[cfg(test)]
            Self::Body => write!(f, "owner body failed"),
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
        let builder = || {
            reqwest::Client::builder()
                .user_agent(concat!("komika-media-cache/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(10))
                // A redirect could send the cache to a host it was not told to trust.
                .redirect(reqwest::redirect::Policy::none())
        };
        let http = builder().timeout(TOTAL_TIMEOUT).build()?;
        let stream_http = builder().read_timeout(STREAM_READ_IDLE).build()?;
        Ok(Self {
            origin,
            http,
            stream_http,
            #[cfg(test)]
            router: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn from_router(router: axum::Router) -> Self {
        let mut owner = Self::new("http://localhost".into()).unwrap();
        owner.router = Some(router);
        owner
    }

    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    /// GET a path on the owner. Returns the body (at most `max` bytes) and its
    /// `Content-Type`.
    pub(crate) async fn get(&self, path: &str, max: u64) -> Result<(Vec<u8>, String), FetchError> {
        let mut download = self.download(&self.http, path, max).await?;
        let mut body = Vec::new();
        while let Some(chunk) = download.chunk().await? {
            body.extend_from_slice(&chunk);
        }
        Ok((body, download.mime))
    }

    /// GET a long body as a stream (packs): no total deadline, but a read that makes no
    /// progress for `STREAM_READ_IDLE` fails the download.
    pub(crate) async fn stream(&self, path: &str, max: u64) -> Result<Download, FetchError> {
        self.download(&self.stream_http, path, max).await
    }

    async fn download(
        &self,
        http: &reqwest::Client,
        path: &str,
        max: u64,
    ) -> Result<Download, FetchError> {
        #[cfg(test)]
        if let Some(router) = &self.router {
            use tower::ServiceExt;
            let response = router
                .clone()
                .oneshot(
                    axum::http::Request::get(path)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            if response.status() != reqwest::StatusCode::OK {
                return Err(FetchError::Status(response.status()));
            }
            if response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .is_some_and(|length| length > max)
            {
                return Err(FetchError::TooLarge);
            }
            let mime = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            return Ok(Download {
                source: Source::Router(response.into_body().into_data_stream()),
                remaining: max,
                mime,
                fetched: 0,
            });
        }
        let response = http
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
        Ok(Download {
            source: Source::Http(response),
            remaining: max,
            mime,
            fetched: 0,
        })
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
