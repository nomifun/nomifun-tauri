use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};

use nomi_protocol::events::ToolCategory;
use nomi_types::file_state::FileState;
use nomi_types::tool::{JsonSchema, ToolImage, ToolResult};

use crate::Tool;
use crate::file_cache::{FileStateCache, file_mtime_ms};
use crate::output_truncation::{TruncationBudget, truncate_middle};

/// Stub returned when a file has not changed since the model last read it.
/// Saves tokens by avoiding re-sending identical content.
const FILE_UNCHANGED_STUB: &str = "File unchanged since last read. The content from the earlier Read \
     tool_result in this conversation is still current — refer to that \
     instead of re-reading.";

/// Image read returns the bytes to the LLM as a ToolImage instead of the
/// "(binary file)" stub. Capped so a huge image cannot blow up the request.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// A batch may contain several image paths, but it must not multiply the
/// single-file multimodal payload bound. This is the padded base64 size of one
/// maximum-size image.
const MAX_BATCH_IMAGE_DATA_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;

/// Amazon Bedrock Converse, the strictest supported provider, accepts at most
/// 20 images in one request.
const MAX_BATCH_IMAGES: usize = 20;

/// Keep a single model-visible Read invocation bounded while still covering
/// the common "inspect a known set of source files" case.
const MAX_BATCH_FILES: usize = 32;

/// Batch sections share the same result budget advertised by `ReadTool`.
const MAX_RESULT_BYTES: usize = 100_000;

#[derive(Clone, Copy)]
struct BatchImageBudget {
    data_bytes: usize,
    slots: usize,
}

