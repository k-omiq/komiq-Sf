//! Entropy-level check of a sequential (baseline or extended) Huffman JPEG.
//!
//! [`crate::avatar::ensure_complete`] only proves a JPEG has its end marker. A file
//! that two writers wrote into at once (Suwayomi downloads a thumbnail to one shared
//! temp path, so two concurrent requests for the same manga interleave their bytes)
//! keeps both its start and its end marker, and `zune-jpeg` decodes it to `Ok`, even
//! in strict mode. The damage is a band of smeared or green blocks, which is then
//! re-encoded and served behind a one-year immutable TTL.
//!
//! libjpeg sees it: the entropy-coded data of a scan, decoded block by block, must end
//! exactly where the next marker starts ("extraneous bytes before marker") and must not
//! run into a marker early ("premature end of data segment"). This module does that
//! accounting without the IDCT: it walks the Huffman codes only.
//!
//! The check is conservative. Anything it does not model (progressive or arithmetic
//! coding, a missing table, a height set by DNL, a malformed header) passes, and the
//! decoder decides as before. Only a scan that decodes wrongly is refused.

use anyhow::{bail, Result};

/// Refuse a sequential Huffman JPEG whose scan data does not decode cleanly. Non-JPEG
/// input and JPEG variants this does not model pass.
pub(crate) fn check(bytes: &[u8]) -> Result<()> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return Ok(());
    }
    let mut dc: [Option<Huffman>; 4] = Default::default();
    let mut ac: [Option<Huffman>; 4] = Default::default();
    let mut restart_interval = 0usize;
    let mut frame: Option<Frame> = None;
    let mut pos = 2;
    loop {
        // A marker: `FF`, any number of `FF` fill bytes, then the code.
        if bytes.get(pos) != Some(&0xFF) {
            return Ok(());
        }
        while bytes.get(pos + 1) == Some(&0xFF) {
            pos += 1;
        }
        let Some(&code) = bytes.get(pos + 1) else {
            return Ok(());
        };
        pos += 2;
        match code {
            0xD9 => return Ok(()),
            0x01 | 0xD0..=0xD7 => continue,
            _ => {}
        }
        let Some(len) = be_u16(bytes, pos).map(usize::from) else {
            return Ok(());
        };
        let Some(segment) = (len >= 2).then(|| bytes.get(pos + 2..pos + len)).flatten() else {
            return Ok(());
        };
        pos += len;
        match code {
            0xC4 => {
                if !parse_dht(segment, &mut dc, &mut ac) {
                    return Ok(());
                }
            }
            0xDD => match be_u16(segment, 0) {
                Some(ri) => restart_interval = usize::from(ri),
                None => return Ok(()),
            },
            0xC0 | 0xC1 => match Frame::parse(segment) {
                Some(f) => frame = Some(f),
                None => return Ok(()),
            },
            // Progressive, lossless, hierarchical or arithmetic: not modelled.
            0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return Ok(()),
            0xDA => {
                let Some(frame) = frame.as_ref() else {
                    return Ok(());
                };
                let Some(scan) = Scan::parse(segment, frame, &dc, &ac) else {
                    return Ok(());
                };
                pos = scan.walk(bytes, pos, frame, restart_interval)?;
            }
            _ => {}
        }
    }
}

fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]))
}

/// Canonical Huffman decoding tables (ITU T.81 F.2.2.3).
#[derive(Clone)]
struct Huffman {
    min_code: [i32; 17],
    max_code: [i32; 17],
    val_ptr: [usize; 17],
    values: Vec<u8>,
}

impl Huffman {
    fn build(counts: &[u8; 16], values: &[u8]) -> Option<Self> {
        let mut table = Huffman {
            min_code: [0; 17],
            max_code: [-1; 17],
            val_ptr: [0; 17],
            values: values.to_vec(),
        };
        let mut code = 0i32;
        let mut k = 0usize;
        for len in 1..=16 {
            let n = i32::from(counts[len - 1]);
            table.val_ptr[len] = k;
            table.min_code[len] = code;
            code += n;
            k += n as usize;
            // More codes than fit in `len` bits: not a valid table.
            if code > 1 << len {
                return None;
            }
            if n > 0 {
                table.max_code[len] = code - 1;
            }
            code <<= 1;
        }
        Some(table)
    }

