use std::collections::HashSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, ImageReader};
use nomi_types::message::ContentBlock;
use tokio::io::AsyncReadExt;

/// A small, explicit envelope keeps a local file from turning into an
/// unbounded request body or decoder allocation.
const MAX_IMAGE_ATTACHMENTS: usize = 4;
const MAX_SOURCE_BYTES: u64 = 12 * 1024 * 1024;
const MAX_SOURCE_PIXELS: u64 = 40_000_000;
const MAX_SOURCE_EDGE: u32 = 16_384;
const MAX_DECODE_ALLOC_BYTES: u64 = 192 * 1024 * 1024;
const MAX_MODEL_EDGE: u32 = 1_568;
const MAX_MODEL_IMAGE_BYTES: usize = 1_500 * 1024;

#[derive(Debug)]
pub(super) struct ImageAttachmentError(String);

impl std::fmt::Display for ImageAttachmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ImageAttachmentError {}

fn attachment_error(message: impl Into<String>) -> ImageAttachmentError {
    ImageAttachmentError(message.into())
}

/// Convert supported local image references into bounded multimodal blocks.
///
/// Non-image files are intentionally ignored: Nomi already receives their
/// paths in the visible user text and can inspect them with its file tools.
/// Image-looking but unsupported files fail explicitly instead of being
/// silently presented as if the model had seen them.
pub(super) async fn load_image_blocks(
    file_references: &[String],
    trusted_root: Option<&Path>,
) -> Result<Vec<ContentBlock>, ImageAttachmentError> {
    let candidates: Vec<(&str, ImageFormat)> = file_references
        .iter()
        .filter_map(|reference| match classify_extension(Path::new(reference)) {
            Ok(Some(format)) => Some(Ok((reference.as_str(), format))),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<_, _>>()?;

    if candidates.len() > MAX_IMAGE_ATTACHMENTS {
        return Err(attachment_error(format!(
            "at most {MAX_IMAGE_ATTACHMENTS} image attachments are allowed"
        )));
    }

    // Restricted sessions (channels, remote access, public-service workers)
    // may only read attachments from their configured write/workspace root.
    // Resolve both sides before comparing so a symlink or Windows junction in
    // a parent component cannot escape the boundary. Local desktop sessions
    // deliberately pass `None` and retain their existing ability to choose any
    // absolute file owned by the OS user.
    let trusted_root = match trusted_root {
        Some(root) if !candidates.is_empty() => {
            let canonical = tokio::fs::canonicalize(root).await.map_err(|_| {
                attachment_error("the configured image attachment root could not be resolved")
            })?;
            let metadata = tokio::fs::metadata(&canonical).await.map_err(|_| {
                attachment_error("the configured image attachment root could not be inspected")
            })?;
            if !metadata.is_dir() || !is_local_absolute_path(&canonical) {
                return Err(attachment_error(
                    "the configured image attachment root must be an absolute local directory",
                ));
            }
            Some(canonical)
        }
        _ => None,
    };

    let mut canonical_seen = HashSet::<PathBuf>::new();
    let mut blocks = Vec::with_capacity(candidates.len());
    for (reference, expected_format) in candidates {
        let path = Path::new(reference);
        if !is_local_absolute_path(path) {
            return Err(attachment_error(format!(
                "image attachment '{}' must be an absolute local path",
                display_name(path)
            )));
        }

        let link_metadata = tokio::fs::symlink_metadata(path).await.map_err(|_| {
            attachment_error(format!(
                "image attachment '{}' does not exist or cannot be read",
                display_name(path)
            ))
        })?;
        if link_metadata.file_type().is_symlink() {
            return Err(attachment_error(format!(
                "image attachment '{}' may not be a symbolic link",
                display_name(path)
            )));
        }

        let canonical = tokio::fs::canonicalize(path).await.map_err(|_| {
            attachment_error(format!(
                "image attachment '{}' could not be resolved",
                display_name(path)
            ))
        })?;
        if !is_local_absolute_path(&canonical) {
            return Err(attachment_error(format!(
                "image attachment '{}' resolved outside a local disk",
                display_name(path)
            )));
        }
        if let Some(root) = &trusted_root
            && !canonical.starts_with(root)
        {
            return Err(attachment_error(format!(
                "image attachment '{}' is outside the allowed workspace",
                display_name(path)
            )));
        }
        if !canonical_seen.insert(canonical.clone()) {
            continue;
        }

        let file = tokio::fs::File::open(&canonical).await.map_err(|_| {
            attachment_error(format!(
                "image attachment '{}' could not be opened",
                display_name(path)
            ))
        })?;
        let metadata = file.metadata().await.map_err(|_| {
            attachment_error(format!(
                "image attachment '{}' metadata could not be read",
                display_name(path)
            ))
        })?;
        if !metadata.is_file() {
            return Err(attachment_error(format!(
                "image attachment '{}' must be a regular file",
                display_name(path)
            )));
        }
        if metadata.len() > MAX_SOURCE_BYTES {
            return Err(attachment_error(format!(
                "image attachment '{}' exceeds the {} MiB file limit",
                display_name(path),
                MAX_SOURCE_BYTES / 1024 / 1024
            )));
        }

        // `take(MAX + 1)` closes the metadata/read race: even if the file grows
        // after metadata(), this process never allocates an unbounded buffer.
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_SOURCE_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| {
                attachment_error(format!(
                    "image attachment '{}' could not be read",
                    display_name(path)
                ))
            })?;
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err(attachment_error(format!(
                "image attachment '{}' exceeds the {} MiB file limit",
                display_name(path),
                MAX_SOURCE_BYTES / 1024 / 1024
            )));
        }

        let name = display_name(path);
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_image(&bytes, expected_format, &name)
        })
        .await
        .map_err(|_| attachment_error("image preparation task failed"))??;

        blocks.push(ContentBlock::Image {
            media_type: prepared.media_type.to_string(),
            data: STANDARD.encode(prepared.bytes),
        });
    }

    Ok(blocks)
}

