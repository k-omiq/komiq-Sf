//! Comment image processing.
//!
//! Like [`crate::avatar`], but for the single image a user can attach to a
//! comment: decode arbitrary uploaded bytes (JPEG/PNG/WebP — animated GIFs land
//! as their first frame, static), downscale **preserving aspect ratio** (avatars
//! square-crop; comment images must not), and re-encode as budgeted lossless WebP
//! with the pure-Rust `image` encoder (no libwebp/C dependency).
//!
//! To honour a size budget without a lossy quality knob we pick the *largest*
//! candidate longest-edge whose encoded size fits [`MAX_MEDIA_BYTES`]; a busy
//! photo lands on a smaller edge, a flat screenshot keeps full size. Storage is
//! the SQLite `comment_media` table (a BLOB per image), Litestream-replicated
//! with the rest of the DB. Upload/serve wiring lives in `main.rs`.

use anyhow::{anyhow, bail, Context, Result};
use image::codecs::webp::WebPEncoder;
use image::{imageops, ExtendedColorType, GenericImageView, RgbaImage};

/// Reject raw uploads larger than this before decoding — a decode-bomb guard and
/// a bound on the work one request can trigger. Matches the avatar cap (8 MB).
pub const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;

/// Size budget for the stored WebP. Larger than an avatar's (comment images are
/// viewed at content width, not a 40px chip) but still small enough to keep the
/// SQLite BLOB store and its Litestream replication cheap.
pub const MAX_MEDIA_BYTES: usize = 500 * 1024;

/// Candidate longest-edge lengths (px), largest first. The first that encodes
/// within [`MAX_MEDIA_BYTES`] wins; if none do (a large noisy photo), the
/// smallest is used — a 400px lossless image is still comfortably bounded.
const CANDIDATE_EDGES: [u32; 7] = [1280, 1080, 960, 800, 640, 512, 400];

/// A processed comment image: budgeted WebP bytes plus the final pixel dimensions
/// (so the client can reserve layout space and avoid reflow while it loads).
pub struct ProcessedImage {
    pub webp: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Decode arbitrary uploaded image bytes, downscale preserving aspect ratio, and
/// re-encode as budgeted lossless WebP.
pub fn process_comment_image(bytes: &[u8]) -> Result<ProcessedImage> {
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
    let (w0, h0) = img.dimensions();
    if w0 == 0 || h0 == 0 {
        bail!("image has a zero dimension");
    }
    let rgba = img.to_rgba8();

    let mut fallback: Option<ProcessedImage> = None;
    for edge in CANDIDATE_EDGES {
        let (w, h) = scaled_dims(w0, h0, edge);
        let resized = if w == w0 && h == h0 {
            rgba.clone()
        } else {
            imageops::resize(&rgba, w, h, imageops::FilterType::Lanczos3)
        };
        let webp = encode_lossless(&resized)?;
        let out = ProcessedImage {
            webp,
            width: w,
            height: h,
        };
        if out.webp.len() <= MAX_MEDIA_BYTES {
            return Ok(out);
        }
        fallback = Some(out); // keep the last (smallest edge) as the fallback
    }
    fallback.ok_or_else(|| anyhow!("no candidate size produced"))
}

/// Decode uploaded image bytes with hard decoder limits so a decompression bomb
/// (a tiny, highly-compressible file that expands to gigabytes of RGBA — the
/// `MAX_UPLOAD_BYTES` cap only bounds the *compressed* input) can't OOM the
/// process before we ever downscale. Caps pixel dimensions and the decode
/// allocation; an over-limit image is rejected as a client error, not decoded.
fn decode_limited(bytes: &[u8]) -> Result<image::DynamicImage> {
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

/// Scale `(w0, h0)` so the longest edge is at most `max_edge`, preserving aspect
/// ratio. Never upscales (an image already within the edge is returned as-is).
fn scaled_dims(w0: u32, h0: u32, max_edge: u32) -> (u32, u32) {
    let longest = w0.max(h0);
    if longest <= max_edge {
        return (w0, h0);
    }
    let scale = max_edge as f64 / longest as f64;
    let w = ((w0 as f64 * scale).round() as u32).max(1);
    let h = ((h0 as f64 * scale).round() as u32).max(1);
    (w, h)
}

/// Encode an RGBA buffer as lossless WebP (VP8L) into a byte vector.
fn encode_lossless(img: &RgbaImage) -> Result<Vec<u8>> {
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

/// Public path for a stored comment image. The `/comment-media/{file}` route reads
/// the bytes from `comment_media` keyed by `<id>`. The id is a random uuid, so the
/// bytes are immutable and the response is long-cached.
pub fn comment_media_url(id: &str) -> String {
    format!("/comment-media/{id}.webp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn noisy_png(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
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

    fn flat_png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbImage::from_pixel(w, h, image::Rgb([20, 120, 200]));
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
    fn preserves_aspect_ratio_and_downscales_longest_edge() {
        // A wide 2000x1000 flat image: aspect ratio kept, longest edge clamped to
        // the top candidate (1280), and it must decode as valid WebP.
        let src = flat_png(2000, 1000);
        let out = process_comment_image(&src).expect("processes");
        assert_eq!(out.width, 1280, "longest edge clamped to top candidate");
        assert_eq!(out.height, 640, "aspect ratio preserved (2:1)");
        let decoded = image::load_from_memory(&out.webp).expect("valid webp");
        assert_eq!(decoded.dimensions(), (1280, 640));
    }

    #[test]
    fn does_not_upscale_small_images() {
        let src = flat_png(300, 200);
        let out = process_comment_image(&src).unwrap();
        assert_eq!((out.width, out.height), (300, 200), "never upscales");
    }

    #[test]
    fn scaled_dims_clamps_longest_edge_without_upscaling() {
        // Landscape: longest edge (w) clamped to max, height scaled proportionally.
        assert_eq!(scaled_dims(2000, 1000, 1280), (1280, 640));
        // Portrait: longest edge (h) clamped, width scaled.
        assert_eq!(scaled_dims(1000, 2000, 1280), (640, 1280));
        // Already within the cap: returned unchanged, never upscaled.
        assert_eq!(scaled_dims(800, 600, 1280), (800, 600));
        // A degenerate thin strip never rounds a dimension to zero.
        let (w, h) = scaled_dims(4000, 3, 1280);
        assert_eq!(w, 1280);
        assert!(h >= 1, "height must stay at least 1px");
    }

    #[test]
    fn noisy_image_processes_to_valid_bounded_webp() {
        // High-entropy input must still yield a well-formed WebP whose longest edge
        // is one of the candidate sizes (either it fit the budget, or it fell back).
        let src = noisy_png(640, 640);
        let out = process_comment_image(&src).unwrap();
        let longest = out.width.max(out.height);
        assert!(longest <= 640 && CANDIDATE_EDGES.contains(&longest));
        image::load_from_memory(&out.webp).expect("valid webp");
    }

    #[test]
    fn rejects_empty_oversize_and_non_image() {
        assert!(process_comment_image(&[]).is_err());
        assert!(process_comment_image(&vec![0u8; MAX_UPLOAD_BYTES + 1]).is_err());
        assert!(process_comment_image(b"not an image").is_err());
    }

    #[test]
    fn media_url_is_id_path() {
        assert_eq!(comment_media_url("abc-123"), "/comment-media/abc-123.webp");
    }
}
