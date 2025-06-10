//! User avatar processing.
//!
//! Uploaded images are normalized to a square, downscaled, and re-encoded as
//! **lossless WebP** (pure-Rust `image` encoder — no libwebp/C dependency). To
//! honour a per-file size budget without a lossy quality knob, we pick the
//! *largest* candidate edge whose encoded size fits the budget (busy photos land
//! on a smaller edge; simple/flat images keep the full edge).
//!
//! Storage is the SQLite `user_avatars` table (a BLOB per user), not the data
//! volume — so avatars are Litestream-replicated with the rest of the DB and any
//! replica can serve any avatar. The upload/serve wiring lives in `main.rs`;
//! this module only turns bytes into budgeted WebP.

use anyhow::{anyhow, bail, Context, Result};
use image::codecs::webp::WebPEncoder;
use image::{imageops, ExtendedColorType, GenericImageView, RgbaImage};

/// Reject raw uploads larger than this before we even decode them — a decode
/// bomb guard and a bound on the work a single request can trigger.
pub const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;

/// Per-avatar size budget for the stored WebP (the "60–70 KB per pic" cap). We
/// keep the largest square that encodes at or under this.
pub const MAX_AVATAR_BYTES: usize = 70 * 1024;

/// Candidate square edge lengths (px), largest first. The first that encodes
/// within [`MAX_AVATAR_BYTES`] wins; if none do, the smallest is used (a 96px
/// lossless avatar is comfortably tiny, so this only trades some sharpness).
const CANDIDATE_EDGES: [u32; 7] = [256, 224, 192, 160, 128, 112, 96];

/// Decode arbitrary uploaded image bytes (JPEG/PNG/WebP), normalize to a square,
/// and re-encode as budgeted lossless WebP. Returns the WebP bytes.
pub fn process_avatar(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.is_empty() {
        bail!("empty upload");
    }
    if bytes.len() > MAX_UPLOAD_BYTES {
        bail!(
            "image too large (max {} MB)",
            MAX_UPLOAD_BYTES / (1024 * 1024)
        );
    }
    let img = decode_limited(bytes)?;
    let square = to_square(&img);

    let mut smallest: Option<Vec<u8>> = None;
    for edge in CANDIDATE_EDGES {
        let resized = imageops::resize(&square, edge, edge, imageops::FilterType::Lanczos3);
        let encoded = encode_lossless(&resized)?;
        if encoded.len() <= MAX_AVATAR_BYTES {
            return Ok(encoded);
        }
        smallest = Some(encoded); // keep the last (smallest edge) as the fallback
    }
    smallest.ok_or_else(|| anyhow!("no candidate size produced"))
}

/// How far back from the end of the file [`ensure_complete`] looks for the container's
/// terminator.
///
/// MEASURED, not guessed. Against the 4,562 real source covers the Suwayomi engine has
/// on disk (`downloads/thumbnails`, 4.1 GB — the exact bytes this pipeline ingests):
///
/// * 3,730 JPEG. 3,689 end exactly on `FFD9`; 41 carry trailing bytes after the EOI,
///   up to **1,373** of them. A 64-byte window therefore FALSE-REJECTS 6 of 3,729
///   perfectly good covers (0.16%) — and a `truncated` verdict is transient, so those
///   covers are re-fetched, re-rejected and never cached on every crawl tick forever.
/// * Dropping the window entirely (reverse-search the whole buffer) is much worse in the
///   other direction: 12.4% of these JPEGs contain an `FFD9` before their real EOI (an
///   EXIF thumbnail's own terminator), so a whole-buffer search accepts **12.3%** of
///   simulated truncations — the exact corruption this gate exists to stop.
/// * 4 KiB is the knee: 0 false rejects on the corpus (3x headroom over the observed
///   1,373-byte maximum) and 0.013% false accepts (3 of 22,374 simulated truncations).
///
/// 465 PNG and 364 WebP in the same corpus terminate cleanly under both the old and the
/// new window, so this only moves the JPEG number.
const TAIL_WINDOW: usize = 4096;

