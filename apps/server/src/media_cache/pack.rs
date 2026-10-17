//! Experimental packs, trusted only through an exact signed revision manifest.

use std::io;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::Method;
use tokio::io::AsyncReadExt;

use super::*;
use crate::media_pack::{build_index, parse_header, Pack, PACK_CONTENT_TYPE};

/// How long a request waits for another request's fill of the same pack. A stalled
/// first client must not hold every later request for that pack.
const PACK_TURN_WAIT: Duration = Duration::from_secs(30);

impl MediaCache {
    async fn revision(&self, copy: &str, revision: i32) -> Result<Arc<Held>, Failure> {
        let key = Key::Revision(copy.to_string(), revision);
        let latest = Key::Manifest(copy.to_string());
        let exact = |held: &Held| {
            held.body
                .as_ref()
                .is_some_and(|body| body.revision == i64::from(revision))
        };
        {
            let memory = self.manifests.lock().unwrap();
            if let Some(held) = memory
                .get(&key)
                .or_else(|| memory.get(&latest).filter(|h| exact(h)))
            {
                if held.body.is_some()
                    || self.clock.now().saturating_sub(held.checked_at) < MANIFEST_RECHECK_SECS
                {
                    return Ok(held.clone());
                }
            }
        }
        let _turn = self.turn(&key).await;
        if let Some(held) = self.manifests.lock().unwrap().get(&key).cloned() {
            if held.body.is_some()
                || self.clock.now().saturating_sub(held.checked_at) < MANIFEST_RECHECK_SECS
            {
                return Ok(held);
            }
        }
        for disk_key in [&key, &latest] {
            if let Some(held) = self
                .manifest_from_disk(copy, disk_key)
                .await
                .filter(|h| exact(h))
            {
                let held = Arc::new(held);
                self.remember_key(key.clone(), held.clone());
                return Ok(held);
            }
        }
        let bytes = match self
            .owner
            .signed(&format!("/media/v1/manifest/{copy}?rev={revision}"))
            .await
        {
            Ok(bytes) => bytes,
            Err(FetchError::Status(StatusCode::NOT_FOUND)) => {
                let held = Arc::new(Held {
                    body: None,
                    checked_at: self.clock.now(),
                });
                self.remember_key(key, held.clone());
                return Ok(held);
            }
            Err(error) => return Err(fetch_failure(error)),
        };
        let body = verify_manifest(&bytes, &self.pins).map_err(|error| {
            tracing::warn!(%error, copy, revision, "pack manifest failed signature verification");
            Failure::BadUpstream
        })?;
        if body.copy_id != copy || body.revision != i64::from(revision) {
            return Err(Failure::BadUpstream);
        }
        // Revision history has its own bounded store key; it never overwrites latest.
        if let Err(error) = self.store.put(&key, bytes).await {
            tracing::warn!(%error, "media cache could not store a revision manifest");
        }
        let held = Arc::new(Held {
            body: Some(body),
            checked_at: self.clock.now(),
        });
        self.remember_key(key, held.clone());
        Ok(held)
    }
}

fn fetch_failure(error: FetchError) -> Failure {
    tracing::warn!(%error, "media cache pack owner request failed");
    match error {
        FetchError::Status(StatusCode::NOT_FOUND) => Failure::NotFound,
        FetchError::TooLarge => Failure::BadUpstream,
        _ => Failure::Unavailable,
    }
}

enum Input {
    Disk(tokio::fs::File, u64),
    Owner(owner::Download, Bytes),
}

impl Input {
    async fn exact(&mut self, length: u64) -> io::Result<Vec<u8>> {
        let length = usize::try_from(length).map_err(invalid)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(invalid)?;
        bytes.resize(length, 0);
        match self {
            Self::Disk(file, _) => {
                file.read_exact(&mut bytes).await?;
            }
            Self::Owner(download, pending) => {
                let mut filled = 0;
                while filled < length {
                    if pending.is_empty() {
                        *pending = download.chunk().await.map_err(invalid)?.ok_or_else(|| {
                            io::Error::new(io::ErrorKind::UnexpectedEof, "short owner pack")
                        })?;
                        if pending.is_empty() {
                            continue;
                        }
                    }
                    let take = pending.len().min(length - filled);
                    bytes[filled..filled + take].copy_from_slice(&pending.split_to(take));
                    filled += take;
                }
            }
        }
        Ok(bytes)
    }

