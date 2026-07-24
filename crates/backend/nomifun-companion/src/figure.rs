//! DIY custom-figure storage for one companion (spec: DIY custom companion figure §3).
//!
//! Two-phase upload: the frontend first lands the processed cutout image
//! under the OS temp upload root via `POST /api/fs/upload`, then this module
//! validates the temp file and atomically installs it as
//! `{companions_dir}/{companion_id}/figure.webp` — so a delete-companion `remove_dir_all`
//! cleans the figure up together with the profile.
//!
//! Validation mirrors `nomifun-requirement`'s attachment ingest: the source
//! must canonicalize to a path inside the upload root (`{temp_dir}/nomifun`,
//! symlink-safe prefix check), carry a WebP or PNG magic number, stay within
//! [`FIGURE_MAX_BYTES`], and measure at most [`FIGURE_MAX_DIM`] pixels per
//! side (spec §3). The install is atomic (unique temp + rename, the
//! crate-wide pattern from [`crate::fsio`]).

use std::path::Path;

use nomifun_common::AppError;

/// File name of the stored figure inside `{companions_dir}/{companion_id}/`. Always
/// `.webp`: the frontend matting pipeline encodes WebP; a transparent PNG
/// passed through keeps its original bytes under this name (the serve
/// handler picks the real Content-Type via [`content_type_of`]).
pub const FIGURE_FILE: &str = "figure.webp";

/// Hard cap on the stored figure size. The generic upload endpoint allows
/// 30MB, but a processed cutout (long edge ≤ 2048) has no business being
/// larger than this.
pub const FIGURE_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Hard cap on either pixel dimension (spec §3: ≤4096×4096).
pub const FIGURE_MAX_DIM: u32 = 4096;

/// Only files inside this root may be ingested — `POST /api/fs/upload` lands
/// here (the same constraint as requirement attachments).
fn upload_root() -> std::path::PathBuf {
    std::env::temp_dir().join("nomifun")
}

/// True when `bytes` starts with a WebP (`RIFF????WEBP`) or PNG
/// (`\x89PNG\r\n\x1a\n`) magic number.
fn has_image_magic(bytes: &[u8]) -> bool {
    let webp = bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP";
    let png = bytes.starts_with(b"\x89PNG\r\n\x1a\n");
    webp || png
}

/// MIME type for serving stored figure bytes, decided by magic number.
/// Anything that passed [`has_image_magic`] is PNG or WebP; default WebP.
pub fn content_type_of(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else {
        "image/webp"
    }
}

/// Pixel dimensions parsed straight from the PNG / WebP container headers
/// (no decoder dependency). `None` when the header is truncated, malformed,
/// or an unknown WebP flavor — callers must reject such files.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    // PNG: signature (8) + IHDR length (4) + "IHDR" (4), then BE u32 w/h.
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
            return None;
        }
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((w, h));
    }
    // WebP: RIFF header (12), then the first chunk fourcc picks the flavor;
    // every chunk payload starts at byte 20 (fourcc 4 + chunk size 4).
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

/// Validate an uploaded figure source (the two-phase-upload temp file) and
/// return its bytes, ready to install. Shared by the per-companion figure
/// ([`ingest_figure`]) and the decoupled figure library
/// ([`crate::figures`]): the source must canonicalize inside the upload
/// sandbox, stay within [`FIGURE_MAX_BYTES`] / [`FIGURE_MAX_DIM`], and carry a
/// real WebP or PNG magic number.
pub fn validate_figure_source(source_path: &Path) -> Result<Vec<u8>, AppError> {
    // Resolve symlinks/`..` first, then prefix-check against the equally
    // canonicalized upload root (macOS /var → /private/var must not bypass).
    let canonical = std::fs::canonicalize(source_path).map_err(|e| {
        AppError::BadRequest(format!(
            "cannot resolve figure source '{}': {e}",
            source_path.display()
        ))
    })?;
    let inside_root = std::fs::canonicalize(upload_root()).is_ok_and(|root| canonical.starts_with(&root));
    if !inside_root {
        return Err(AppError::Forbidden(format!(
            "figure source '{}' is outside the allowed sandbox",
            source_path.display()
        )));
    }

    // Size gate before reading, so an oversized file never enters memory.
    let size = std::fs::metadata(&canonical)
        .map_err(|e| AppError::Internal(format!("stat figure source: {e}")))?
        .len();
    if size > FIGURE_MAX_BYTES {
        return Err(AppError::BadRequest(format!(
            "figure file is too large: {size} bytes (max {FIGURE_MAX_BYTES})"
        )));
    }

    let bytes =
        std::fs::read(&canonical).map_err(|e| AppError::Internal(format!("read figure source: {e}")))?;
    validate_image_payload(&bytes).map_err(AppError::BadRequest)?;
    Ok(bytes)
}