    fn decode(&self, bits: &mut Bits) -> Result<u8> {
        let mut code = bits.bit()? as i32;
        for len in 1..=16 {
            if code <= self.max_code[len] {
                let index = self.val_ptr[len] + (code - self.min_code[len]) as usize;
                return match self.values.get(index) {
                    Some(&v) => Ok(v),
                    None => bail!("garbled JPEG data (Huffman code out of range)"),
                };
            }
            if len < 16 {
                code = (code << 1) | bits.bit()? as i32;
            }
        }
        bail!("garbled JPEG data (bad Huffman code)")
    }
}

/// Every table in one DHT segment, or `false` when the segment is malformed.
fn parse_dht(mut seg: &[u8], dc: &mut [Option<Huffman>; 4], ac: &mut [Option<Huffman>; 4]) -> bool {
    while !seg.is_empty() {
        if seg.len() < 17 {
            return false;
        }
        let (class, id) = (seg[0] >> 4, usize::from(seg[0] & 0x0F));
        let mut counts = [0u8; 16];
        counts.copy_from_slice(&seg[1..17]);
        let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
        let Some(values) = seg.get(17..17 + total) else {
            return false;
        };
        let Some(table) = Huffman::build(&counts, values) else {
            return false;
        };
        match (class, id) {
            (0, 0..=3) => dc[id] = Some(table),
            (1, 0..=3) => ac[id] = Some(table),
            _ => return false,
        }
        seg = &seg[17 + total..];
    }
    true
}

struct Component {
    id: u8,
    h: usize,
    v: usize,
}

struct Frame {
    width: usize,
    height: usize,
    components: Vec<Component>,
    h_max: usize,
    v_max: usize,
}

impl Frame {
    fn parse(seg: &[u8]) -> Option<Self> {
        let height = usize::from(be_u16(seg, 1)?);
        let width = usize::from(be_u16(seg, 3)?);
        let n = usize::from(*seg.get(5)?);
        let mut components = Vec::with_capacity(n);
        for i in 0..n {
            let at = 6 + i * 3;
            let sampling = *seg.get(at + 1)?;
            let (h, v) = (usize::from(sampling >> 4), usize::from(sampling & 0x0F));
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
                return None;
            }
            components.push(Component {
                id: *seg.get(at)?,
                h,
                v,
            });
        }
        // Height 0 is set later by a DNL marker: not modelled.
        if width == 0 || height == 0 || components.is_empty() {
            return None;
        }
        let h_max = components.iter().map(|c| c.h).max()?;
        let v_max = components.iter().map(|c| c.v).max()?;
        Some(Frame {
            width,
            height,
            components,
            h_max,
            v_max,
        })
    }
}

/// One scan component: its frame sampling and its two tables.
struct ScanComponent {
    h: usize,
    v: usize,
    dc: Huffman,
    ac: Huffman,
}

struct Scan {
    components: Vec<ScanComponent>,
}

impl Scan {
    fn parse(
        seg: &[u8],
        frame: &Frame,
        dc: &[Option<Huffman>; 4],
        ac: &[Option<Huffman>; 4],
    ) -> Option<Self> {
        let n = usize::from(*seg.first()?);
        let mut components = Vec::with_capacity(n);
        for i in 0..n {
            let at = 1 + i * 2;
            let id = *seg.get(at)?;
            let tables = *seg.get(at + 1)?;
            let fc = frame.components.iter().find(|c| c.id == id)?;
            components.push(ScanComponent {
                h: fc.h,
                v: fc.v,
                dc: dc.get(usize::from(tables >> 4))?.clone()?,
                ac: ac.get(usize::from(tables & 0x0F))?.clone()?,
            });
        }
        (!components.is_empty()).then_some(Scan { components })
    }

    /// Decode every block of the scan starting at `pos`, checking the restart markers
    /// on the way, and return the position of the marker that ends the scan.
    fn walk(
        &self,
        bytes: &[u8],
        pos: usize,
        frame: &Frame,
        restart_interval: usize,
    ) -> Result<usize> {
        let div_ceil = |a: usize, b: usize| a.div_ceil(b);
        // A single-component scan is non-interleaved: its unit is one block of that
        // component, over the component's own (subsampled) size. Otherwise the unit is
        // an MCU holding h×v blocks of each component.
        let (units, per_unit): (usize, Vec<usize>) = if let [c] = self.components.as_slice() {
            let w = div_ceil(div_ceil(frame.width * c.h, frame.h_max), 8);
            let h = div_ceil(div_ceil(frame.height * c.v, frame.v_max), 8);
            (w * h, vec![1])
        } else {
            let w = div_ceil(frame.width, 8 * frame.h_max);
            let h = div_ceil(frame.height, 8 * frame.v_max);
            (w * h, self.components.iter().map(|c| c.h * c.v).collect())
        };
        let mut bits = Bits {
            data: bytes,
            pos,
            acc: 0,
            left: 0,
        };
        let mut next_rst = 0u8;
        for unit in 0..units {
            if restart_interval > 0 && unit > 0 && unit % restart_interval == 0 {
                bits.pos = expect_marker(bytes, bits.pos, Some(0xD0 + next_rst))?;
                bits.left = 0;
                next_rst = (next_rst + 1) % 8;
            }
            for (component, &blocks) in self.components.iter().zip(&per_unit) {
                for _ in 0..blocks {
                    decode_block(&mut bits, component)?;
                }
            }
        }
        // The padding bits of the last byte are dropped; the next byte must be a marker.
        let end = bits.pos;
        expect_marker(bytes, end, None)?;
        Ok(end)
    }
}