    async fn end(&mut self) -> io::Result<()> {
        match self {
            Self::Disk(file, _) => {
                if file.read(&mut [0_u8; 1]).await? != 0 {
                    return Err(invalid("long stored pack"));
                }
            }
            Self::Owner(download, pending) => {
                if !pending.is_empty() {
                    return Err(invalid("long owner pack"));
                }
                while let Some(bytes) = download.chunk().await.map_err(invalid)? {
                    if !bytes.is_empty() {
                        return Err(invalid("long owner pack"));
                    }
                }
            }
        }
        Ok(())
    }

    fn fetched(&self) -> u64 {
        match self {
            Self::Disk(..) => 0,
            Self::Owner(download, _) => download.fetched,
        }
    }
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

struct Delivery {
    cache: Arc<MediaCache>,
    key: Key,
    manifest: ManifestBody,
    pack: Pack,
    input: Option<Input>,
    pending: Option<store::Pending>,
    turn: Option<Turn>,
    header: Option<Bytes>,
    position: usize,
    served: u64,
    fetched: u64,
    hit: bool,
    finished: bool,
}

impl Delivery {
    async fn allowed(&mut self) -> io::Result<()> {
        self.cache
            .allow(&self.manifest.copy_id, self.manifest.policy_epoch)
            .await
            .map(|_| ())
            .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "pack policy refused"))
    }

    async fn complete(&mut self) -> io::Result<()> {
        self.input.as_mut().unwrap().end().await?;
        self.allowed().await?;
        if let Some(mut pending) = self.pending.take() {
            pending.sync().await.map_err(invalid)?;
            self.allowed().await?;
            pending.finish().map_err(invalid)?;
        }
        self.fetched = self.input.as_ref().unwrap().fetched();
        self.input.take();
        self.turn.take();
        self.finished = true;
        Ok(())
    }

    async fn next(&mut self) -> io::Result<Option<Bytes>> {
        if self.finished {
            return Ok(None);
        }
        self.allowed().await?;
        if let Some(header) = self.header.take() {
            if self.pack.unique.is_empty() {
                self.complete().await?;
            }
            self.served += header.len() as u64;
            return Ok(Some(header));
        }
        let page = &self.manifest.pages[self.pack.unique[self.position]];
        let bytes = self.input.as_mut().unwrap().exact(page.length).await?;
        // The exact canonical header authenticates the MIME metadata; pack objects
        // have no separate HTTP Content-Type. Use the same object verifier as hits.
        verify_object(page, &bytes, &page.mime).map_err(invalid)?;
        if let Some(pending) = &mut self.pending {
            pending.write(&bytes).await.map_err(invalid)?;
        }
        self.allowed().await?;
        self.position += 1;
        if self.position == self.pack.unique.len() {
            self.complete().await?;
        }
        self.served += bytes.len() as u64;
        Ok(Some(bytes.into()))
    }

    /// End the body after `kind` of failure. A stored pack is removed only when its
    /// bytes failed (verification or I/O); a policy or lease refusal leaves it intact.
    fn abort(&mut self, kind: io::ErrorKind) {
        if let Some(input) = self.input.take() {
            self.fetched = input.fetched();
            if let Input::Disk(_, generation) = input {
                if kind != io::ErrorKind::PermissionDenied {
                    self.cache.store.remove(&self.key, generation);
                }
            }
        }
        self.pending.take();
        self.turn.take();
        self.finished = true;
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        let fetched = self.input.as_ref().map_or(self.fetched, Input::fetched);
        tracing::info!(key = ?self.key, hit = self.hit, bytes_served = self.served,
            bytes_fetched = fetched, bytes_aborted = self.pack.total_len - self.served,
            "media cache pack delivery");
    }
}

type PackHeaders = [(header::HeaderName, HeaderValue); 6];

/// A request answered before any body: a HEAD's headers, or a failure.
enum Early {
    Head(Box<PackHeaders>),
    Failed(Failure),
}

impl From<Failure> for Early {
    fn from(failure: Failure) -> Self {
        Self::Failed(failure)
    }
}

fn pack_headers(total_len: u64, s_maxage: i64, hit: bool) -> PackHeaders {
    [
        (
            header::CONTENT_TYPE,
            HeaderValue::from_static(PACK_CONTENT_TYPE),
        ),
        (header::CONTENT_LENGTH, HeaderValue::from(total_len)),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_str(&format!(
                "public, max-age=31536000, immutable, s-maxage={s_maxage}"
            ))
            .unwrap(),
        ),
        (
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox; default-src 'none'"),
        ),
        (
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        ),
        (
            header::HeaderName::from_static("x-komika-cache"),
            HeaderValue::from_static(if hit { "hit" } else { "miss" }),
        ),
    ]
}