/// Reject an image whose container is *incomplete* (truncated download) BEFORE decoding.
///
/// This exists because `image` 0.25 / `zune-jpeg` returns `Ok` for truncated JPEG data:
/// a body that was cut off mid-scan decodes "successfully" into a correct-size image
/// whose missing rows are filled with the decoder's zero/neutral value — `(0,135,0)`
/// (YCbCr zero-fill) or `(128,128,128)` (neutral DC). Nothing downstream can tell that
/// from real art, so a partial fetch gets re-encoded and frozen into the cover cache
/// with `cover_cached_version` set and a one-year immutable edge TTL. Checking the
/// container's end-of-stream marker catches the truncation directly, at the only point
/// where the information still exists.
///
/// Only formats with an UNAMBIGUOUS terminator are checked, and each check is
/// deliberately permissive about trailing padding — a false reject costs a real cover
/// (`truncated` is transient, so it is never recorded in `work_cover_issue`; instead the
/// work is re-fetched and re-rejected on EVERY crawl tick, forever, and never gets a
/// cover). Unknown/unchecked formats (GIF, AVIF, BMP, …) pass through.
pub(crate) fn ensure_complete(bytes: &[u8]) -> Result<()> {
    // JPEG: SOI `FFD8FF` … EOI `FFD9`. `FF` inside entropy-coded scan data is byte-
    // stuffed as `FF00`, so the literal pair `FFD9` cannot occur inside valid *scan*
    // data. It CAN occur earlier in the file (the EOI of an EXIF thumbnail, or ICC /
    // comment payload bytes), which is why this searches a bounded tail rather than the
    // whole buffer — see `TAIL_WINDOW`.
    if bytes.len() >= 4 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        let tail = &bytes[bytes.len().saturating_sub(TAIL_WINDOW)..];
        let has_eoi = tail.windows(2).any(|w| w == [0xFF, 0xD9]);
        if !has_eoi {
            bail!("truncated JPEG (no EOI marker)");
        }
        return Ok(());
    }
    // WebP: RIFF container declares its own length — `RIFF` + u32 LE size + `WEBP`,
    // where the file is exactly `size + 8` bytes. Exact, so no tolerance needed;
    // trailing bytes past the declared length are fine, missing bytes are not.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        let declared = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        if bytes.len() < declared.saturating_add(8) {
            bail!(
                "truncated WebP (RIFF declares {} bytes, got {})",
                declared + 8,
                bytes.len()
            );
        }
        return Ok(());
    }
    // PNG: must end with the zero-length IEND chunk (len 0, "IEND", CRC AE426082).
    // Same tail window as JPEG, for the same reason (trailing bytes after the terminator
    // are legal and do occur); the 8-byte name+CRC signature makes a false accept far
    // less likely here than for JPEG's 2-byte marker.
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    const IEND: [u8; 8] = [b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82];
    if bytes.len() >= 16 && bytes.starts_with(&PNG_SIG) {
        let tail = &bytes[bytes.len().saturating_sub(TAIL_WINDOW)..];
        if !tail.windows(8).any(|w| w == IEND) {
            bail!("truncated PNG (no IEND chunk)");
        }
        return Ok(());
    }
    Ok(())
}

/// Decode uploaded image bytes with hard decoder limits so a decompression bomb
/// (a tiny, highly-compressible file that expands to gigabytes of RGBA — the
/// `MAX_UPLOAD_BYTES` cap only bounds the *compressed* input) can't OOM the
/// process before we ever downscale. Caps pixel dimensions and the decode
/// allocation; an over-limit image is rejected as a client error, not decoded.
pub(crate) fn decode_limited(bytes: &[u8]) -> Result<image::DynamicImage> {
    // Truncation gate first: a partial JPEG decodes to `Ok` with a flat filler tail, so
    // the decoder cannot be trusted to report this. Applies to every caller of
    // `decode_limited` — avatar upload, `process_cover`, both crawl passes, the lazy
    // `serve_cover` path and the admin cover upload.
    ensure_complete(bytes)?;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .context("unsupported or corrupt image")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(10_000);
    limits.max_image_height = Some(10_000);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().context("image too large or unsupported")
}

/// Center-crop to the largest centered square, as an owned RGBA buffer.
fn to_square(img: &image::DynamicImage) -> RgbaImage {
    let (w, h) = img.dimensions();
    let edge = w.min(h);
    let x = (w - edge) / 2;
    let y = (h - edge) / 2;
    img.crop_imm(x, y, edge, edge).to_rgba8()
}

/// Encode an RGBA buffer as lossless WebP (VP8L) into a byte vector.
pub(crate) fn encode_lossless(img: &RgbaImage) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out)
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgba8,
        )
        .context("webp encode failed")?;
    Ok(out)
}

