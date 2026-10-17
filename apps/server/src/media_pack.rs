//! Version 1 chapter packs: a bounded JSON index followed by exact, unique objects.

use std::{collections::HashMap, io, sync::Arc, time::Duration};

use axum::{
    body::{Body, Bytes},
    extract::{rejection::PathRejection, Path, State},
    http::{header, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

use crate::{graphql::AppState, media_manifest::store, media_verify::ManifestBody, telemetry};

/// The pack's media type, also kept out of response compression by name.
pub(crate) const PACK_CONTENT_TYPE: &str = "application/vnd.komika.pack";
const MAX_INDEX_LEN: usize = 1024 * 1024;
const SEND_IDLE: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub(crate) struct Pack {
    pub header: Vec<u8>,
    pub unique: Vec<usize>,
    pub total_len: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PackPage {
    pub sha256: String,
    pub length: u64,
    pub mime: String,
    pub offset: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PackIndex {
    pub v: u32,
    pub copy_id: String,
    pub revision: i64,
    pub pages: Vec<PackPage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PackError {
    ShortHeader,
    Magic,
    IndexTooLarge,
    Json,
    Version,
    Overflow,
    Manifest,
    Range,
    Hash,
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for PackError {}

// Fallible even for a supplied manifest: an unrepresentable length must never wrap
// into a plausible Content-Length or a page range.
pub(crate) fn build_index(manifest: &ManifestBody) -> Result<Pack, PackError> {
    let mut index = PackIndex {
        v: 1,
        copy_id: manifest.copy_id.clone(),
        revision: manifest.revision,
        pages: Vec::new(),
    };
    let mut offsets: HashMap<&str, (u64, usize)> = HashMap::new();
    let mut unique = Vec::new();
    let mut data_len = 0_u64;
    for (position, page) in manifest.pages.iter().enumerate() {
        let offset = match offsets.get(page.sha256.as_str()) {
            Some(&(offset, first)) => {
                let original = &manifest.pages[first];
                if page.length != original.length || page.mime != original.mime {
                    return Err(PackError::Manifest);
                }
                offset
            }
            None => {
                let offset = data_len;
                data_len = data_len
                    .checked_add(page.length)
                    .ok_or(PackError::Overflow)?;
                offsets.insert(page.sha256.as_str(), (offset, position));
                unique.push(position);
                offset
            }
        };
        index.pages.push(PackPage {
            sha256: page.sha256.clone(),
            length: page.length,
            mime: page.mime.clone(),
            offset,
        });
    }
    let json = serde_json::to_vec(&index).map_err(|_| PackError::Json)?;
    if json.len() > MAX_INDEX_LEN {
        return Err(PackError::IndexTooLarge);
    }
    let mut header = Vec::with_capacity(8 + json.len());
    header.extend_from_slice(b"KPK1");
    header.extend_from_slice(&(json.len() as u32).to_be_bytes());
    header.extend_from_slice(&json);
    let total_len = data_len
        .checked_add(header.len() as u64)
        .ok_or(PackError::Overflow)?;
    Ok(Pack {
        header,
        unique,
        total_len,
    })
}

fn index_end(bytes: &[u8]) -> Result<usize, PackError> {
    let prefix = bytes.get(..8).ok_or(PackError::ShortHeader)?;
    if &prefix[..4] != b"KPK1" {
        return Err(PackError::Magic);
    }
    let length = u32::from_be_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]);
    if u64::from(length) > MAX_INDEX_LEN as u64 {
        return Err(PackError::IndexTooLarge);
    }
    let end = 8_usize
        .checked_add(length as usize)
        .ok_or(PackError::Overflow)?;
    if bytes.len() < end {
        return Err(PackError::ShortHeader);
    }
    Ok(end)
}

pub(crate) fn parse_header(bytes: &[u8]) -> Result<PackIndex, PackError> {
    let end = index_end(bytes)?;
    let index: PackIndex = serde_json::from_slice(&bytes[8..end]).map_err(|_| PackError::Json)?;
    if index.v != 1 {
        return Err(PackError::Version);
    }
    for page in &index.pages {
        page.offset
            .checked_add(page.length)
            .ok_or(PackError::Overflow)?;
    }
    Ok(index)
}

// Kept portable for the regional TypeScript reader. The manifest must already
// have passed signature verification; the pack itself carries no signature.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn verify_pack(
    bytes: &[u8],
    manifest: &ManifestBody,
) -> Result<Vec<Vec<u8>>, PackError> {
    let index = parse_header(bytes)?;
    if manifest.v != 1
        || index.copy_id != manifest.copy_id
        || index.revision != manifest.revision
        || index.pages.len() != manifest.pages.len()
    {
        return Err(PackError::Manifest);
    }
    let data = &bytes[index_end(bytes)?..];
    let expected = build_index(manifest)?;
    let expected_index = parse_header(&expected.header)?;
    let mut pages = Vec::new();
    for ((page, signed), canonical) in index
        .pages
        .iter()
        .zip(&manifest.pages)
        .zip(&expected_index.pages)
    {
        if page.sha256 != signed.sha256 || page.length != signed.length || page.mime != signed.mime
        {
            return Err(PackError::Manifest);
        }
        let end = page
            .offset
            .checked_add(page.length)
            .ok_or(PackError::Overflow)?;
        let start = usize::try_from(page.offset).map_err(|_| PackError::Range)?;
        let end = usize::try_from(end).map_err(|_| PackError::Range)?;
        let object = data.get(start..end).ok_or(PackError::Range)?;
        if hex::encode(Sha256::digest(object)) != page.sha256 {
            return Err(PackError::Hash);
        }
        if page.offset != canonical.offset {
            return Err(PackError::Range);
        }
        pages.push(object.to_vec());
    }
    if data.len() as u64 != expected.total_len - expected.header.len() as u64 {
        return Err(PackError::Range);
    }
    Ok(pages)
}

pub(crate) async fn serve_media_pack(
    State(app): State<Arc<AppState>>,
    method: Method,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    if app.media_manifests.is_none() {
        return crate::image_failure_response(StatusCode::NOT_FOUND);
    }
    let Ok(Path((copy, raw_revision))) = path else {
        return crate::image_failure_response(StatusCode::NOT_FOUND);
    };
    let revision = match raw_revision.parse::<i32>() {
        Ok(revision) if revision > 0 && crate::valid_media_copy(&copy) => revision,
        _ => return crate::image_failure_response(StatusCode::NOT_FOUND),
    };
    if app.suwayomi.wait_for_generation().await.is_err() {
        return crate::page_busy_response();
    }
    let generation = app.suwayomi.read_generation();
    let prepared = async {
        let Some(stored) = store::revision(&app.pool, &copy, revision).await? else {
            return Ok(Err(crate::image_failure_response(StatusCode::NOT_FOUND)));
        };
        if stored.stale {
            return Ok(Err(crate::image_failure_response(StatusCode::NOT_FOUND)));
        }
        let Some((chapter, manga)) = store::chapter_for_copy(&app.pool, &copy).await? else {
            return Ok(Err(crate::image_failure_response(StatusCode::NOT_FOUND)));
        };
        if let Err(response) = allowed(&app, &copy, chapter, manga).await {
            return Ok(Err(response));
        }
        let pack = build_index(&stored.body)?;
        Ok::<_, anyhow::Error>(Ok((stored.body, pack, chapter, manga)))
    }
    .await;
    if app
        .suwayomi
        .validate_read_generation(generation)
        .await
        .is_err()
    {
        return crate::page_busy_response();
    }
    let (manifest, pack, chapter, manga) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(response)) => return response,
        Err(error) => return crate::manifest_store_error(error),
    };
    let s_maxage = crate::media_policy::lease_remaining(chrono::Utc::now().timestamp());
    let headers = [
        (
            header::CONTENT_TYPE,
            HeaderValue::from_static(PACK_CONTENT_TYPE),
        ),
        (header::CONTENT_LENGTH, HeaderValue::from(pack.total_len)),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_str(&format!(
                "public, max-age=31536000, immutable, s-maxage={s_maxage}"
            ))
            .unwrap_or_else(|_| HeaderValue::from_static("no-store")),
        ),
        (
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox; default-src 'none'"),
        ),
    ];
    // HEAD answers from the manifest alone: no engine fetch and no producer task.
    if method == Method::HEAD {
        return (headers, Body::empty()).into_response();
    }
    let (tx, rx) = mpsc::channel(2);
    let (finished, done) = oneshot::channel();
    let delivery = Delivery {
        header: Some(Bytes::from(pack.header)),
        rx,
        done,
        remaining: pack.total_len,
        finished: false,
    };
    tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            _ = tx.closed() => Err(io::Error::other("pack client disconnected")),
            result = async {
                for idx in pack.unique {
                    allowed(&app, &copy, chapter, manga).await.map_err(policy_error)?;
                    let (bytes, _, _) = crate::fetch_chapter_page(&app, chapter, &idx.to_string())
                        .await.map_err(|_| io::Error::other("pack object fetch failed"))?;
                    let page = &manifest.pages[idx];
                    if bytes.len() as u64 != page.length || hex::encode(Sha256::digest(&bytes)) != page.sha256 {
                        // Drop the slot even if marking the revision needs to wait for the store.
                        drop(bytes);
                        let stale = store::mark_stale(&app.pool, &copy, revision).await;
                        if let Some(media) = &app.media_manifests {
                            media.queue.queue_now(chapter);
                        }
                        if let Err(error) = stale {
                            tracing::warn!(%error, "pack could not mark revision stale");
                        }
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "pack object changed"));
                    }
                    let object = Bytes::copy_from_slice(&bytes);
                    drop(bytes);
                    app.suwayomi.validate_read_generation(generation).await
                        .map_err(|_| io::Error::other("pack engine generation changed"))?;
                    // Includes the final object: a policy change during a fetch must
                    // be seen before those bytes are handed to the body.
                    allowed(&app, &copy, chapter, manga).await.map_err(policy_error)?;
                    send_object(&tx, object).await?;
                }
                allowed(&app, &copy, chapter, manga).await.map_err(policy_error)?;
                Ok(())
            } => result,
        };
        // A separate completion channel preserves errors even when the bounded
        // data channel is full at the idle deadline.
        let _ = finished.send(result);
    });
    (
        headers,
        Body::from_stream(futures::stream::unfold(delivery, |mut state| async move {
            if state.finished {
                return None;
            }
            if let Some(header) = state.header.take() {
                state.remaining -= header.len() as u64;
                return Some((Ok(header), state));
            }
            if let Some(object) = state.rx.recv().await {
                state.remaining -= object.len() as u64;
                telemetry::MEDIA.pack_object_sent();
                return Some((Ok(object), state));
            }
            state.finished = true;
            let result = (&mut state.done)
                .await
                .unwrap_or_else(|_| Err(io::Error::other("pack producer stopped")));
            match result {
                Err(error) => Some((Err(error), state)),
                Ok(()) if state.remaining != 0 => Some((
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short pack")),
                    state,
                )),
                Ok(()) => None,
            }
        })),
    )
        .into_response()
}

async fn allowed(app: &AppState, copy: &str, chapter: i64, manga: i64) -> Result<(), Response> {
    let policy = crate::check_page_block(app, manga).await?;
    if policy.denies_copy(copy) || policy.denies_chapter(chapter) {
        return Err(crate::image_failure_response(StatusCode::NOT_FOUND));
    }
    Ok(())
}

fn policy_error(_: Response) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "pack policy refused")
}

async fn send_object(tx: &mpsc::Sender<Bytes>, bytes: Bytes) -> io::Result<()> {
    tokio::time::timeout(SEND_IDLE, tx.send(bytes))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "pack client stopped reading"))?
        .map_err(|_| io::Error::other("pack client disconnected"))
}

struct Delivery {
    header: Option<Bytes>,
    rx: mpsc::Receiver<Bytes>,
    done: oneshot::Receiver<io::Result<()>>,
    remaining: u64,
    finished: bool,
}

impl Drop for Delivery {
    fn drop(&mut self) {
        telemetry::MEDIA.pack_bytes_aborted(self.remaining);
    }
}