/// MIME type for image extensions the LLM API accepts as image content blocks
/// (jpeg/png/gif/webp). bmp/tiff keep the binary stub; svg is text and is read
/// as source like any text file.
fn image_media_type(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[derive(Clone)]
pub struct ReadTool {
    file_cache: Option<Arc<RwLock<FileStateCache>>>,
    /// Session working directory used to resolve relative `file_path` inputs
    /// (matching Grep/Glob/Bash). `None` leaves relative paths resolving
    /// against the process cwd (legacy behavior).
    cwd: Option<PathBuf>,
}

impl ReadTool {
    /// Create a ReadTool with optional file state cache for dedup and an
    /// optional session cwd for resolving relative paths.
    ///
    /// Pass `None` for `file_cache` to disable caching (all reads return full
    /// content). Pass `None` for `cwd` to keep relative paths resolving against
    /// the process working directory.
    pub fn new(file_cache: Option<Arc<RwLock<FileStateCache>>>, cwd: Option<PathBuf>) -> Self {
        Self { file_cache, cwd }
    }

    fn requested_paths(input: &Value) -> Result<Vec<String>, String> {
        let has_single = input.get("file_path").is_some();
        let has_batch = input.get("file_paths").is_some();
        if has_single == has_batch {
            return Err("Provide exactly one of file_path or file_paths".to_string());
        }

        if has_single {
            let path = input
                .get("file_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "file_path must be a string".to_string())?;
            return Ok(vec![path.to_string()]);
        }

        let paths = input
            .get("file_paths")
            .and_then(Value::as_array)
            .ok_or_else(|| "file_paths must be an array of strings".to_string())?;
        if paths.is_empty() {
            return Err("file_paths must contain at least one path".to_string());
        }
        if paths.len() > MAX_BATCH_FILES {
            return Err(format!(
                "file_paths accepts at most {MAX_BATCH_FILES} paths per Read call"
            ));
        }

        let mut seen = HashSet::with_capacity(paths.len());
        let mut requested = Vec::with_capacity(paths.len());
        for path in paths {
            let path = path
                .as_str()
                .ok_or_else(|| "every file_paths entry must be a string".to_string())?;
            if seen.insert(path.to_string()) {
                requested.push(path.to_string());
            }
        }
        Ok(requested)
    }

    fn read_one(
        &self,
        raw_path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
        batch_image_budget: Option<BatchImageBudget>,
    ) -> ToolResult {
        // Relative paths resolve against the session working directory when one
        // was injected (matching Grep/Glob/Bash). Everything below — including
        // the cache key — uses the resolved path so dedup stays consistent.
        let resolved: String = match &self.cwd {
            Some(cwd) if !Path::new(raw_path).is_absolute() => {
                cwd.join(raw_path).to_string_lossy().into_owned()
            }
            _ => raw_path.to_owned(),
        };
        let file_path = resolved.as_str();

        // Get file mtime for dedup and cache.
        let mtime_ms = file_mtime_ms(Path::new(file_path));

        // Dedup check: if cache has the same file with matching offset/limit and mtime,
        // return a short stub instead of full content.
        if let (Some(cache_arc), Some(current_mtime)) = (&self.file_cache, mtime_ms)
            && let Ok(mut cache) = cache_arc.write()
            && let Some(cached) = cache.get(Path::new(file_path))
            && cached.offset == offset
            && cached.limit == limit
            && cached.mtime_ms == current_mtime
        {
            return ToolResult {
                content: FILE_UNCHANGED_STUB.to_string(),
                is_error: false,
                images: Vec::new(),
            };
        }

        // Read file from disk.
        let content = match std::fs::read(file_path) {
            Ok(bytes) => bytes,
            Err(e) => {
                return ToolResult {
                    content: format!("Failed to read file {}: {}", file_path, e),
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };

        // Image files come back as a multimodal result via ToolResult.images —
        // the same channel screenshots use. Other binaries keep the stub below.
        if let Some(media_type) = image_media_type(file_path) {
            if content.len() > MAX_IMAGE_BYTES {
                return ToolResult {
                    content: format!(
                        "(image file too large to attach: {} bytes, max {} bytes)",
                        content.len(),
                        MAX_IMAGE_BYTES
                    ),
                    is_error: false,
                    images: Vec::new(),
                };
            }
            let encoded_bytes = content.len().div_ceil(3) * 4;
            if let Some(budget) = batch_image_budget {
                let reason = if budget.slots == 0 {
                    Some(format!("the batch already retained {MAX_BATCH_IMAGES} images"))
                } else if encoded_bytes > budget.data_bytes {
                    Some(format!(
                        "the batch image payload would exceed {MAX_BATCH_IMAGE_DATA_BYTES} base64 bytes"
                    ))
                } else {
                    None
                };
                if let Some(reason) = reason {
                    return ToolResult {
                        content: format!(
                            "(image: {file_path}, {} bytes, {media_type}; attachment omitted before base64 encoding because {reason})",
                            content.len()
                        ),
                        is_error: false,
                        images: Vec::new(),
                    };
                }
            }
            let data = base64::engine::general_purpose::STANDARD.encode(&content);
            return ToolResult {
                content: format!("(image: {}, {} bytes, {})", file_path, content.len(), media_type),
                is_error: false,
                images: vec![ToolImage {
                    media_type: media_type.to_string(),
                    data,
                }],
            };
        }

        // Check if binary.
        if content.iter().take(8192).any(|&b| b == 0) {
            return ToolResult {
                content: format!("(binary file, {} bytes)", content.len()),
                is_error: false,
                images: Vec::new(),
            };
        }

        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();

        let effective_offset = offset.unwrap_or(0);
        let effective_limit = limit.unwrap_or(lines.len());

        let end = effective_offset.saturating_add(effective_limit).min(lines.len());
        let slice = &lines[effective_offset.min(lines.len())..end];

        let numbered: Vec<String> = slice
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}", effective_offset + i + 1, line))
            .collect();

        let result_content = numbered.join("\n");

        // Update cache after successful read.
        if let Some(cache_arc) = &self.file_cache
            && let (Ok(mut cache), Some(mtime)) = (cache_arc.write(), mtime_ms)
        {
            cache.insert(
                file_path.into(),
                FileState {
                    content: result_content.clone(),
                    mtime_ms: mtime,
                    offset,
                    limit,
                },
            );
        }

        ToolResult {
            content: result_content,
            is_error: false,
            images: Vec::new(),
        }
    }

    fn render_batch_with<F>(paths: &[String], mut read: F) -> ToolResult
    where
        F: FnMut(&str, BatchImageBudget) -> ToolResult,
    {
        let total = paths.len();
        let display_paths: Vec<String> = paths
            .iter()
            .map(|path| truncate_middle(path, TruncationBudget::Bytes(512)))
            .collect();
        let overhead_bytes: usize = display_paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                format!(
                    "===== FILE {}/{}: {} =====\n\n===== END FILE {}/{} =====\n",
                    index + 1,
                    total,
                    path,
                    index + 1,
                    total
                )
                .len()
            })
            .sum();
        let body_budget = MAX_RESULT_BYTES
            .saturating_sub(overhead_bytes)
            .checked_div(total.max(1))
            .unwrap_or(0)
            .saturating_sub(64);

        let mut content = String::with_capacity(MAX_RESULT_BYTES);
        let mut images = Vec::new();
        let mut image_data_bytes = 0_usize;
        let mut is_error = false;
        for (index, (path, display_path)) in paths.iter().zip(display_paths).enumerate() {
            let result = read(
                path,
                BatchImageBudget {
                    data_bytes: MAX_BATCH_IMAGE_DATA_BYTES.saturating_sub(image_data_bytes),
                    slots: MAX_BATCH_IMAGES.saturating_sub(images.len()),
                },
            );
            let ToolResult {
                content: mut body,
                is_error: entry_is_error,
                images: entry_images,
            } = result;
            let mut omitted_images = 0_usize;
            for image in entry_images {
                let next_bytes = image_data_bytes.saturating_add(image.data.len());
                if images.len() < MAX_BATCH_IMAGES && next_bytes <= MAX_BATCH_IMAGE_DATA_BYTES {
                    image_data_bytes = next_bytes;
                    images.push(image);
                } else {
                    omitted_images = omitted_images.saturating_add(1);
                }
            }
            if omitted_images > 0 {
                body.push_str(&format!(
                    "\n({omitted_images} image attachment omitted because the batch exceeded {MAX_BATCH_IMAGES} images or {MAX_BATCH_IMAGE_DATA_BYTES} base64 bytes)"
                ));
            }
            if index > 0 {
                content.push('\n');
            }
            content.push_str(&format!(
                "===== FILE {}/{}: {} =====\n",
                index + 1,
                total,
                display_path
            ));
            content.push_str(&truncate_middle(
                &body,
                TruncationBudget::Bytes(body_budget),
            ));
            content.push_str(&format!(
                "\n===== END FILE {}/{} =====\n",
                index + 1,
                total
            ));
            is_error |= entry_is_error;
        }

        ToolResult {
            content,
            is_error,
            images,
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Reads one or more files from the local filesystem. Returns content with line numbers.\n\n\
         Usage:\n\
         - Use file_path for one file, or file_paths for several already-known files that need the same slice.\n\
         - Prefer absolute paths; relative paths are resolved against the session working directory.\n\
         - By default, it reads the entire file. Use offset and limit for partial reads on large files.\n\
         - Results are returned with line numbers (1-based) followed by a tab and the line content.\n\
         - Image files (jpg/png/gif/webp) are returned as viewable images. Other binary files return \"(binary file, N bytes)\".\n\
         - This tool can only read files, not directories. To list a directory, use Bash with ls."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file to read (absolute preferred; a relative path resolves against the session working directory)"
                },
                "file_paths": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_BATCH_FILES,
                    "items": { "type": "string" },
                    "description": "Paths to read in one call, preserving first-seen order (absolute preferred; relative paths resolve against the session working directory)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (0-based)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read"
                }
            },
            "oneOf": [
                {
                    "required": ["file_path"],
                    "not": { "required": ["file_paths"] }
                },
                {
                    "required": ["file_paths"],
                    "not": { "required": ["file_path"] }
                }
            ]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let offset = input["offset"].as_u64().map(|v| v as usize);
        let limit = input["limit"].as_u64().map(|v| v as usize);

        let paths = match Self::requested_paths(&input) {
            Ok(paths) => paths,
            Err(content) => {
                return ToolResult {
                    content,
                    is_error: true,
                    images: Vec::new(),
                };
            }
        };

        let single = paths.len() == 1 && input.get("file_path").is_some();
        let worker = self.clone();
        match tokio::task::spawn_blocking(move || {
            if single {
                return worker.read_one(&paths[0], offset, limit, None);
            }
            Self::render_batch_with(&paths, |path, image_budget| {
                worker.read_one(path, offset, limit, Some(image_budget))
            })
        })
        .await
        {
            Ok(result) => result,
            Err(error) => ToolResult::error(format!("Read worker failed: {error}")),
        }
    }

    fn max_result_size(&self) -> usize {
        MAX_RESULT_BYTES
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }

    fn describe(&self, input: &Value) -> String {
        if let Some(paths) = input.get("file_paths").and_then(Value::as_array) {
            return format!("Read {} files", paths.len());
        }
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        format!("Read {}", path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::tempdir;

    use nomi_config::file_cache::FileCacheConfig;

    fn make_cache() -> Arc<RwLock<FileStateCache>> {
        let config = FileCacheConfig {
            max_entries: 100,
            max_size_bytes: 25 * 1024 * 1024,
            enabled: true,
        };
        Arc::new(RwLock::new(FileStateCache::new(&config)))
    }

    // -- Basic read tests (no cache) --

    #[tokio::test]
    async fn test_read_file_full() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        let mut file = std::fs::File::create(&file_path).unwrap();
        writeln!(file, "line one").unwrap();
        writeln!(file, "line two").unwrap();
        writeln!(file, "line three").unwrap();
        drop(file);

        let tool = ReadTool::new(None, None);
        let input = json!({ "file_path": file_path.to_str().unwrap() });
        let result = tool.execute(input).await;

        assert!(!result.is_error);
        assert!(result.content.contains("1\tline one"));
        assert!(result.content.contains("2\tline two"));
        assert!(result.content.contains("3\tline three"));
    }

    #[tokio::test]
    async fn test_read_file_with_offset_and_limit() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("lines.txt");
        let mut file = std::fs::File::create(&file_path).unwrap();
        for i in 1..=10 {
            writeln!(file, "line {}", i).unwrap();
        }
        drop(file);

        let tool = ReadTool::new(None, None);
        let input = json!({
            "file_path": file_path.to_str().unwrap(),
            "offset": 2,
            "limit": 3
        });
        let result = tool.execute(input).await;

        assert!(!result.is_error);
        let lines: Vec<&str> = result.content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("3\tline 3"));
        assert!(lines[1].contains("4\tline 4"));
        assert!(lines[2].contains("5\tline 5"));
    }

    #[tokio::test]
    async fn test_read_nonexistent_file() {
        let tool = ReadTool::new(None, None);
        let input = json!({ "file_path": "/tmp/nonexistent_file_abc123.txt" });
        let result = tool.execute(input).await;

        assert!(result.is_error);
        assert!(result.content.contains("Failed to read file"));
    }

    #[tokio::test]
    async fn test_read_empty_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("empty.txt");
        std::fs::File::create(&file_path).unwrap();

        let tool = ReadTool::new(None, None);
        let input = json!({ "file_path": file_path.to_str().unwrap() });
        let result = tool.execute(input).await;

        assert!(!result.is_error);
        assert!(result.content.is_empty());
    }

    #[tokio::test]
    async fn test_read_large_file_truncation() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("large.txt");
        let mut file = std::fs::File::create(&file_path).unwrap();
        for i in 1..=200 {
            writeln!(file, "line number {}", i).unwrap();
        }
        drop(file);

        let tool = ReadTool::new(None, None);
        let input = json!({ "file_path": file_path.to_str().unwrap() });
        let result = tool.execute(input).await;

        assert!(!result.is_error);
        let lines: Vec<&str> = result.content.lines().collect();
        assert_eq!(lines.len(), 200);
        assert!(lines[0].contains("1\tline number 1"));
        assert!(lines[199].contains("200\tline number 200"));
    }

    // -- Dedup tests (with cache) --

    #[tokio::test]
    async fn dedup_returns_stub_on_unchanged_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("dedup.txt");
        std::fs::write(&file_path, "hello\n").unwrap();

        let cache = make_cache();
        let tool = ReadTool::new(Some(cache), None);

        let input = json!({ "file_path": file_path.to_str().unwrap() });

        // First read: full content.
        let r1 = tool.execute(input.clone()).await;
        assert!(!r1.is_error);
        assert!(r1.content.contains("hello"));

        // Second read: dedup stub.
        let r2 = tool.execute(input).await;
        assert!(!r2.is_error);
        assert_eq!(r2.content, FILE_UNCHANGED_STUB);
    }

    #[tokio::test]
    async fn dedup_returns_new_content_after_modification() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("modified.txt");
        std::fs::write(&file_path, "version1\n").unwrap();

        let cache = make_cache();
        let tool = ReadTool::new(Some(cache), None);

        let input = json!({ "file_path": file_path.to_str().unwrap() });

        let r1 = tool.execute(input.clone()).await;
        assert!(r1.content.contains("version1"));

        // Modify the file — ensure mtime changes.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&file_path, "version2\n").unwrap();

        let r2 = tool.execute(input).await;
        assert!(!r2.is_error);
        assert!(r2.content.contains("version2"));
    }

    #[tokio::test]
    async fn dedup_different_offset_limit_returns_full() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("multi.txt");
        let mut file = std::fs::File::create(&file_path).unwrap();
        for i in 1..=20 {
            writeln!(file, "line {}", i).unwrap();
        }
        drop(file);

        let cache = make_cache();
        let tool = ReadTool::new(Some(cache), None);

        let input1 = json!({
            "file_path": file_path.to_str().unwrap(),
            "offset": 0,
            "limit": 10
        });
        let r1 = tool.execute(input1).await;
        assert!(!r1.is_error);
        assert!(r1.content.contains("line 1"));

        // Different range: should return full content, not stub.
        let input2 = json!({
            "file_path": file_path.to_str().unwrap(),
            "offset": 10,
            "limit": 10
        });
        let r2 = tool.execute(input2).await;
        assert!(!r2.is_error);
        assert!(r2.content.contains("line 11"));
        assert!(!r2.content.contains(FILE_UNCHANGED_STUB));
    }

    #[tokio::test]
    async fn no_cache_always_returns_full_content() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("nocache.txt");
        std::fs::write(&file_path, "data\n").unwrap();

        let tool = ReadTool::new(None, None);
        let input = json!({ "file_path": file_path.to_str().unwrap() });

        let r1 = tool.execute(input.clone()).await;
        assert!(r1.content.contains("data"));

        let r2 = tool.execute(input).await;
        assert!(r2.content.contains("data"));
        assert_ne!(r2.content, FILE_UNCHANGED_STUB);
    }

    #[tokio::test]
    async fn nonexistent_file_not_cached() {
        let cache = make_cache();
        let tool = ReadTool::new(Some(cache.clone()), None);

        let input = json!({ "file_path": "/tmp/nonexistent_xyz_789.txt" });
        let r = tool.execute(input).await;
        assert!(r.is_error);

        // Cache should be empty.
        let c = cache.read().unwrap();
        assert!(c.is_empty());
    }

    #[tokio::test]
    async fn dedup_empty_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("empty.txt");
        std::fs::File::create(&file_path).unwrap();

        let cache = make_cache();
        let tool = ReadTool::new(Some(cache), None);

        let input = json!({ "file_path": file_path.to_str().unwrap() });

        let r1 = tool.execute(input.clone()).await;
        assert!(!r1.is_error);

        let r2 = tool.execute(input).await;
        assert!(!r2.is_error);
        assert_eq!(r2.content, FILE_UNCHANGED_STUB);
    }

    // -- Image branch tests --

    /// Minimal valid PNG header bytes (enough to be a "binary" file with NUL bytes).
    const PNG_BYTES: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D];

    #[tokio::test]
    async fn read_png_returns_multimodal_image() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("pic.png");
        std::fs::write(&file_path, PNG_BYTES).unwrap();

        let tool = ReadTool::new(None, None);
        let result = tool.execute(json!({ "file_path": file_path.to_str().unwrap() })).await;

        assert!(!result.is_error);
        assert_eq!(result.images.len(), 1, "png must come back as a ToolImage");
        assert_eq!(result.images[0].media_type, "image/png");
        assert!(!result.images[0].data.is_empty());
        assert!(result.content.contains("image"), "text content should describe the image");
        assert!(!result.content.contains("(binary file"), "image must not fall through to the binary stub");
    }

    #[tokio::test]
    async fn read_oversized_image_returns_hint_without_image() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("big.png");
        // > 5MB of zeroes
        std::fs::write(&file_path, vec![0u8; 5 * 1024 * 1024 + 1]).unwrap();

        let tool = ReadTool::new(None, None);
        let result = tool.execute(json!({ "file_path": file_path.to_str().unwrap() })).await;

        assert!(!result.is_error);
        assert!(result.images.is_empty());
        assert!(result.content.contains("too large"));
    }

    #[tokio::test]
    async fn read_non_image_binary_unchanged() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("blob.bin");
        std::fs::write(&file_path, [0u8, 1, 2, 3]).unwrap();

        let tool = ReadTool::new(None, None);
        let result = tool.execute(json!({ "file_path": file_path.to_str().unwrap() })).await;

        assert!(!result.is_error);
        assert!(result.images.is_empty());
        assert!(result.content.contains("(binary file"));
    }

    // -- Relative-path resolution (session cwd) --

    #[tokio::test]
    async fn relative_path_resolves_against_cwd() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("rel.txt"), "cwd resolved content\n").unwrap();

        let tool = ReadTool::new(None, Some(dir.path().to_path_buf()));
        let result = tool.execute(json!({ "file_path": "rel.txt" })).await;

        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert!(result.content.contains("cwd resolved content"));
    }

    #[tokio::test]
    async fn relative_subdir_path_resolves_against_cwd() {
        // The prompt hands the model paths like "./.nomi/requirement-attachments/…" —
        // a ./-prefixed nested relative path must resolve under the session cwd.
        let dir = tempdir().unwrap();
        let sub = dir.path().join(".nomi").join("requirement-attachments");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("note.txt"), "staged attachment\n").unwrap();

        let tool = ReadTool::new(None, Some(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({ "file_path": "./.nomi/requirement-attachments/note.txt" }))
            .await;

        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert!(result.content.contains("staged attachment"));
    }

    #[tokio::test]
    async fn relative_path_without_cwd_fails_gracefully() {
        // No cwd injected → legacy behavior: the relative path is tried as-is
        // against the process cwd; a missing file is a read failure, not a panic.
        let tool = ReadTool::new(None, None);
        let result = tool
            .execute(json!({ "file_path": "definitely_missing_rel_file_xyz_42.txt" }))
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("Failed to read file"));
    }

    #[tokio::test]
    async fn relative_path_dedup_uses_resolved_cache_key() {
        // The cache key must be the RESOLVED path so dedup behaves identically
        // whether the model passes the relative or the absolute form.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("rel.txt"), "hello\n").unwrap();

        let cache = make_cache();
        let tool = ReadTool::new(Some(cache), Some(dir.path().to_path_buf()));

        let r1 = tool.execute(json!({ "file_path": "rel.txt" })).await;
        assert!(r1.content.contains("hello"));

        // Same file via its absolute path → same cache entry → dedup stub.
        let abs = dir.path().join("rel.txt");
        let r2 = tool.execute(json!({ "file_path": abs.to_str().unwrap() })).await;
        assert!(!r2.is_error);
        assert_eq!(r2.content, FILE_UNCHANGED_STUB);
    }

    // -- Batched reads --

    #[tokio::test]
    async fn batch_reads_known_files_and_populates_shared_cache() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        std::fs::write(&first, "alpha\n").unwrap();
        std::fs::write(&second, "beta\n").unwrap();

        let cache = make_cache();
        let tool = ReadTool::new(Some(cache.clone()), None);
        let result = tool
            .execute(json!({
                "file_paths": [first.to_str().unwrap(), second.to_str().unwrap()]
            }))
            .await;

        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert!(result.content.contains(first.to_str().unwrap()));
        assert!(result.content.contains(second.to_str().unwrap()));
        assert!(result.content.contains("alpha"));
        assert!(result.content.contains("beta"));

        let mut cache = cache.write().unwrap();
        assert!(cache.get(&first).is_some(), "first file must be cached");
        assert!(cache.get(&second).is_some(), "second file must be cached");
    }

    #[tokio::test]
    async fn batch_rejects_empty_oversized_and_mixed_path_inputs() {
        let tool = ReadTool::new(None, None);

        let empty = tool.execute(json!({ "file_paths": [] })).await;
        assert!(empty.is_error);
        assert!(empty.content.contains("at least one"));

        let oversized: Vec<String> = (0..=MAX_BATCH_FILES)
            .map(|index| format!("file-{index}.txt"))
            .collect();
        let oversized = tool.execute(json!({ "file_paths": oversized })).await;
        assert!(oversized.is_error);
        assert!(oversized.content.contains(&MAX_BATCH_FILES.to_string()));

        let mixed = tool
            .execute(json!({
                "file_path": "one.txt",
                "file_paths": ["two.txt"]
            }))
            .await;
        assert!(mixed.is_error);
        assert!(mixed.content.contains("exactly one"));
    }

    #[tokio::test]
    async fn batch_deduplicates_paths_in_first_seen_order() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        std::fs::write(&first, "alpha\n").unwrap();
        std::fs::write(&second, "beta\n").unwrap();

        let tool = ReadTool::new(None, None);
        let result = tool
            .execute(json!({
                "file_paths": [
                    first.to_str().unwrap(),
                    second.to_str().unwrap(),
                    first.to_str().unwrap()
                ]
            }))
            .await;

        assert!(!result.is_error, "unexpected error: {}", result.content);
        assert_eq!(result.content.matches("===== FILE ").count(), 2);
        assert!(
            result.content.find(first.to_str().unwrap())
                < result.content.find(second.to_str().unwrap())
        );
    }

    #[tokio::test]
    async fn batch_reports_missing_member_without_discarding_successes() {
        let dir = tempdir().unwrap();
        let present = dir.path().join("present.txt");
        let missing = dir.path().join("missing.txt");
        std::fs::write(&present, "kept result\n").unwrap();

        let cache = make_cache();
        let tool = ReadTool::new(Some(cache.clone()), None);
        let result = tool
            .execute(json!({
                "file_paths": [present.to_str().unwrap(), missing.to_str().unwrap()]
            }))
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("kept result"));
        assert!(result.content.contains("Failed to read file"));

        let mut cache = cache.write().unwrap();
        assert!(cache.get(&present).is_some());
        assert!(cache.get(&missing).is_none());
    }

    #[test]
    fn batch_caps_aggregate_image_attachment_payload() {
        let encoded_single_image_budget = MAX_IMAGE_BYTES.div_ceil(3) * 4;
        let entries = vec![
            (
                "first.png".to_string(),
                ToolResult {
                    content: "first image".to_string(),
                    is_error: false,
                    images: vec![ToolImage {
                        media_type: "image/png".to_string(),
                        data: "a".repeat(encoded_single_image_budget),
                    }],
                },
            ),
            (
                "second.png".to_string(),
                ToolResult {
                    content: "second image".to_string(),
                    is_error: false,
                    images: vec![ToolImage {
                        media_type: "image/png".to_string(),
                        data: "b".to_string(),
                    }],
                },
            ),
        ];

        let paths = entries
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let mut entries = entries.into_iter();
        let result = ReadTool::render_batch_with(&paths, |_, _| {
            entries.next().expect("one result per path").1
        });

        assert_eq!(result.images.len(), 1);
        assert!(result.content.contains("attachment omitted"));
    }

    #[tokio::test]
    async fn batch_execution_caps_image_count_before_base64_allocation() {
        let dir = tempdir().unwrap();
        let paths = (0..=MAX_BATCH_IMAGES)
            .map(|index| {
                let path = dir.path().join(format!("tiny-{index}.png"));
                std::fs::write(&path, [index as u8]).unwrap();
                path.to_string_lossy().into_owned()
            })
            .collect::<Vec<_>>();
        let tool = ReadTool::new(None, None);

        let result = tool.execute(json!({ "file_paths": paths })).await;

        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.images.len(), MAX_BATCH_IMAGES);
        assert!(result.content.contains("omitted before base64 encoding"));
    }
}