fn validate_image_payload(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 > FIGURE_MAX_BYTES {
        return Err(format!(
            "figure file is too large: {} bytes (max {FIGURE_MAX_BYTES})",
            bytes.len()
        ));
    }
    if !has_image_magic(bytes) {
        return Err("figure file is not a WebP or PNG image".into());
    }
    let (width, height) =
        image_dimensions(bytes).ok_or_else(|| "无法解析图像尺寸".to_owned())?;
    if width > FIGURE_MAX_DIM || height > FIGURE_MAX_DIM {
        return Err(format!(
            "图像尺寸 {width}x{height} 超出上限 {FIGURE_MAX_DIM}x{FIGURE_MAX_DIM}"
        ));
    }
    Ok(())
}

/// Validate bytes already installed in the side store. This deliberately does
/// not apply the upload-root check, but keeps the same format, size and
/// dimension contract so a damaged durable image never becomes a successful
/// HTTP response.
pub(crate) fn validate_figure_bytes(bytes: &[u8]) -> Result<(), AppError> {
    validate_image_payload(bytes)
        .map_err(|detail| AppError::Internal(format!("stored figure is invalid: {detail}")))
}

/// Validate `source_path` and atomically install its bytes as
/// `{companions_dir}/{companion_id}/figure.webp`.
///
/// The caller owns the companion-existence gate (the service 404s unknown companions
/// before calling this, so `companion_id` is always a registry-vetted id).
pub fn ingest_figure(companions_dir: &Path, companion_id: &str, source_path: &Path) -> Result<(), AppError> {
    nomifun_common::CompanionId::try_from(companion_id)
        .map_err(|error| AppError::BadRequest(format!("invalid companion_id: {error}")))?;
    let bytes = validate_figure_source(source_path)?;
    crate::fsio::save_bytes_atomic(&companions_dir.join(companion_id), FIGURE_FILE, &bytes)
        .map_err(|e| AppError::Internal(format!("save companion figure: {e}")))
}

