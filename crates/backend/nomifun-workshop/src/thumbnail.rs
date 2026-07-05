//! Raster-thumbnail generation for workshop image assets.
//!
//! Decodes an uploaded (or generated) image with the `image` crate, downscales
//! so the longest edge is at most [`THUMB_MAX_EDGE`] (never upscales), and
//! re-encodes as baseline JPEG. Thumbnails live at
//! `{data_dir}/workshop/assets/thumbs/{id}.jpg` (see [`crate::WORKSHOP_REL_DIR`]).
//!
//! CPU-bound: callers run [`encode_thumbnail_jpeg`] inside
//! `tokio::task::spawn_blocking` so a large decode never stalls the runtime.

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

/// Longest-edge cap for a generated thumbnail (contract §M3: 512 px).
pub(crate) const THUMB_MAX_EDGE: u32 = 512;

/// JPEG quality (1–100). 80 is a good size/quality knee for gallery tiles.
const THUMB_JPEG_QUALITY: u8 = 80;

/// File extension (and disk suffix) for a stored thumbnail.
pub(crate) const THUMB_EXT: &str = "jpg";

/// Content-Type served for a stored thumbnail.
pub(crate) const THUMB_MIME: &str = "image/jpeg";

/// Decode `bytes`, downscale to fit a `max_edge`×`max_edge` box (aspect
/// preserved, no upscaling), and encode baseline JPEG. Returns `None` when the
/// bytes are not a decodable image (unknown/corrupt format) — the caller then
/// leaves the asset with no thumbnail rather than failing.
pub(crate) fn encode_thumbnail_jpeg(bytes: &[u8], max_edge: u32) -> Option<Vec<u8>> {
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    // Only shrink. `resize` preserves aspect ratio and fits within the box.
    let scaled = if w.max(h) > max_edge {
        img.resize(max_edge, max_edge, FilterType::Triangle)
    } else {
        img
    };
    // JPEG has no alpha channel — flatten to RGB.
    let rgb = scaled.to_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, THUMB_JPEG_QUALITY)
        .encode_image(&rgb)
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a solid-color RGB PNG of the given size (a real, decodable image —
    /// the header-only fixtures elsewhere can't be decoded).
    pub(crate) fn png_of(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([120, 200, 90]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn downscales_large_image_and_encodes_jpeg() {
        let png = png_of(1000, 500);
        let thumb = encode_thumbnail_jpeg(&png, THUMB_MAX_EDGE).unwrap();
        // JPEG SOI marker.
        assert_eq!(&thumb[0..2], &[0xFF, 0xD8]);
        // The re-decoded thumbnail fits within the box, longest edge == cap.
        let decoded = image::load_from_memory(&thumb).unwrap();
        assert_eq!(decoded.width().max(decoded.height()), THUMB_MAX_EDGE);
        assert!(decoded.width() <= THUMB_MAX_EDGE && decoded.height() <= THUMB_MAX_EDGE);
    }

    #[test]
    fn small_image_is_not_upscaled() {
        let png = png_of(64, 48);
        let thumb = encode_thumbnail_jpeg(&png, THUMB_MAX_EDGE).unwrap();
        let decoded = image::load_from_memory(&thumb).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 48));
    }

    #[test]
    fn undecodable_bytes_yield_none() {
        // A valid PNG *header* with no image data cannot be decoded.
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.extend_from_slice(&[0, 0, 0, 13]);
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&1u32.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert!(encode_thumbnail_jpeg(&b, THUMB_MAX_EDGE).is_none());
        assert!(encode_thumbnail_jpeg(b"not an image", THUMB_MAX_EDGE).is_none());
    }
}
