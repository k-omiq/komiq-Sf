//! Validate source image envelopes before publishing cacheable bytes. The fetch
//! slot belongs to the payload until its last owner drops; delivery to a client is
//! bounded in time, so a slow or stalled reader cannot keep that owner alive.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Result};
use axum::body::{Body, Bytes};
use image::ImageFormat;
use tokio::sync::{oneshot, Notify, OwnedSemaphorePermit};

use crate::telemetry::{BodyCheck, BodySource, MEDIA};

pub(crate) const MUTABLE_CACHE: &str = "public, max-age=300, must-revalidate";
/// Chapter pages under `/media/v1/c/{epoch}/…`. The engine generation is in the URL, so a
/// chapter id that a later generation reuses is never answered from this entry. A source
/// can still re-upload a chapter's pages: one day bounds that, and the strong ETag makes
/// revalidating it a 304.
pub(crate) const GENERATION_PAGE_CACHE: &str = "public, max-age=86400, s-maxage=86400";
/// Current cover and avatar versions. Browsers keep them for a year; a shared cache
/// keeps them no longer than a media policy lease, so a takedown reaches the edge
/// within one lease. The `s-maxage` below must equal `media_policy::LEASE_SECS`.
pub(crate) const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable, s-maxage=600";

/// The trailing `s-maxage=<n>` of a cache header, at compile time.
const fn trailing_s_maxage(header: &str) -> i64 {
    let bytes = header.as_bytes();
    let mut end = bytes.len();
    let mut value = 0;
    let mut scale = 1;
    while end > 0 && bytes[end - 1].is_ascii_digit() {
        value += (bytes[end - 1] - b'0') as i64 * scale;
        scale *= 10;
        end -= 1;
    }
    value
}

const _: () = assert!(
    trailing_s_maxage(IMMUTABLE_CACHE) == crate::media_policy::LEASE_SECS,
    "IMMUTABLE_CACHE s-maxage must follow media_policy::LEASE_SECS"
);

struct ImageOwner {
    bytes: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}

impl AsRef<[u8]> for ImageOwner {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// The retained-payload gauges follow the owner, so they fall exactly when the
/// fetch slot is released.
impl Drop for ImageOwner {
    fn drop(&mut self) {
        MEDIA.release(self.bytes.len());
    }
}

pub(crate) fn retain_fetch_slot(bytes: Vec<u8>, permit: OwnedSemaphorePermit) -> Bytes {
    MEDIA.retain(bytes.len());
    Bytes::from_owner(ImageOwner {
        bytes,
        _permit: permit,
    })
}

/// Copied chunk size handed to the HTTP transport. Copies, not slices: a slice of a
/// slot-owning payload would keep the whole image and its fetch slot alive inside
/// the connection's write queue for as long as the client refuses to read.
const DELIVERY_CHUNK: usize = 64 * 1024;
/// A client that takes no further bytes for this long has stalled.
const DELIVERY_IDLE: Duration = Duration::from_secs(20);
/// Every client gets this long plus [`DELIVERY_MIN_RATE`] worth of the image.
const DELIVERY_BASE: Duration = Duration::from_secs(30);
/// Bytes per second a slow client must sustain before the total deadline applies.
const DELIVERY_MIN_RATE: u64 = 256 * 1024;

/// Response body for a payload that may own a fetch slot. A slow or stalled client
/// cannot pin the slot or the buffered image: once it stops taking bytes for
/// [`DELIVERY_IDLE`], or misses a size-scaled total deadline, the payload is dropped
/// and the connection is aborted. The transport retains at most its own small write
/// buffer of copied chunks. The caller sets `Content-Length`.
pub(crate) fn delivery_body(bytes: Bytes) -> Body {
    let total = DELIVERY_BASE + Duration::from_secs(bytes.len() as u64 / DELIVERY_MIN_RATE);
    bounded_delivery(bytes, DELIVERY_IDLE, total)
}

fn bounded_delivery(bytes: Bytes, idle: Duration, total: Duration) -> Body {
    let (done, finished) = oneshot::channel::<()>();
    Body::from_stream(Delivery {
        len: bytes.len(),
        payload: Arc::new(std::sync::Mutex::new(Some(bytes))),
        progress: Arc::new(Notify::new()),
        offset: 0,
        expiry: Some(Expiry {
            idle,
            total,
            finished,
        }),
        _done: done,
    })
}

struct Delivery {
    payload: Arc<std::sync::Mutex<Option<Bytes>>>,
    progress: Arc<Notify>,
    offset: usize,
    len: usize,
    /// Armed on the first poll that leaves bytes behind: a response dropped unsent
    /// (HEAD, cancellation) or sent in one chunk never needs a timer.
    expiry: Option<Expiry>,
    _done: oneshot::Sender<()>,
}

struct Expiry {
    idle: Duration,
    total: Duration,
    finished: oneshot::Receiver<()>,
}

impl Delivery {
    fn arm(&mut self) {
        let Some(Expiry {
            idle,
            total,
            mut finished,
        }) = self.expiry.take()
        else {
            return;
        };
        let payload = self.payload.clone();
        let progress = self.progress.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::sleep(total);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    // The body finished or was dropped: nothing left to expire.
                    _ = &mut finished => return,
                    _ = &mut deadline => break,
                    _ = progress.notified() => {}
                    _ = tokio::time::sleep(idle) => break,
                }
            }
            // A payload already handed over in full is not an aborted delivery.
            if payload
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .is_some()
            {
                MEDIA.delivery_aborted();
            }
        });
    }
}