/// The stored figure bytes plus their mtime in unix seconds (the serve
/// handler's ETag input). `None` when this companion has no figure.
pub fn read_figure(
    companions_dir: &Path,
    companion_id: &str,
) -> Result<Option<(Vec<u8>, u64)>, AppError> {
    nomifun_common::CompanionId::try_from(companion_id)
        .map_err(|error| AppError::BadRequest(format!("invalid companion_id: {error}")))?;
    let path = companions_dir.join(companion_id).join(FIGURE_FILE);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Internal(format!(
                "inspect companion figure {}: {error}",
                path.display()
            )));
        }
    };
    if !metadata.file_type().is_file() {
        return Err(AppError::Internal(format!(
            "companion figure is not a regular file: {}",
            path.display()
        )));
    }
    let mtime = metadata
        .modified()
        .map_err(|error| AppError::Internal(format!("read companion figure mtime: {error}")))?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| AppError::Internal(format!("companion figure mtime predates epoch: {error}")))?
        .as_secs();
    let bytes = std::fs::read(&path)
        .map_err(|error| AppError::Internal(format!("read companion figure {}: {error}", path.display())))?;
    validate_figure_bytes(&bytes)?;
    Ok(Some((bytes, mtime)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn companion_fixture(sequence: u64) -> String {
        let raw = format!("0190f5fe-7c00-7a00-8abc-{sequence:012}");
        nomifun_common::CompanionId::try_from(raw.as_str()).unwrap().into_string()
    }

    /// A unique scratch dir inside the allowed upload root
    /// (`{temp_dir}/nomifun`) — figure sources must live under it.
    fn upload_scratch() -> tempfile::TempDir {
        let root = upload_root();
        std::fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("figure-test-")
            .tempdir_in(root)
            .unwrap()
    }

    /// A real 7×5 lossless WebP (VP8L) generated with PIL — passes both the
    /// magic check and dimension parsing.
    fn webp_bytes() -> Vec<u8> {
        vec![
            0x52, 0x49, 0x46, 0x46, 0x1E, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50,
            0x38, 0x4C, 0x11, 0x00, 0x00, 0x00, 0x2F, 0x06, 0x00, 0x01, 0x00, 0x07, 0x50, 0x8A,
            0x2A, 0xD4, 0xA3, 0xFF, 0x81, 0x88, 0xE8, 0x7F, 0x00, 0x00,
        ]
    }

    /// A real 7×5 RGBA PNG generated with PIL.
    fn png_bytes() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x05, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x89, 0x9A, 0xF6, 0xD8, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0xE4, 0x12, 0x91, 0x6B, 0x60, 0xC0, 0x01, 0x98, 0x70, 0x49, 0xD0, 0x50,
            0x12, 0x00, 0x6B, 0x56, 0x00, 0xC6, 0xD1, 0x14, 0x3D, 0x99, 0x00, 0x00, 0x00, 0x00,
            0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    /// A real 12×9 lossy WebP (`VP8 `) generated with PIL.
    fn lossy_webp_bytes() -> Vec<u8> {
        vec![
            0x52, 0x49, 0x46, 0x46, 0x3A, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50,
            0x38, 0x20, 0x2E, 0x00, 0x00, 0x00, 0xF0, 0x01, 0x00, 0x9D, 0x01, 0x2A, 0x0C, 0x00,
            0x09, 0x00, 0x01, 0x40, 0x26, 0x25, 0xA0, 0x02, 0x74, 0xBA, 0x01, 0xF8, 0x00, 0x04,
            0xC8, 0x00, 0x00, 0xFE, 0xAE, 0x17, 0xFF, 0x36, 0x04, 0x0C, 0xD0, 0xFA, 0x60, 0xFF,
            0xD2, 0x6C, 0xF1, 0x36, 0x78, 0x9B, 0x3E, 0x39, 0x80, 0x00,
        ]
    }

    /// Header of a real 21×13 extended WebP (VP8X + ALPH, PIL lossy RGBA) —
    /// `image_dimensions` only reads the first 30 bytes.
    fn vp8x_header_bytes() -> Vec<u8> {
        vec![
            0x52, 0x49, 0x46, 0x46, 0x68, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50,
            0x38, 0x58, 0x0A, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x0C,
            0x00, 0x00,
        ]
    }

    #[test]
    fn image_dimensions_parses_real_samples() {
        assert_eq!(image_dimensions(&png_bytes()), Some((7, 5)));
        assert_eq!(image_dimensions(&webp_bytes()), Some((7, 5)));
        assert_eq!(image_dimensions(&lossy_webp_bytes()), Some((12, 9)));
        assert_eq!(image_dimensions(&vp8x_header_bytes()), Some((21, 13)));
        // Valid magic but garbage payload must parse to nothing.
        assert_eq!(image_dimensions(b"RIFF\x10\x00\x00\x00WEBPVP8 fake-payload"), None);
        assert_eq!(image_dimensions(b"\x89PNG\r\n\x1a\nrest-of-png"), None);
    }

    #[test]
    fn content_type_follows_magic() {
        assert_eq!(content_type_of(&png_bytes()), "image/png");
        assert_eq!(content_type_of(&webp_bytes()), "image/webp");
        assert_eq!(content_type_of(&lossy_webp_bytes()), "image/webp");
    }

    #[test]
    fn ingest_accepts_webp_and_installs_atomically() {
        let upload = upload_scratch();
        let companions = tempfile::tempdir().unwrap();
        let source = upload.path().join("cutout.webp");
        std::fs::write(&source, webp_bytes()).unwrap();

        ingest_figure(companions.path(), &companion_fixture(1), &source).unwrap();

        let companion_dir = companions.path().join(companion_fixture(1));
        assert_eq!(std::fs::read(companion_dir.join(FIGURE_FILE)).unwrap(), webp_bytes());
        // Exactly the figure — no half-written temp file left behind.
        assert_eq!(std::fs::read_dir(&companion_dir).unwrap().count(), 1);

        // PNG passes too (transparent originals skip re-encoding).
        let png = upload.path().join("cutout.png");
        std::fs::write(&png, png_bytes()).unwrap();
        ingest_figure(companions.path(), &companion_fixture(2), &png).unwrap();
        assert!(companions.path().join(companion_fixture(2)).join(FIGURE_FILE).exists());
    }

    #[test]
    fn ingest_rejects_fake_magic() {
        let upload = upload_scratch();
        let companions = tempfile::tempdir().unwrap();
        let source = upload.path().join("fake.webp");
        std::fs::write(&source, b"GIF89a definitely not webp bytes").unwrap();

        let err = ingest_figure(companions.path(), &companion_fixture(1), &source).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "unexpected error: {err}");
        assert!(!companions.path().join(companion_fixture(1)).join(FIGURE_FILE).exists());
    }

    #[test]
    fn ingest_rejects_oversized_dimensions() {
        let upload = upload_scratch();
        let companions = tempfile::tempdir().unwrap();
        // Hand-built IHDR claiming 4097×100 (one past FIGURE_MAX_DIM).
        let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0DIHDR".to_vec();
        bytes.extend_from_slice(&4097u32.to_be_bytes());
        bytes.extend_from_slice(&100u32.to_be_bytes());
        let source = upload.path().join("wide.png");
        std::fs::write(&source, &bytes).unwrap();

        let err = ingest_figure(companions.path(), &companion_fixture(1), &source).unwrap_err();
        match &err {
            AppError::BadRequest(msg) => {
                assert!(msg.contains("4097x100"), "message lacks actual size: {msg}");
            }
            other => panic!("unexpected error: {other}"),
        }
        assert!(!companions.path().join(companion_fixture(1)).join(FIGURE_FILE).exists());
    }

    #[test]
    fn ingest_rejects_unparseable_dimensions() {
        let upload = upload_scratch();
        let companions = tempfile::tempdir().unwrap();
        // Valid WebP magic, but the VP8 payload has no key-frame start code.
        let source = upload.path().join("opaque.webp");
        std::fs::write(&source, b"RIFF\x10\x00\x00\x00WEBPVP8 fake-payload").unwrap();

        let err = ingest_figure(companions.path(), &companion_fixture(1), &source).unwrap_err();
        match &err {
            AppError::BadRequest(msg) => assert!(msg.contains("无法解析图像尺寸"), "msg: {msg}"),
            other => panic!("unexpected error: {other}"),
        }
        assert!(!companions.path().join(companion_fixture(1)).join(FIGURE_FILE).exists());
    }

    #[test]
    fn ingest_rejects_oversized_file() {
        let upload = upload_scratch();
        let companions = tempfile::tempdir().unwrap();
        // Valid magic so the rejection is attributable to size alone.
        let mut bytes = webp_bytes();
        bytes.resize(FIGURE_MAX_BYTES as usize + 1, 0);
        let source = upload.path().join("huge.webp");
        std::fs::write(&source, &bytes).unwrap();

        let err = ingest_figure(companions.path(), &companion_fixture(1), &source).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "unexpected error: {err}");
        assert!(!companions.path().join(companion_fixture(1)).join(FIGURE_FILE).exists());
    }

    #[test]
    fn ingest_rejects_source_outside_upload_root() {
        // tempdir() lands directly in temp_dir(), NOT under {temp_dir}/nomifun.
        let outside = tempfile::tempdir().unwrap();
        let companions = tempfile::tempdir().unwrap();
        let source = outside.path().join("escape.webp");
        std::fs::write(&source, webp_bytes()).unwrap();

        let err = ingest_figure(companions.path(), &companion_fixture(1), &source).unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)), "unexpected error: {err}");
        assert!(!companions.path().join(companion_fixture(1)).join(FIGURE_FILE).exists());
    }

    #[test]
    fn read_returns_bytes_and_mtime() {
        let upload = upload_scratch();
        let companions = tempfile::tempdir().unwrap();
        assert!(read_figure(companions.path(), &companion_fixture(1)).unwrap().is_none());

        let source = upload.path().join("cutout.webp");
        std::fs::write(&source, webp_bytes()).unwrap();
        ingest_figure(companions.path(), &companion_fixture(1), &source).unwrap();

        let (bytes, mtime) = read_figure(companions.path(), &companion_fixture(1)).unwrap().unwrap();
        assert_eq!(bytes, webp_bytes());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(mtime > 0 && mtime <= now + 60, "mtime {mtime} not near now {now}");
    }
}
