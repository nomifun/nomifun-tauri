use std::path::Path;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::{Value, json};

use nomi_protocol::events::ToolCategory;
use nomi_types::tool::{JsonSchema, ToolResult};

use crate::Tool;
use crate::file_cache::{FileStateCache, update_cache_after_write};

pub struct WriteTool {
    file_cache: Option<Arc<RwLock<FileStateCache>>>,
    /// Optional containment root; when set, writes outside it are rejected.
    write_root: Option<std::path::PathBuf>,
    /// Session working directory used to resolve relative `file_path` inputs
    /// (matching ReadTool / Grep / Glob / Bash). `None` leaves relative paths
    /// resolving against the process cwd (legacy behavior).
    cwd: Option<std::path::PathBuf>,
}

impl WriteTool {
    /// Create a WriteTool with optional file state cache.
    ///
    /// When cache is `Some`, the tool updates the cache after each successful
    /// write so that subsequent Edit/Read calls see the latest content and mtime.
    ///
    /// No "must Read first" guard: Write is intended for creating new files
    /// or complete rewrites.
    ///
    /// Pass `None` to disable cache integration (legacy behavior).
    pub fn new(file_cache: Option<Arc<RwLock<FileStateCache>>>) -> Self {
        Self {
            file_cache,
            write_root: None,
            cwd: None,
        }
    }

    /// Restrict writes to within `root` (design §3.6 write-root containment).
    pub fn with_write_root(mut self, root: Option<std::path::PathBuf>) -> Self {
        self.write_root = root;
        self
    }

    /// Resolve relative `file_path` inputs against `cwd` (the session working
    /// directory), matching ReadTool/Grep/Glob/Bash. Without this, a relative
    /// path written by the model lands against the process cwd rather than the
    /// conversation's workspace.
    pub fn with_cwd(mut self, cwd: Option<std::path::PathBuf>) -> Self {
        self.cwd = cwd;
        self
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Writes content to a file, creating parent directories if needed.\n\n\
         Usage:\n\
         - This tool overwrites the existing file completely (not append).\n\
         - If the file already exists, you must use Read first to see its current content.\n\
         - Prefer Edit over Write for modifying existing files — Edit only sends the diff.\n\
         - Use Write only for creating new files or complete rewrites."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file to write (absolute preferred; a relative path resolves against the session working directory)"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(file_path) = input["file_path"].as_str() else {
            return ToolResult {
                content: "Missing required parameter: file_path".to_string(),
                is_error: true,
                images: Vec::new(),
            };
        };
        let Some(content) = input["content"].as_str() else {
            return ToolResult {
                content: "Missing required parameter: content".to_string(),
                is_error: true,
                images: Vec::new(),
            };
        };

        // Resolve a relative file_path against the session working directory
        // (matching ReadTool/Grep/Glob/Bash) before any filesystem use — so a
        // relative write lands in the conversation workspace, not the process cwd.
        let resolved = crate::path_guard::resolve_against_cwd(file_path, self.cwd.as_deref());
        let file_path = resolved.as_str();

        let path = Path::new(file_path);
        let existed = path.exists();

        // Write-root containment (opt-in): reject writes outside the configured
        // root before touching the filesystem.
        if let Some(msg) = crate::path_guard::ensure_within_root(file_path, self.write_root.as_deref()) {
            return ToolResult {
                content: msg,
                is_error: true,
                images: Vec::new(),
            };
        }

        // Enforce "must Read first" for files that already exist: overwriting a
        // file the model never read silently clobbers content it cannot see.
        // New files are exempt (Write's purpose is creation). Only enforced when
        // a file cache is wired; None disables it, preserving legacy behavior.
        if existed
            && let Some(cache_arc) = &self.file_cache
            && let Ok(mut cache) = cache_arc.write()
            && cache.get(path).is_none()
        {
                return ToolResult {
                    content: format!(
                        "You must Read {} before overwriting it — it already exists. \
                         Use the Read tool first, or use Edit for a targeted change.",
                        file_path
                    ),
                    is_error: true,
                    images: Vec::new(),
                };
        }

        // Create parent directories
        if let Some(parent) = path.parent().filter(|p| !p.exists()) {
            match std::fs::create_dir_all(parent) {
                Ok(()) => {}
                Err(e) => {
                    return ToolResult {
                        content: format!("Failed to create directories: {}", e),
                        is_error: true,
                        images: Vec::new(),
                    };
                }
            }
        }

        // Write atomically: write to temp file, then rename
        let tmp_path = format!("{}.tmp.{}", file_path, std::process::id());
        if let Err(e) = std::fs::write(&tmp_path, content) {
            return ToolResult {
                content: format!("Failed to write file: {}", e),
                is_error: true,
                images: Vec::new(),
            };
        }

        if let Err(e) = std::fs::rename(&tmp_path, file_path) {
            // Fallback: direct write if rename fails (cross-device)
            let _ = std::fs::remove_file(&tmp_path);
            if let Err(e) = std::fs::write(file_path, content) {
                return ToolResult {
                    content: format!("Failed to write file: {}", e),
                    is_error: true,
                    images: Vec::new(),
                };
            }
            if let Some(cache_arc) = &self.file_cache {
                update_cache_after_write(cache_arc, path, content);
            }

            return ToolResult {
                content: format!(
                    "Updated {} (rename failed: {}, used direct write)",
                    file_path, e
                ),
                is_error: false,
                images: Vec::new(),
            };
        }

        if let Some(cache_arc) = &self.file_cache {
            update_cache_after_write(cache_arc, path, content);
        }

        let line_count = content.lines().count();
        let action = if existed { "Updated" } else { "Created" };
        ToolResult {
            content: format!("{} {} ({} lines)", action, file_path, line_count),
            is_error: false,
            images: Vec::new(),
        }
    }