impl futures::Stream for Delivery {
    type Item = std::io::Result<Bytes>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.offset >= this.len {
            return std::task::Poll::Ready(None);
        }
        let end = (this.offset + DELIVERY_CHUNK).min(this.len);
        if end < this.len {
            // More than this poll's chunk remains, so the client can now stall.
            this.arm();
        }
        let mut payload = this.payload.lock().unwrap_or_else(|e| e.into_inner());
        let Some(bytes) = payload.as_ref() else {
            return std::task::Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "image client stopped reading",
            ))));
        };
        let chunk = Bytes::copy_from_slice(&bytes[this.offset..end]);
        this.offset = end;
        if end == this.len {
            // Release the slot as soon as the last byte is handed over.
            payload.take();
        }
        drop(payload);
        this.progress.notify_one();
        MEDIA.delivered(chunk.len());
        std::task::Poll::Ready(Some(Ok(chunk)))
    }
}

/// Check MIME against the actual raster format, dimensions where our codecs
/// support them, and complete container framing. This does not allocate decoded
/// pixels or claim to authenticate source art; signed digests remain future work.
pub(crate) fn validate(bytes: &[u8], content_type: &str) -> Result<&'static str> {
    ensure!(!bytes.is_empty(), "empty image body");
    let format = image::guess_format(bytes).map_err(|_| anyhow!("unsupported image body"))?;
    let Some(mime) = raster_mime(format) else {
        bail!("unsupported image format");
    };
    ensure!(
        declared_matches(content_type, mime),
        "image Content-Type does not match its body"
    );
    crate::avatar::ensure_complete(bytes)?;
    match format {
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP | ImageFormat::Gif => {
            let reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
            let (width, height) = reader.into_dimensions()?;
            ensure!(width > 0 && height > 0, "zero image dimensions");
        }
        _ => {}
    }
    match format {
        ImageFormat::Png => validate_png(bytes)?,
        ImageFormat::WebP => validate_webp(bytes)?,
        ImageFormat::Gif => validate_gif(bytes)?,
        ImageFormat::Avif => validate_avif(bytes)?,
        ImageFormat::Bmp => {
            ensure!(bytes.len() >= 26, "truncated BMP header");
            let length = le_u32(bytes, 2)?;
            let pixels = le_u32(bytes, 10)?;
            ensure!(
                length <= bytes.len() && pixels >= 14 && pixels < length,
                "truncated BMP body"
            );
        }
        ImageFormat::Ico => {
            ensure!(bytes.len() >= 6, "truncated ICO header");
            let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
            ensure!(
                count > 0 && 6 + count * 16 <= bytes.len(),
                "truncated ICO directory"
            );
            for entry in (6..6 + count * 16).step_by(16) {
                let length = le_u32(bytes, entry + 8)?;
                let offset = le_u32(bytes, entry + 12)?;
                ensure!(
                    length > 0
                        && offset >= 6 + count * 16
                        && offset
                            .checked_add(length)
                            .is_some_and(|end| end <= bytes.len()),
                    "truncated ICO image"
                );
            }
        }
        _ => {}
    }
    Ok(mime)
}