/// Encode an RGBA buffer as LOSSY WebP (VP8) at `quality` (0–100) into a byte
/// vector, via libwebp (bundled by `libwebp-sys2`, no system dependency). Used by
/// the cover path to hit a tight byte budget while keeping detailed art sharp —
/// the pure-Rust `image-webp` encoder is lossless-only, so it can't trade quality
/// for size. libwebp premultiplies alpha internally; covers are opaque so this is
/// moot, but we pass RGBA straight through for a general encoder.
pub(crate) fn encode_webp_lossy(img: &RgbaImage, quality: f32) -> Result<Vec<u8>> {
    if img.width() == 0 || img.height() == 0 {
        bail!("cannot encode a zero-dimension image");
    }
    let encoder = webp::Encoder::from_rgba(img.as_raw(), img.width(), img.height());
    // `encode` returns a WebPMemory (Deref<[u8]>); it allocates via libwebp and
    // has no fallible path we can recover from here, so copy the bytes out.
    let mem = encoder.encode(quality.clamp(1.0, 100.0));
    if mem.is_empty() {
        bail!("libwebp produced empty output");
    }
    Ok(mem.to_vec())
}

/// The public path stored on the user row for a saved avatar, cache-busted with
/// `?v=<version>` so the browser refetches after a change. The `/avatars/{file}`
/// route reads the bytes from the `user_avatars` table keyed by `<user_id>`.
pub fn avatar_url(user_id: &str, version: i64) -> String {
    format!("/avatars/{user_id}.webp?v={version}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    /// A deterministic, high-entropy test image that resists lossless
    /// compression (so the budget logic actually has to downscale for it).
    fn noisy_png(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            // A cheap pseudo-random-ish pattern — no repeating runs.
            let r = ((x * 73 + y * 151) % 256) as u8;
            let g = ((x * 199 + y * 37) % 256) as u8;
            let b = ((x ^ (y.wrapping_mul(101))) % 256) as u8;
            *px = image::Rgb([r, g, b]);
        }
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn output_is_square_webp_within_budget() {
        // A wide, noisy source: must be cropped square and fit the byte budget.
        let src = noisy_png(600, 400);
        let webp = process_avatar(&src).expect("processes");
        assert!(
            webp.len() <= MAX_AVATAR_BYTES,
            "avatar {} bytes exceeds budget {}",
            webp.len(),
            MAX_AVATAR_BYTES
        );
        let decoded = image::load_from_memory(&webp).expect("valid webp");
        let (w, h) = decoded.dimensions();
        assert_eq!(w, h, "avatar must be square");
        assert!(
            CANDIDATE_EDGES.contains(&w),
            "edge {w} not a candidate size"
        );
    }

    #[test]
    fn simple_image_keeps_full_edge() {
        // A flat image compresses tiny, so the largest candidate edge is kept.
        let flat = {
            let img = RgbImage::from_pixel(500, 500, image::Rgb([20, 120, 200]));
            let mut bytes = Vec::new();
            DynamicImage::ImageRgb8(img)
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .unwrap();
            bytes
        };
        let webp = process_avatar(&flat).unwrap();
        let (w, _) = image::load_from_memory(&webp).unwrap().dimensions();
        assert_eq!(
            w, CANDIDATE_EDGES[0],
            "flat image should keep the full edge"
        );
    }

    #[test]
    fn rejects_empty_and_oversize() {
        assert!(process_avatar(&[]).is_err());
        assert!(process_avatar(&vec![0u8; MAX_UPLOAD_BYTES + 1]).is_err());
    }

    #[test]
    fn rejects_non_image() {
        assert!(process_avatar(b"this is not an image").is_err());
    }

    #[test]
    fn avatar_url_is_versioned_path() {
        assert_eq!(avatar_url("user-123", 42), "/avatars/user-123.webp?v=42");
    }

    fn encode(w: u32, h: u32, fmt: image::ImageFormat) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([
                ((x * 73 + y * 151) % 256) as u8,
                ((x * 199 + y * 37) % 256) as u8,
                ((x ^ (y.wrapping_mul(101))) % 256) as u8,
            ]);
        }
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), fmt)
            .unwrap();
        out
    }

    #[test]
    fn ensure_complete_accepts_whole_images() {
        for fmt in [
            image::ImageFormat::Jpeg,
            image::ImageFormat::Png,
            image::ImageFormat::WebP,
        ] {
            let bytes = encode(64, 96, fmt);
            ensure_complete(&bytes).unwrap_or_else(|e| panic!("{fmt:?} rejected: {e}"));
        }
    }

    /// The bug this gate exists for: `zune-jpeg` returns `Ok` for a JPEG cut off
    /// mid-scan, filling the missing rows with a flat decoder value. The terminator
    /// check is the only place the truncation is still detectable.
    #[test]
    fn ensure_complete_rejects_truncated_jpeg_the_decoder_accepts() {
        let whole = encode(200, 300, image::ImageFormat::Jpeg);
        let mut caught = 0;
        let mut decoder_fooled = 0;
        for pct in [30, 40, 50, 60, 70, 80, 90] {
            let part = &whole[..whole.len() * pct / 100];
            if image::load_from_memory(part).is_ok() {
                decoder_fooled += 1;
            }
            let err = ensure_complete(part).expect_err("truncated JPEG must be rejected");
            assert!(err.to_string().contains("truncated"), "got: {err}");
            caught += 1;
        }
        assert_eq!(caught, 7, "every truncation point must be caught");
        assert!(
            decoder_fooled > 0,
            "precondition: the decoder must accept at least one truncation \
             (otherwise this gate isn't testing anything)"
        );
    }

    #[test]
    fn ensure_complete_rejects_truncated_webp_and_png() {
        for fmt in [image::ImageFormat::WebP, image::ImageFormat::Png] {
            let whole = encode(200, 300, fmt);
            ensure_complete(&whole).unwrap_or_else(|e| panic!("whole {fmt:?} rejected: {e}"));
            let part = &whole[..whole.len() * 7 / 10];
            let e = ensure_complete(part)
                .unwrap_err_or_panic(&format!("{fmt:?} truncation must be rejected"));
            assert!(e.to_string().contains("truncated"), "{fmt:?}: {e}");
        }
    }

    /// Tiny local helper so the loop above reads as an assertion rather than a
    /// double-negative `unwrap_or_else(|_| panic!(...))`.
    trait UnwrapErrOrPanic<E> {
        fn unwrap_err_or_panic(self, msg: &str) -> E;
    }
    impl<T, E> UnwrapErrOrPanic<E> for std::result::Result<T, E> {
        fn unwrap_err_or_panic(self, msg: &str) -> E {
            match self {
                Ok(_) => panic!("{msg}"),
                Err(e) => e,
            }
        }
    }

    /// A JPEG with trailing padding after the EOI still passes — the check searches a
    /// tail window rather than demanding EOI be the very last byte, because a false
    /// reject costs a real cover permanently.
    ///
    /// The 1,400-byte case is the regression guard for [`TAIL_WINDOW`]: six real covers
    /// in the production corpus carry 376–1,373 bytes after their EOI and were rejected
    /// outright by the original 64-byte window.
    #[test]
    fn ensure_complete_tolerates_trailing_padding() {
        for pad in [16usize, 1400, TAIL_WINDOW - 8] {
            let mut bytes = encode(64, 96, image::ImageFormat::Jpeg);
            bytes.extend_from_slice(&vec![0u8; pad]);
            ensure_complete(&bytes)
                .unwrap_or_else(|e| panic!("{pad} bytes of trailing padding rejected: {e}"));
        }
        // Same for PNG: trailing bytes after IEND are not a truncation.
        let mut png = encode(64, 96, image::ImageFormat::Png);
        png.extend_from_slice(&[0u8; 1400]);
        ensure_complete(&png).expect("PNG trailing padding must not be a rejection");
    }

    /// Formats with no unambiguous terminator (and non-images) are passed through to the
    /// decoder, which reports them properly.
    #[test]
    fn ensure_complete_passes_unknown_formats_through() {
        ensure_complete(b"not an image at all").expect("unknown bytes are the decoder's problem");
        ensure_complete(&[]).expect("empty is caught by the emptiness check, not here");
    }

    #[test]
    fn decode_limited_rejects_truncated_jpeg() {
        let whole = encode(200, 300, image::ImageFormat::Jpeg);
        let part = &whole[..whole.len() * 6 / 10];
        assert!(
            decode_limited(part).is_err(),
            "decode_limited must inherit the truncation gate"
        );
    }
}