struct PreparedImage {
    media_type: &'static str,
    bytes: Vec<u8>,
}

fn prepare_image(
    bytes: &[u8],
    expected_format: ImageFormat,
    display_name: &str,
) -> Result<PreparedImage, ImageAttachmentError> {
    let dimension_reader = image_reader(bytes, display_name)?;
    let detected_format = dimension_reader.format().ok_or_else(|| {
        attachment_error(format!("image attachment '{display_name}' has an unknown format"))
    })?;
    if detected_format != expected_format {
        return Err(attachment_error(format!(
            "image attachment '{display_name}' content does not match its file extension"
        )));
    }

    let (width, height) = dimension_reader.into_dimensions().map_err(|_| {
        attachment_error(format!(
            "image attachment '{display_name}' dimensions could not be read"
        ))
    })?;
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if width == 0 || height == 0 {
        return Err(attachment_error(format!(
            "image attachment '{display_name}' has invalid zero-sized dimensions"
        )));
    }
    if width > MAX_SOURCE_EDGE || height > MAX_SOURCE_EDGE {
        return Err(attachment_error(format!(
            "image attachment '{display_name}' exceeds the {MAX_SOURCE_EDGE}px dimension limit"
        )));
    }
    if pixels > MAX_SOURCE_PIXELS {
        return Err(attachment_error(format!(
            "image attachment '{display_name}' exceeds the 40 megapixel limit"
        )));
    }

    let mut decode_reader = image_reader(bytes, display_name)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_EDGE);
    limits.max_image_height = Some(MAX_SOURCE_EDGE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    decode_reader.limits(limits);
    let image = decode_reader.decode().map_err(|_| {
        attachment_error(format!(
            "image attachment '{display_name}' is corrupt or too large to decode"
        ))
    })?;

    encode_bounded(image, detected_format, display_name)
}

fn image_reader<'a>(
    bytes: &'a [u8],
    display_name: &str,
) -> Result<ImageReader<Cursor<&'a [u8]>>, ImageAttachmentError> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| attachment_error(format!("image attachment '{display_name}' could not be inspected")))
}