    fn max_result_size(&self) -> usize {
        10_000
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Edit
    }

    fn describe(&self, input: &Value) -> String {
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        format!("Write to {}", path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    use crate::Tool;
    use crate::file_cache::file_mtime_ms;
    use nomi_config::file_cache::FileCacheConfig;

    fn make_cache() -> Arc<RwLock<FileStateCache>> {
        let config = FileCacheConfig {
            max_entries: 100,
            max_size_bytes: 25 * 1024 * 1024,
            enabled: true,
        };
        Arc::new(RwLock::new(FileStateCache::new(&config)))
    }

    // -- Legacy tests (no cache) --

    #[tokio::test]
    async fn test_write_new_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("hello.txt");

        let input = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "hello world"
        });

        let tool = WriteTool::new(None);
        let result = tool.execute(input).await;

        assert!(
            !result.is_error,
            "expected success, got: {}",
            result.content
        );
        assert!(file_path.exists(), "file should exist after write");
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_write_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("subdir/nested/file.txt");

        let input = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "nested content"
        });

        let tool = WriteTool::new(None);
        let result = tool.execute(input).await;

        assert!(
            !result.is_error,
            "expected success, got: {}",
            result.content
        );
        assert!(
            file_path.parent().unwrap().exists(),
            "parent dirs should be created"
        );
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "nested content"
        );
    }

    #[tokio::test]
    async fn test_write_overwrite_existing() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("overwrite.txt");

        let tool = WriteTool::new(None);

        let input1 = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "original"
        });
        let result1 = tool.execute(input1).await;
        assert!(!result1.is_error);
        assert!(result1.content.contains("Created"));

        let input2 = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "replaced"
        });
        let result2 = tool.execute(input2).await;
        assert!(!result2.is_error);
        assert!(result2.content.contains("Updated"));

        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "replaced");
    }

    #[tokio::test]
    async fn test_write_file_content_matches() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("exact.txt");

        let content = "line 1\nline 2\nline 3\n";
        let input = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": content
        });

        let tool = WriteTool::new(None);
        let result = tool.execute(input).await;

        assert!(
            !result.is_error,
            "expected success, got: {}",
            result.content
        );

        let read_back = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(
            read_back, content,
            "read-back content must exactly match written content"
        );
    }

    // -- Cache integration tests --

    #[tokio::test]
    async fn write_populates_cache() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("cached.txt");

        let cache = make_cache();
        let tool = WriteTool::new(Some(cache.clone()));

        let input = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "cached content"
        });
        let result = tool.execute(input).await;
        assert!(!result.is_error, "write failed: {}", result.content);

        // Cache should have an entry with correct mtime.
        let disk_mtime = file_mtime_ms(&file_path).unwrap();
        let mut c = cache.write().unwrap();
        let cached = c
            .get(&file_path)
            .expect("file should be in cache after write");
        assert_eq!(cached.mtime_ms, disk_mtime);
        assert!(cached.content.contains("cached content"));
    }

    #[tokio::test]
    async fn write_then_edit_succeeds() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("write_edit.txt");

        let cache = make_cache();
        let write_tool = WriteTool::new(Some(cache.clone()));
        let edit_tool = crate::edit::EditTool::new(Some(cache));

        // Write creates the file and populates cache.
        let write_input = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "hello world"
        });
        let wr = write_tool.execute(write_input).await;
        assert!(!wr.is_error, "write failed: {}", wr.content);

        // Edit should succeed without needing a separate Read.
        let edit_input = json!({
            "file_path": file_path.to_str().unwrap(),
            "old_string": "hello",
            "new_string": "goodbye"
        });
        let er = edit_tool.execute(edit_input).await;
        assert!(!er.is_error, "edit after write failed: {}", er.content);
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "goodbye world"
        );
    }

    #[tokio::test]
    async fn write_rejects_overwriting_unread_existing_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("exists.txt");
        std::fs::write(&file_path, "original").unwrap();

        let cache = make_cache();
        let tool = WriteTool::new(Some(cache));
        let input = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "clobber"
        });
        let result = tool.execute(input).await;
        assert!(
            result.is_error,
            "overwriting an existing file that was never read must be rejected"
        );
        assert!(result.content.contains("Read"));
        // The file must be left untouched.
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "original");
    }

    #[tokio::test]
    async fn write_allows_overwriting_after_read() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("exists2.txt");
        std::fs::write(&file_path, "original").unwrap();

        let cache = make_cache();
        // Simulate a prior Read by populating the cache.
        update_cache_after_write(&cache, &file_path, "original");
        let tool = WriteTool::new(Some(cache));
        let input = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "replaced"
        });
        let result = tool.execute(input).await;
        assert!(
            !result.is_error,
            "overwrite after read should succeed: {}",
            result.content
        );
        assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "replaced");
    }

    #[tokio::test]
    async fn write_overwrite_updates_cache_mtime() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("overwrite_cache.txt");

        let cache = make_cache();
        let tool = WriteTool::new(Some(cache.clone()));

        // First write.
        let input1 = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "v1"
        });
        tool.execute(input1).await;

        let mtime1 = {
            let mut c = cache.write().unwrap();
            c.get(&file_path).unwrap().mtime_ms
        };

        // Brief delay to ensure mtime changes.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Second write.
        let input2 = json!({
            "file_path": file_path.to_str().unwrap(),
            "content": "v2"
        });
        tool.execute(input2).await;

        let mtime2 = {
            let mut c = cache.write().unwrap();
            c.get(&file_path).unwrap().mtime_ms
        };

        assert!(
            mtime2 >= mtime1,
            "cache mtime should update after overwrite"
        );
    }

    #[tokio::test]
    async fn write_resolves_relative_path_against_cwd() {
        // The conversation workspace.
        let workspace = tempdir().unwrap();
        let tool = WriteTool::new(None).with_cwd(Some(workspace.path().to_path_buf()));

        // A bare relative file name must land INSIDE the workspace, not against
        // the process cwd (the bug: companion chat reported success but the file
        // was written to the Tauri launch dir).
        let rel = "__nomi_reltest_write__.txt";
        let result = tool
            .execute(json!({ "file_path": rel, "content": "hello" }))
            .await;
        assert!(!result.is_error, "relative write should succeed: {}", result.content);

        let expected = workspace.path().join(rel);
        assert!(
            expected.exists(),
            "relative file_path must resolve against the injected cwd (workspace)"
        );
        assert_eq!(std::fs::read_to_string(&expected).unwrap(), "hello");
        // The success message must echo the resolved (workspace) path.
        assert!(
            result.content.contains(&workspace.path().to_string_lossy().into_owned()),
            "success message should report the resolved path, got: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn write_absolute_path_unaffected_by_cwd() {
        // With a cwd set, an ABSOLUTE path must be used verbatim (never joined).
        let workspace = tempdir().unwrap();
        let target = tempdir().unwrap();
        let abs = target.path().join("abs.txt");
        let tool = WriteTool::new(None).with_cwd(Some(workspace.path().to_path_buf()));

        let result = tool
            .execute(json!({ "file_path": abs.to_str().unwrap(), "content": "x" }))
            .await;
        assert!(!result.is_error, "absolute write should succeed: {}", result.content);
        assert!(abs.exists(), "absolute path must be written as-is");
        assert!(
            !workspace.path().join("abs.txt").exists(),
            "absolute path must NOT be joined onto the cwd"
        );
    }

    #[tokio::test]
    async fn write_root_rejects_outside_and_allows_inside() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let tool = WriteTool::new(None).with_write_root(Some(root.path().to_path_buf()));

        // Outside the root → rejected, nothing written.
        let escape = outside.path().join("escape.txt");
        let denied = tool
            .execute(json!({ "file_path": escape.to_str().unwrap(), "content": "x" }))
            .await;
        assert!(denied.is_error, "write outside root must be rejected");
        assert!(!escape.exists(), "rejected write must not touch disk");

        // Inside the root → allowed.
        let inside = root.path().join("ok.txt");
        let ok = tool
            .execute(json!({ "file_path": inside.to_str().unwrap(), "content": "y" }))
            .await;
        assert!(!ok.is_error, "write inside root must succeed: {}", ok.content);
        assert_eq!(std::fs::read_to_string(&inside).unwrap(), "y");
    }
}