/// Skip one 8×8 block's coefficients.
fn decode_block(bits: &mut Bits, c: &ScanComponent) -> Result<()> {
    let size = c.dc.decode(bits)?;
    if size > 16 {
        bail!("garbled JPEG data (bad DC size)");
    }
    bits.skip(size)?;
    let mut k = 1;
    while k < 64 {
        let rs = c.ac.decode(bits)?;
        let (run, size) = (usize::from(rs >> 4), rs & 0x0F);
        if size == 0 {
            if run != 15 {
                break; // end of block
            }
            k += 16;
            continue;
        }
        k += run + 1;
        bits.skip(size)?;
    }
    Ok(())
}

/// Check that a marker starts at `pos` (after optional `FF` fill bytes) and, when
/// `want` is given, that it is that marker; return the position just past it.
fn expect_marker(bytes: &[u8], mut pos: usize, want: Option<u8>) -> Result<usize> {
    let start = pos;
    if bytes.get(pos) != Some(&0xFF) {
        let extra = bytes[pos.min(bytes.len())..]
            .windows(2)
            .position(|w| w[0] == 0xFF && w[1] != 0x00)
            .unwrap_or(bytes.len().saturating_sub(pos));
        bail!("garbled JPEG data ({extra} extraneous bytes before marker)");
    }
    while bytes.get(pos + 1) == Some(&0xFF) {
        pos += 1;
    }
    let found = bytes.get(pos + 1).copied();
    if let Some(want) = want {
        if found != Some(want) {
            bail!(
                "garbled JPEG data (restart marker {want:#04X} expected at byte {start}, found {:?})",
                found
            );
        }
    }
    Ok(pos + 2)
}

/// Entropy-coded bits, MSB first, with `FF 00` byte stuffing. Running into a marker
/// while bits are still needed is the "premature end of data segment" case.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u8,
    left: u8,
}

