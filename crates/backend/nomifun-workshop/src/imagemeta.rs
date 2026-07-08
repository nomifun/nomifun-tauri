//! Pixel-dimension extraction straight from image container headers — no
//! decoder dependency. Best-effort: unknown / truncated / unsupported formats
//! return `None` and the asset is still stored with null width/height.
//!
//! Covers the formats a creative-workshop upload realistically carries: PNG,
//! JPEG, GIF, and WebP (all three flavors). The PNG/WebP parsing mirrors
//! `nomifun-companion::figure`.

/// Parse `(width, height)` from the leading bytes of an encoded image, or
/// `None` when the format is unrecognized or the header is malformed/truncated.
pub(crate) fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    png_dimensions(bytes)
        .or_else(|| gif_dimensions(bytes))
        .or_else(|| webp_dimensions(bytes))
        .or_else(|| jpeg_dimensions(bytes))
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // signature (8) + IHDR length (4) + "IHDR" (4), then BE u32 w/h.
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w, h))
}

fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // "GIF87a"/"GIF89a" then LE u16 logical-screen width @6, height @8.
    if bytes.len() < 10 || (&bytes[0..6] != b"GIF87a" && &bytes[0..6] != b"GIF89a") {
        return None;
    }
    let w = u16::from_le_bytes(bytes[6..8].try_into().ok()?);
    let h = u16::from_le_bytes(bytes[8..10].try_into().ok()?);
    Some((u32::from(w), u32::from(h)))
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // RIFF header (12), then the first chunk fourcc picks the flavor; every
    // chunk payload starts at byte 20 (fourcc 4 + chunk size 4).
    if bytes.len() < 16 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    match &bytes[12..16] {
        // Extended: canvas w/h as LE 24-bit minus-one fields at payload +4.
        b"VP8X" if bytes.len() >= 30 => {
            let le24 = |b: &[u8]| u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16;
            Some((le24(&bytes[24..27]) + 1, le24(&bytes[27..30]) + 1))
        }
        // Lossless: signature byte 0x2F, then 14+14 bits minus-one in a LE u32.
        b"VP8L" if bytes.len() >= 25 && bytes[20] == 0x2F => {
            let b = u32::from_le_bytes(bytes[21..25].try_into().ok()?);
            Some(((b & 0x3FFF) + 1, ((b >> 14) & 0x3FFF) + 1))
        }
        // Lossy: 3-byte frame tag, start code 9D 01 2A, then LE u16 w/h
        // (low 14 bits each; the top 2 bits are upscaling hints).
        b"VP8 " if bytes.len() >= 30 && bytes[23..26] == [0x9D, 0x01, 0x2A] => {
            let w = u16::from_le_bytes(bytes[26..28].try_into().ok()?) & 0x3FFF;
            let h = u16::from_le_bytes(bytes[28..30].try_into().ok()?) & 0x3FFF;
            Some((u32::from(w), u32::from(h)))
        }
        _ => None,
    }
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // SOI marker.
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < bytes.len() {
        // Every marker begins with 0xFF; skip fill bytes.
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // Standalone markers (no length): padding (0xFF), RSTn, SOI/EOI.
        if marker == 0xFF || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // SOF markers carry the frame dimensions; exclude DHT/JPG/DAC (C4/C8/CC).
        let is_sof = matches!(marker, 0xC0..=0xCF)
            && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            // segment: [len(2)][precision(1)][height(2)][width(2)]…
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]);
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]);
            return Some((u32::from(w), u32::from(h)));
        }
        if seg_len < 2 {
            return None;
        }
        i += 2 + seg_len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_header_dimensions() {
        // 8-byte sig + IHDR chunk (len + "IHDR" + 7x5 + rest).
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&7u32.to_be_bytes());
        b.extend_from_slice(&5u32.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(image_dimensions(&b), Some((7, 5)));
    }

    #[test]
    fn gif_header_dimensions() {
        let mut b = b"GIF89a".to_vec();
        b.extend_from_slice(&12u16.to_le_bytes());
        b.extend_from_slice(&9u16.to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        assert_eq!(image_dimensions(&b), Some((12, 9)));
    }

    #[test]
    fn jpeg_sof0_dimensions() {
        // SOI, then a SOF0 marker with a minimal frame header (5x4).
        let mut b = vec![0xFF, 0xD8];
        b.extend_from_slice(&[0xFF, 0xC0]); // SOF0
        b.extend_from_slice(&17u16.to_be_bytes()); // segment length
        b.push(8); // precision
        b.extend_from_slice(&4u16.to_be_bytes()); // height
        b.extend_from_slice(&5u16.to_be_bytes()); // width
        b.extend_from_slice(&[0; 8]); // padding so the loop bound is satisfied
        assert_eq!(image_dimensions(&b), Some((5, 4)));
    }

    #[test]
    fn unknown_bytes_none() {
        assert_eq!(image_dimensions(b"not an image at all here"), None);
        assert_eq!(image_dimensions(b""), None);
    }
}