fn encode_bounded(
    image: DynamicImage,
    source_format: ImageFormat,
    display_name: &str,
) -> Result<PreparedImage, ImageAttachmentError> {
    let resized = (image.width() > MAX_MODEL_EDGE || image.height() > MAX_MODEL_EDGE)
        .then(|| image.thumbnail(MAX_MODEL_EDGE, MAX_MODEL_EDGE));
    let model_image = resized.as_ref().unwrap_or(&image);

    // PNG is preferable for screenshots and text-heavy diagrams. Re-encoding
    // strips metadata; if lossless output is too large we fall back to JPEG.
    if source_format == ImageFormat::Png {
        let mut png = Vec::new();
        model_image
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .map_err(|_| attachment_error(format!("image attachment '{display_name}' could not be encoded")))?;
        if png.len() <= MAX_MODEL_IMAGE_BYTES {
            return Ok(PreparedImage {
                media_type: "image/png",
                bytes: png,
            });
        }
    }

    // Multiple quality/size rungs guarantee a hard encoded-body ceiling while
    // retaining as much detail as ordinary screenshots and phone photos need.
    for edge in [MAX_MODEL_EDGE, 1_280, 1_024, 768] {
        let candidate = if image.width() > edge || image.height() > edge {
            image.thumbnail(edge, edge).to_rgb8()
        } else {
            image.to_rgb8()
        };
        for quality in [85, 70, 55] {
            let mut jpeg = Vec::new();
            JpegEncoder::new_with_quality(&mut jpeg, quality)
                .encode_image(&candidate)
                .map_err(|_| {
                    attachment_error(format!(
                        "image attachment '{display_name}' could not be encoded"
                    ))
                })?;
            if jpeg.len() <= MAX_MODEL_IMAGE_BYTES {
                return Ok(PreparedImage {
                    media_type: "image/jpeg",
                    bytes: jpeg,
                });
            }
        }
    }

    Err(attachment_error(format!(
        "image attachment '{display_name}' could not be reduced below the request size limit"
    )))
}

fn classify_extension(path: &Path) -> Result<Option<ImageFormat>, ImageAttachmentError> {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return Ok(None);
    };
    let extension = extension.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Ok(Some(ImageFormat::Png)),
        "jpg" | "jpeg" => Ok(Some(ImageFormat::Jpeg)),
        "webp" => Ok(Some(ImageFormat::WebP)),
        "gif" | "bmp" | "tif" | "tiff" | "ico" | "avif" | "heic" | "heif" | "svg" => {
            Err(attachment_error(format!(
                "image attachment '{}' uses unsupported .{extension} format; use PNG, JPEG, or WebP",
                display_name(path)
            )))
        }
        _ => Ok(None),
    }
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("image")
        .to_string()
}