impl Bits<'_> {
    fn bit(&mut self) -> Result<u8> {
        if self.left == 0 {
            let Some(&byte) = self.data.get(self.pos) else {
                bail!("garbled JPEG data (premature end of data segment)");
            };
            if byte == 0xFF {
                if self.data.get(self.pos + 1) != Some(&0x00) {
                    bail!("garbled JPEG data (premature end of data segment)");
                }
                self.pos += 2;
            } else {
                self.pos += 1;
            }
            self.acc = byte;
            self.left = 8;
        }
        self.left -= 1;
        Ok((self.acc >> self.left) & 1)
    }

    fn skip(&mut self, n: u8) -> Result<()> {
        for _ in 0..n {
            self.bit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([
                (x * 7 % 256) as u8,
                (y * 5 % 256) as u8,
                ((x ^ y) % 256) as u8,
            ])
        });
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
            .encode_image(&img)
            .unwrap();
        out
    }

    /// The offset just past the SOS header, and the EOI offset.
    fn scan_bounds(bytes: &[u8]) -> (usize, usize) {
        let sos = bytes.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap();
        let len = usize::from(be_u16(bytes, sos + 2).unwrap());
        (sos + 2 + len, bytes.len() - 2)
    }

    #[test]
    fn whole_images_pass() {
        for (w, h) in [(1, 1), (8, 8), (17, 33), (200, 300), (690, 1000)] {
            check(&jpeg(w, h)).unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
        }
    }

    #[test]
    fn non_jpeg_and_unmodelled_input_passes() {
        check(b"not an image").unwrap();
        check(&[]).unwrap();
        check(&[0xFF, 0xD8, 0xFF]).unwrap();
        // A truncated header is the decoder's problem, not this check's.
        let whole = jpeg(64, 64);
        check(&whole[..40]).unwrap();
    }

    #[test]
    fn skipped_chunk_in_scan_data_is_refused() {
        let whole = jpeg(400, 600);
        let (start, end) = scan_bounds(&whole);
        let cut = start + (end - start) / 3;
        let mut garbled = whole[..cut].to_vec();
        garbled.extend_from_slice(&whole[cut + 4096..]);
        let err = check(&garbled).unwrap_err().to_string();
        assert!(err.contains("garbled"), "{err}");
    }

    #[test]
    fn repeated_chunk_in_scan_data_is_refused() {
        let whole = jpeg(400, 600);
        let (start, end) = scan_bounds(&whole);
        let cut = start + (end - start) / 2;
        let mut garbled = whole[..cut].to_vec();
        garbled.extend_from_slice(&whole[cut - 4096..]);
        let err = check(&garbled).unwrap_err().to_string();
        assert!(err.contains("garbled"), "{err}");
    }

    #[test]
    fn trailing_bytes_after_eoi_pass() {
        let mut bytes = jpeg(64, 64);
        bytes.extend_from_slice(b"trailer after the end marker");
        check(&bytes).unwrap();
    }

    /// 48x32, 4:2:0, a restart marker after every MCU (5 of them): libjpeg 9
    /// `cjpeg -quality 80 -sample 2x2 -restart 1B`, base64. The image crate's encoder
    /// writes no restart markers, so this is the only way to test that path.
    const RESTART_48X32: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAYEBQYFBAYGBQYHBwYIChAKCgkJChQODwwQFxQYGBcUFhYaHSUfGhsjHBYWICwgIyYnKSopGR8tMC0oMCUoKSj/2wBDAQcHBwoIChMKChMoGhYaKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCgoKCj/wAARCAAgADADASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/90ABAAB/9oADAMBAAIRAxEAPwD50s9M34+Wtq30ZFXdIAo963NL09Vj3uPlAzWnb6fnEsw3MeFUfyHtXtYOlQjQjOcVKUlfXZLu+u+iS3OPC5k72TP/0PH7fSojwkTOe3GAa0ItMjAGIo1bODk7v0Fb8Vgc4kBJwMovCge9aFvY4AP8IP8AD8o/A9TXtValJNrlS/7dWn6/dJtdUj38FmL01P/R4CHTEXG6JCD324H6mr6aWpkUJGuMdl/+sa3oNOxhQgXPHAx9D3P8qvwWOcPtzhRycHv75/nXTXr0+Wzil6L77dbH6bgswemp/9Ll7GwzbxoBy56Y6gf5FacFkchwFyeEJHAUd/8APrWzZ2QEMeFbcEJyPfgVpw6fjOVIH3enYdR+JpU8a1ShZ7KPy0/z5l5Np9D86weP13P/04rbTwBtUDZ2UjH4n2rQgsTuzwTwM8A//WHNbcNgcnI5I5wOMj+g4rQgsBgALj+efr6+/b61yVMZbRaW/paa99F02Wl2/HwWP8z/1NqGwwu0LgAcgL/n075PPStCGxO7IX2/nx/n8q3ILDZsGAMdOMYHX8P85NaEGn4GcEDjJP69v8+nevDqYt33t38rL/L+u3rYLH+Z/9k=";

    fn restart_fixture() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(RESTART_48X32)
            .unwrap()
    }

    #[test]
    fn restart_markers_in_order_pass() {
        check(&restart_fixture()).unwrap();
    }

    #[test]
    fn bytes_lost_inside_a_restart_interval_are_refused() {
        let whole = restart_fixture();
        let rst = |n: u8| {
            whole
                .windows(2)
                .position(|w| w == [0xFF, 0xD0 + n])
                .unwrap()
        };
        // Drop bytes between RST0 and RST1: the second interval ends early.
        let (a, b) = (rst(0) + 2, rst(1));
        let mut garbled = whole[..a + 10].to_vec();
        garbled.extend_from_slice(&whole[b - 10..]);
        let err = check(&garbled).unwrap_err().to_string();
        assert!(err.contains("garbled"), "{err}");
        // Repeat an interval: the decoder finds RST1 where RST2 belongs.
        let mut repeated = whole[..rst(2)].to_vec();
        repeated.extend_from_slice(&whole[rst(1)..]);
        let err = check(&repeated).unwrap_err().to_string();
        assert!(err.contains("restart marker"), "{err}");
    }
}