/// [`validate`], counted in the media body-check telemetry for `source`.
pub(crate) fn validate_from(
    source: BodySource,
    bytes: &[u8],
    content_type: &str,
) -> Result<&'static str> {
    let result = validate(bytes, content_type);
    let check = match result {
        Ok(_) => BodyCheck::Ok,
        Err(_) => refusal_kind(bytes, content_type),
    };
    MEDIA.body_check(source, check);
    result
}

/// Which of `validate`'s checks refused, re-derived from the same helpers.
fn refusal_kind(bytes: &[u8], content_type: &str) -> BodyCheck {
    if bytes.is_empty() {
        return BodyCheck::Empty;
    }
    match image::guess_format(bytes).ok().and_then(raster_mime) {
        None => BodyCheck::Unsupported,
        Some(mime) if !declared_matches(content_type, mime) => BodyCheck::Mismatched,
        Some(_) => BodyCheck::Truncated,
    }
}

fn raster_mime(format: ImageFormat) -> Option<&'static str> {
    Some(match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
        ImageFormat::WebP => "image/webp",
        ImageFormat::Gif => "image/gif",
        ImageFormat::Avif => "image/avif",
        ImageFormat::Bmp => "image/bmp",
        ImageFormat::Ico => "image/x-icon",
        _ => return None,
    })
}

fn declared_matches(content_type: &str, mime: &str) -> bool {
    let declared = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    declared.is_empty()
        || declared == "application/octet-stream"
        || declared == mime
        || (mime == "image/x-icon" && declared == "image/vnd.microsoft.icon")
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<usize> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| anyhow!("truncated image header"))?
            .try_into()?,
    ) as usize)
}

fn validate_png(bytes: &[u8]) -> Result<()> {
    let mut cursor = 8;
    let mut data = false;
    while cursor + 12 <= bytes.len() {
        let length = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into()?) as usize;
        let end = cursor
            .checked_add(length)
            .and_then(|n| n.checked_add(12))
            .filter(|n| *n <= bytes.len())
            .ok_or_else(|| anyhow!("truncated PNG chunk"))?;
        let kind = &bytes[cursor + 4..cursor + 8];
        if cursor == 8 {
            ensure!(kind == b"IHDR" && length == 13, "invalid PNG header");
        }
        if kind == b"IDAT" {
            data = true;
        }
        if kind == b"IEND" {
            ensure!(length == 0 && data, "invalid PNG terminator");
            return Ok(());
        }
        cursor = end;
    }
    bail!("truncated PNG chunks")
}

fn validate_webp(bytes: &[u8]) -> Result<()> {
    let end = le_u32(bytes, 4)?
        .checked_add(8)
        .filter(|n| *n <= bytes.len())
        .ok_or_else(|| anyhow!("truncated WebP container"))?;
    let mut cursor = 12;
    let mut data = false;
    while cursor + 8 <= end {
        let length = le_u32(bytes, cursor + 4)?;
        let kind = &bytes[cursor..cursor + 4];
        data |= matches!(kind, b"VP8 " | b"VP8L" | b"ANMF");
        cursor = cursor
            .checked_add(8)
            .and_then(|n| n.checked_add(length + (length & 1)))
            .filter(|n| *n <= end)
            .ok_or_else(|| anyhow!("truncated WebP chunk"))?;
    }
    ensure!(cursor == end && data, "incomplete WebP chunks");
    Ok(())
}

fn gif_subblocks(bytes: &[u8], cursor: &mut usize) -> Result<()> {
    loop {
        let length = *bytes
            .get(*cursor)
            .ok_or_else(|| anyhow!("truncated GIF subblock"))? as usize;
        *cursor += 1;
        *cursor = cursor
            .checked_add(length)
            .filter(|n| *n <= bytes.len())
            .ok_or_else(|| anyhow!("truncated GIF subblock"))?;
        if length == 0 {
            return Ok(());
        }
    }
}