fn is_local_absolute_path(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }

    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};

        return matches!(
            path.components().next(),
            Some(Component::Prefix(prefix))
                if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        );
    }

    #[cfg(not(windows))]
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, Rgb, RgbImage};

    fn write_png(path: &Path, width: u32, height: u32) {
        let image = RgbImage::from_pixel(width, height, Rgb([20, 100, 220]));
        DynamicImage::ImageRgb8(image)
            .save_with_format(path, ImageFormat::Png)
            .unwrap();
    }

    #[tokio::test]
    async fn valid_png_becomes_a_decodable_base64_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.png");
        write_png(&path, 32, 24);

        let blocks = load_image_blocks(&[path.to_string_lossy().into_owned()], None)
            .await
            .unwrap();
        assert_eq!(blocks.len(), 1);
        let ContentBlock::Image { media_type, data } = &blocks[0] else {
            panic!("expected image block");
        };
        assert_eq!(media_type, "image/png");
        let decoded = STANDARD.decode(data).unwrap();
        assert_eq!(image::load_from_memory(&decoded).unwrap().dimensions(), (32, 24));
    }

    #[tokio::test]
    async fn large_image_is_resized_before_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.jpg");
        let image = RgbImage::from_pixel(2_400, 1_200, Rgb([10, 30, 60]));
        DynamicImage::ImageRgb8(image)
            .save_with_format(&path, ImageFormat::Jpeg)
            .unwrap();

        let blocks = load_image_blocks(&[path.to_string_lossy().into_owned()], None)
            .await
            .unwrap();
        let ContentBlock::Image { data, .. } = &blocks[0] else {
            panic!("expected image block");
        };
        let decoded = STANDARD.decode(data).unwrap();
        let prepared = image::load_from_memory(&decoded).unwrap();
        assert!(prepared.width() <= MAX_MODEL_EDGE);
        assert!(prepared.height() <= MAX_MODEL_EDGE);
        assert!(decoded.len() <= MAX_MODEL_IMAGE_BYTES);
    }

    #[tokio::test]
    async fn ordinary_non_image_files_keep_the_legacy_text_file_flow() {
        let blocks = load_image_blocks(&["relative/source.rs".into(), "notes.txt".into()], None)
            .await
            .unwrap();
        assert!(blocks.is_empty());
    }

    #[tokio::test]
    async fn relative_image_paths_are_rejected() {
        let error = load_image_blocks(&["relative/image.png".into()], None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("absolute local path"));
    }

    #[tokio::test]
    async fn image_urls_are_rejected_as_non_local_paths() {
        let error = load_image_blocks(&["https://example.test/image.png".into()], None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("absolute local path"));
    }

    #[tokio::test]
    async fn image_count_is_bounded_before_file_io() {
        let references = (0..=MAX_IMAGE_ATTACHMENTS)
            .map(|index| format!("C:\\images\\{index}.png"))
            .collect::<Vec<_>>();
        let error = load_image_blocks(&references, None).await.unwrap_err();
        assert!(error.to_string().contains("at most 4"));
    }

    #[tokio::test]
    async fn extension_spoofing_is_rejected_after_magic_detection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-really.png");
        std::fs::write(&path, b"not an image").unwrap();

        let error = load_image_blocks(&[path.to_string_lossy().into_owned()], None)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("unknown format")
                || error.to_string().contains("dimensions could not be read")
        );
    }

    #[tokio::test]
    async fn source_byte_limit_is_checked_before_reading_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.png");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_SOURCE_BYTES + 1).unwrap();

        let error = load_image_blocks(&[path.to_string_lossy().into_owned()], None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("file limit"));
    }

    #[tokio::test]
    async fn restricted_root_accepts_an_image_inside_the_root() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("inside.png");
        write_png(&path, 24, 16);

        let blocks = load_image_blocks(
            &[path.to_string_lossy().into_owned()],
            Some(root.path()),
        )
        .await
        .unwrap();

        assert_eq!(blocks.len(), 1);
    }

    #[tokio::test]
    async fn restricted_root_rejects_an_image_outside_the_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("outside.png");
        write_png(&path, 24, 16);

        let error = load_image_blocks(
            &[path.to_string_lossy().into_owned()],
            Some(root.path()),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("outside the allowed workspace"));
    }

    #[tokio::test]
    async fn unrestricted_desktop_session_accepts_an_image_outside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("chosen.png");
        write_png(&path, 24, 16);

        // `workspace` intentionally stays unused as an authority: passing None
        // is the local-desktop trust model and permits any absolute local path.
        let _workspace = workspace;
        let blocks = load_image_blocks(&[path.to_string_lossy().into_owned()], None)
            .await
            .unwrap();

        assert_eq!(blocks.len(), 1);
    }

    #[tokio::test]
    async fn restricted_root_rejects_a_parent_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_image = outside.path().join("escaped.png");
        write_png(&outside_image, 24, 16);
        let link = root.path().join("escape");

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_dir(outside.path(), &link) {
            // Creating symlinks may require Developer Mode or elevated rights
            // on older Windows hosts. The containment behavior is still covered
            // by the direct outside-root test above on such machines.
            if error.kind() == std::io::ErrorKind::PermissionDenied
                || error.raw_os_error() == Some(1314)
            {
                return;
            }
            panic!("could not create test directory symlink: {error}");
        }

        let escaped = link.join("escaped.png");
        let error = load_image_blocks(
            &[escaped.to_string_lossy().into_owned()],
            Some(root.path()),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("outside the allowed workspace"));
    }
}