pub(super) async fn serve(
    State(cache): State<Arc<MediaCache>>,
    method: Method,
    path: Result<UrlPath<(String, String)>, axum::extract::rejection::PathRejection>,
) -> axum::response::Response {
    let Ok(UrlPath((copy, raw_revision))) = path else {
        return failure_response(Failure::NotFound);
    };
    let revision = match raw_revision.parse::<i32>() {
        Ok(rev) if rev > 0 && crate::valid_media_copy(&copy) => rev,
        _ => return failure_response(Failure::NotFound),
    };
    let prepared = async {
        // Refuse denials and unusable leases before even asking for a manifest.
        cache.allow(&copy, i64::MIN).await?;
        let held = cache.revision(&copy, revision).await?;
        let manifest = held.body.as_ref().ok_or(Failure::NotFound)?.clone();
        let s_maxage = cache.allow(&copy, manifest.policy_epoch).await?;
        let pack = build_index(&manifest).map_err(|_| Failure::BadUpstream)?;
        let key = Key::Pack(copy.clone(), revision);
        if method == Method::HEAD {
            // Headers from the signed manifest alone: no owner stream, no temp file.
            return Err(Early::Head(Box::new(pack_headers(
                pack.total_len,
                s_maxage,
                cache.store.contains(&key),
            ))));
        }
        let turn = tokio::time::timeout(PACK_TURN_WAIT, cache.turn(&key))
            .await
            .map_err(|_| {
                tracing::warn!(?key, "media cache pack fill still running; gave up waiting");
                Failure::Unavailable
            })?;
        cache.allow(&copy, manifest.policy_epoch).await?;
        let stored = cache
            .store
            .open_file(&key)
            .map_err(|_| Failure::Unavailable)?;
        let hit = stored.is_some();
        let mut input = match stored {
            Some((file, generation)) => Input::Disk(file, generation),
            None => Input::Owner(
                cache
                    .owner
                    .stream(&format!("/media/v1/p/{copy}/{revision}"), pack.total_len)
                    .await
                    .map_err(fetch_failure)?,
                Bytes::new(),
            ),
        };
        let checked = async {
            let mut header = input.exact(8).await?;
            if header != pack.header[..8] {
                return Err(invalid("wrong pack prefix"));
            }
            header.extend_from_slice(&input.exact((pack.header.len() - 8) as u64).await?);
            parse_header(&header).map_err(invalid)?;
            if header != pack.header {
                return Err(invalid("wrong pack header"));
            }
            Ok::<_, io::Error>(())
        }
        .await;
        if let Err(error) = checked {
            if let Input::Disk(_, generation) = &input {
                cache.store.remove(&key, *generation);
            }
            tracing::warn!(%error, ?key, hit, bytes_served = 0,
                bytes_fetched = input.fetched(), bytes_aborted = pack.total_len,
                "media cache pack header refused");
            return Err(Failure::BadUpstream.into());
        }
        let mut pending = if hit {
            None
        } else {
            cache
                .store
                .begin(key.clone(), pack.total_len)
                .map_err(|_| Failure::Unavailable)?
        };
        if let Some(pending) = &mut pending {
            pending
                .write(&pack.header)
                .await
                .map_err(|_| Failure::Unavailable)?;
        }
        let s_maxage = cache.allow(&copy, manifest.policy_epoch).await?;
        Ok((
            Delivery {
                cache: cache.clone(),
                key,
                manifest,
                header: Some(Bytes::copy_from_slice(&pack.header)),
                pack,
                input: Some(input),
                pending,
                // A hit has its file open already: later hits must not wait for this body.
                turn: if hit { None } else { Some(turn) },
                position: 0,
                served: 0,
                fetched: 0,
                hit,
                finished: false,
            },
            s_maxage,
        ))
    }
    .await;
    let (delivery, s_maxage) = match prepared {
        Ok(value) => value,
        Err(Early::Head(headers)) => return (*headers, Body::empty()).into_response(),
        Err(Early::Failed(failure)) => return failure_response(failure),
    };
    let total_len = delivery.pack.total_len;
    let hit = delivery.hit;
    let body = Body::from_stream(futures::stream::unfold(
        delivery,
        |mut delivery| async move {
            match delivery.next().await {
                Ok(Some(bytes)) => Some((Ok(bytes), delivery)),
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(%error, key = ?delivery.key, "media cache pack body aborted");
                    delivery.abort(error.kind());
                    Some((Err(error), delivery))
                }
            }
        },
    ));
    (pack_headers(total_len, s_maxage, hit), body).into_response()
}