fn validate_gif(bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() >= 13, "truncated GIF header");
    let table = |packed: u8| {
        if packed & 128 != 0 {
            3usize * (2usize << (packed & 7))
        } else {
            0
        }
    };
    let mut cursor = 13 + table(bytes[10]);
    let mut image = false;
    loop {
        let tag = *bytes
            .get(cursor)
            .ok_or_else(|| anyhow!("truncated GIF body"))?;
        cursor += 1;
        match tag {
            0x3b => {
                ensure!(image, "GIF has no image");
                return Ok(());
            }
            0x21 => {
                ensure!(cursor < bytes.len(), "truncated GIF extension");
                cursor += 1;
            }
            0x2c => {
                ensure!(cursor + 9 <= bytes.len(), "truncated GIF image header");
                cursor += 9 + table(bytes[cursor + 8]);
                ensure!(cursor < bytes.len(), "truncated GIF image data");
                cursor += 1; // LZW minimum code size.
                image = true;
            }
            _ => bail!("invalid GIF block"),
        }
        gif_subblocks(bytes, &mut cursor)?;
    }
}

fn validate_avif(bytes: &[u8]) -> Result<()> {
    let mut cursor = 0;
    let mut meta = false;
    while cursor + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[cursor..cursor + 4].try_into()?) as usize;
        let header = if size == 1 { 16 } else { 8 };
        let size = match size {
            0 => bytes.len() - cursor,
            1 => {
                ensure!(cursor + 16 <= bytes.len(), "truncated AVIF box");
                usize::try_from(u64::from_be_bytes(
                    bytes[cursor + 8..cursor + 16].try_into()?,
                ))?
            }
            n => n,
        };
        ensure!(size >= header, "invalid AVIF box length");
        meta |= &bytes[cursor + 4..cursor + 8] == b"meta";
        cursor = cursor
            .checked_add(size)
            .filter(|n| *n <= bytes.len())
            .ok_or_else(|| anyhow!("truncated AVIF box"))?;
    }
    ensure!(cursor == bytes.len() && meta, "incomplete AVIF container");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_and_incomplete_source_bodies_are_rejected() {
        assert!(validate(b"", "image/jpeg").is_err());
        assert!(validate(b"<html>challenge</html>", "image/jpeg").is_err());
        assert!(validate(&[0xff, 0xd8, 0xff, 0xe0, 0, 16], "image/jpeg").is_err());
        let jpeg = crate::suwayomi::testsrv::jpeg(10, 12);
        assert!(validate(&jpeg, "text/html").is_err());
        assert!(validate(&jpeg, "image/png").is_err());
        assert_eq!(validate(&jpeg, "").unwrap(), "image/jpeg");
        assert_eq!(
            validate(&jpeg, "IMAGE/JPEG; charset=binary").unwrap(),
            "image/jpeg"
        );
    }

    #[test]
    fn supported_codec_outputs_have_complete_containers() {
        let image = image::DynamicImage::new_rgba8(2, 3);
        for (format, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::WebP, "image/webp"),
            (ImageFormat::Gif, "image/gif"),
        ] {
            let mut bytes = std::io::Cursor::new(Vec::new());
            image.write_to(&mut bytes, format).unwrap();
            let bytes = bytes.into_inner();
            assert_eq!(validate(&bytes, mime).unwrap(), mime);
            assert!(validate(&bytes[..bytes.len() - 1], mime).is_err(), "{mime}");
        }
    }

    /// 2x3 images written by Pillow 11.3 (libavif for AVIF), base64. The server has
    /// no AVIF/BMP/ICO codecs, so these are real encoder outputs rather than images
    /// this crate produced for itself.
    const AVIF_2X3: &str = "AAAAIGZ0eXBhdmlmAAAAAGF2aWZtaWYxbWlhZk1BMUIAAADrbWV0YQAAAAAAAAAhaGRscgAAAAAAAAAAcGljdAAAAAAAAAAAAAAAAAAAAAAOcGl0bQAAAAAAAQAAAB5pbG9jAAAAAEQAAAEAAQAAAAEAAAETAAAAGQAAAChpaW5mAAAAAAABAAAAGmluZmUCAAAAAAEAAGF2MDFDb2xvcgAAAABqaXBycAAAAEtpcGNvAAAAFGlzcGUAAAAAAAAAAgAAAAMAAAAQcGl4aQAAAAADCAgIAAAADGF2MUOBAAwAAAAAE2NvbHJuY2x4AAEADQAGgAAAABdpcG1hAAAAAAAAAAEAAQQBAoMEAAAAIW1kYXQSAAoFGABzAhAyDhgACiiihAAAsBOvz87o";
    const BMP_2X3: &str = "Qk1OAAAAAAAAADYAAAAoAAAAAgAAAAMAAAABABgAAAAAABgAAADEDgAAxA4AAAAAAAAAAAAAWijIWijIAABaKMhaKMgAAFooyFooyAAA";
    const ICO_2X3: &str = "AAABAAEAAgMAAAAAIABNAAAAFgAAAIlQTkcNChoKAAAADUlIRFIAAAACAAAAAwgCAAAANohJ1gAAABRJREFUeJxjPKERxcDAwMQABlAKABlAAVCRI4itAAAAAElFTkSuQmCC";

    #[test]
    fn external_avif_bmp_and_ico_fixtures_are_accepted_and_truncations_refused() {
        use base64::Engine;
        for (fixture, mime, alias) in [
            (AVIF_2X3, "image/avif", "image/avif"),
            (BMP_2X3, "image/bmp", "image/bmp"),
            (ICO_2X3, "image/x-icon", "image/vnd.microsoft.icon"),
        ] {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(fixture)
                .unwrap();
            assert_eq!(validate(&bytes, mime).unwrap(), mime);
            assert_eq!(validate(&bytes, alias).unwrap(), mime);
            assert_eq!(validate(&bytes, "").unwrap(), mime);
            assert!(validate(&bytes, "image/png").is_err(), "{mime} mislabeled");
            assert!(validate(&bytes, "text/html").is_err(), "{mime} as HTML");
            assert!(
                validate(&bytes[..bytes.len() - 1], mime).is_err(),
                "{mime} truncated"
            );
        }
    }

    async fn slot_payload(len: usize) -> (std::sync::Arc<tokio::sync::Semaphore>, Bytes) {
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let bytes = retain_fetch_slot(
            (0..len).map(|i| i as u8).collect(),
            slots.clone().acquire_owned().await.unwrap(),
        );
        (slots, bytes)
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_trickling_and_dropped_deliveries_release_the_fetch_slot() {
        use futures::StreamExt;
        // Stalled: one chunk, then the client stops reading.
        let (slots, bytes) = slot_payload(3 * DELIVERY_CHUNK).await;
        let mut body = bounded_delivery(bytes, Duration::from_secs(1), Duration::from_secs(60))
            .into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap().len(), DELIVERY_CHUNK);
        assert_eq!(slots.available_permits(), 0);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            slots.available_permits(),
            1,
            "a stalled client pinned the slot"
        );
        assert!(
            body.next().await.unwrap().is_err(),
            "expired delivery must abort"
        );

        // Trickling: never idle, but the total deadline still applies.
        let (slots, bytes) = slot_payload(100 * DELIVERY_CHUNK).await;
        let mut body = bounded_delivery(bytes, Duration::from_secs(1), Duration::from_secs(3))
            .into_data_stream();
        let mut delivered = 0;
        while let Some(Ok(_)) = body.next().await {
            delivered += 1;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!((5..=7).contains(&delivered), "{delivered} chunks");
        assert_eq!(slots.available_permits(), 1);

        // Cancelled before a byte was read: the payload goes with the body.
        let (slots, bytes) = slot_payload(DELIVERY_CHUNK).await;
        drop(bounded_delivery(
            bytes,
            Duration::from_secs(60),
            Duration::from_secs(60),
        ));
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn completed_delivery_is_exact_and_releases_on_the_last_chunk() {
        use futures::StreamExt;
        let len = 2 * DELIVERY_CHUNK + 7;
        let (slots, bytes) = slot_payload(len).await;
        let expected = bytes.to_vec();
        let mut body = delivery_body(bytes).into_data_stream();
        let mut received = Vec::new();
        while received.len() < len {
            received.extend_from_slice(&body.next().await.unwrap().unwrap());
        }
        assert_eq!(received, expected);
        assert_eq!(
            slots.available_permits(),
            1,
            "the last chunk must release the slot before end-of-stream"
        );
        assert!(body.next().await.is_none());
    }

    /// Through a real HTTP connection: a client that reads the head and then stops
    /// reading must not keep the fetch slot; a normal client gets every byte.
    #[tokio::test]
    async fn real_stalled_connection_does_not_pin_the_fetch_slot() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let len = 16 * 1024 * 1024;
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let app = {
            let slots = slots.clone();
            axum::Router::new().route(
                "/image",
                axum::routing::get(move || {
                    let slots = slots.clone();
                    async move {
                        let permit = slots.try_acquire_owned().unwrap();
                        let bytes = retain_fetch_slot(vec![7u8; len], permit);
                        (
                            [(axum::http::header::CONTENT_LENGTH, bytes.len())],
                            bounded_delivery(
                                bytes,
                                Duration::from_millis(300),
                                Duration::from_secs(30),
                            ),
                        )
                    }
                }),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut stalled = tokio::net::TcpStream::connect(addr).await.unwrap();
        stalled
            .write_all(b"GET /image HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut head = [0u8; 512];
        let read = stalled.read(&mut head).await.unwrap();
        assert!(String::from_utf8_lossy(&head[..read]).contains("content-length: 16777216"));
        tokio::time::timeout(Duration::from_secs(10), async {
            while slots.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("a stalled client kept the fetch slot");

        let body = reqwest::get(format!("http://{addr}/image"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(body.len(), len);
        assert!(body.iter().all(|byte| *byte == 7));
        assert_eq!(slots.available_permits(), 1);
        drop(stalled);
        server.abort();
    }

    /// The media telemetry is process-global and other tests move it, so these
    /// assert lower bounds only.
    #[tokio::test(start_paused = true)]
    async fn telemetry_counts_refused_bodies_retained_payloads_and_aborted_deliveries() {
        use futures::StreamExt;
        let check = |result| MEDIA.body_checks(BodySource::Proxy, result);
        let before: Vec<u64> = [
            BodyCheck::Ok,
            BodyCheck::Empty,
            BodyCheck::Unsupported,
            BodyCheck::Mismatched,
            BodyCheck::Truncated,
        ]
        .into_iter()
        .map(check)
        .collect();
        let jpeg = crate::suwayomi::testsrv::jpeg(10, 12);
        assert!(validate_from(BodySource::Proxy, &jpeg, "image/jpeg").is_ok());
        assert!(validate_from(BodySource::Proxy, b"", "image/jpeg").is_err());
        assert!(validate_from(BodySource::Proxy, b"<html></html>", "image/jpeg").is_err());
        assert!(validate_from(BodySource::Proxy, &jpeg, "image/png").is_err());
        assert!(validate_from(BodySource::Proxy, &jpeg[..jpeg.len() - 2], "image/jpeg").is_err());
        for (index, result) in [
            BodyCheck::Ok,
            BodyCheck::Empty,
            BodyCheck::Unsupported,
            BodyCheck::Mismatched,
            BodyCheck::Truncated,
        ]
        .into_iter()
        .enumerate()
        {
            assert!(check(result) > before[index], "{result:?} not counted");
        }

        let (_, aborts_before) = MEDIA.delivery();
        let (slots, bytes) = slot_payload(3 * DELIVERY_CHUNK).await;
        let (owners, retained) = MEDIA.retained();
        assert!(owners >= 1 && retained >= 3 * DELIVERY_CHUNK as u64);
        let mut body = bounded_delivery(bytes, Duration::from_secs(1), Duration::from_secs(60))
            .into_data_stream();
        assert_eq!(body.next().await.unwrap().unwrap().len(), DELIVERY_CHUNK);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(slots.available_permits(), 1);
        let (delivered, aborts) = MEDIA.delivery();
        assert!(delivered >= DELIVERY_CHUNK as u64);
        assert!(aborts > aborts_before, "a stalled delivery is an abort");
    }

    #[tokio::test]
    async fn transport_payload_clones_keep_the_fetch_slot() {
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let bytes = retain_fetch_slot(vec![1, 2, 3], slots.clone().acquire_owned().await.unwrap());
        let clone = bytes.slice(1..);
        let response = axum::body::Body::from(bytes);
        drop(response);
        assert_eq!(slots.available_permits(), 0);
        drop(clone);
        assert_eq!(slots.available_permits(), 1);
    }
}
