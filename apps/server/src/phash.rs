//! Perceptual hashing for cover-based dedup (CATALOGUE.md §4, §10 — server-side pHash).
//!
//! A 64-bit dHash (difference hash): decode → grayscale → resize to 9×8 → compare
//! horizontally-adjacent pixels (one bit each) → 64 bits → hex. Robust to
//! rescaling/JPEG recompression, so the same cover art hashes near-identically
//! across sources. Compared via `catalog::similarity::phash_similarity`
//! (normalized Hamming over the hex), which expects exactly this 16-char hex form.

use image::imageops::FilterType;

/// dHash of an encoded image (JPEG/PNG bytes) as a 16-char hex string (64 bits).
/// `None` if the bytes can't be decoded — pHash is a best-effort corroboration
/// signal, never fatal to a sync.
pub fn dhash(bytes: &[u8]) -> Option<String> {
    let img = image::load_from_memory(bytes).ok()?;
    // 9×8 grayscale: 8 horizontal comparisons per row × 8 rows = 64 bits.
    let gray = img.to_luma8();
    let small = image::imageops::resize(&gray, 9, 8, FilterType::Triangle);
    let mut bits: u64 = 0;
    for y in 0..8u32 {
        for x in 0..8u32 {
            let left = small.get_pixel(x, y).0[0];
            let right = small.get_pixel(x + 1, y).0[0];
            bits <<= 1;
            if left > right {
                bits |= 1;
            }
        }
    }
    Some(format!("{bits:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, ImageFormat, Luma};
    use std::io::Cursor;

    /// Encode a small grayscale image to PNG bytes for a round-trip decode+hash.
    fn png_bytes(f: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let img = GrayImage::from_fn(32, 32, |x, y| Luma([f(x, y)]));
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn dhash_is_16_hex_chars_and_deterministic() {
        // A horizontal gradient: left brighter than right on every row.
        let bytes = png_bytes(|x, _| 255u8.saturating_sub((x * 8) as u8));
        let a = dhash(&bytes).expect("decodes");
        let b = dhash(&bytes).expect("decodes");
        assert_eq!(a, b, "same image hashes identically");
        assert_eq!(a.len(), 16, "64-bit hash is 16 hex chars");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // Left-brighter-than-right on every comparison => all bits set.
        assert_eq!(a, "ffffffffffffffff");
    }

    #[test]
    fn distinct_images_hash_differently() {
        let left_bright = dhash(&png_bytes(|x, _| 255u8.saturating_sub((x * 8) as u8))).unwrap();
        let right_bright = dhash(&png_bytes(|x, _| (x * 8) as u8)).unwrap();
        assert_ne!(left_bright, right_bright);
    }

    #[test]
    fn garbage_bytes_return_none() {
        assert_eq!(dhash(b"not an image"), None);
    }
}
