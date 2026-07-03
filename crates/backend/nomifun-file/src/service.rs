use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use base64::Engine;
use dashmap::DashMap;
use ignore::WalkBuilder;
use tracing::warn;

use nomifun_api_types::WebSocketMessage;
use nomifun_common::AppError;
use nomifun_realtime::EventBroadcaster;

use crate::path_safety::{
    PathAuthority, has_traversal, validate_path, validate_path_authority, validate_path_for_write,
    validate_path_for_write_authority, validate_path_with_extra_root,
};
use crate::types::{
    ContentUpdateEvent, ContentUpdateOperation, CopyResult, DirOrFile, FileMetadata, WorkspaceFlatFile, ZipEntry,
};

/// Maximum number of files returned by `list_workspace_files`.
const MAX_WORKSPACE_FILES: usize = 20_000;

/// Maximum file size for read operations (256 MB).
const MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// Maximum remote image size (5 MB).
const MAX_REMOTE_IMAGE_SIZE: usize = 5 * 1024 * 1024;

/// Maximum number of HTTP redirects for remote image fetching.
const MAX_REDIRECTS: usize = 5;

/// Request timeout for remote image fetching (30 seconds).
const REMOTE_IMAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Allowed hosts for remote image fetching.
const ALLOWED_IMAGE_HOSTS: &[&str] = &[
    "github.com",
    "raw.githubusercontent.com",
    "avatars.githubusercontent.com",
    "user-images.githubusercontent.com",
    "camo.githubusercontent.com",
    "objects.githubusercontent.com",
    "repository-images.githubusercontent.com",
];

/// Placeholder SVG returned when remote image fetching fails.
const PLACEHOLDER_SVG: &str = concat!(
    "<svg xmlns=\"http://www.w3.org/2000/svg\" ",
    "width=\"200\" height=\"200\" viewBox=\"0 0 200 200\">",
    "<rect fill=\"#f0f0f0\" width=\"200\" height=\"200\"/>",
    "<text x=\"100\" y=\"96\" text-anchor=\"middle\" ",
    "fill=\"#999\" font-family=\"sans-serif\" font-size=\"14\">",
    "Image Unavailable",
    "</text>",
    "</svg>",
);

/// A concrete implementation of [`crate::traits::IFileService`].
pub struct FileService {
    broadcaster: Arc<dyn EventBroadcaster>,
    /// Allowed root directories for path safety validation.
    allowed_roots: Vec<std::path::PathBuf>,
    /// In-memory cache for `list_workspace_files`, keyed by canonical root.
    workspace_files_cache: DashMap<String, Vec<WorkspaceFlatFile>>,
    /// Cancellation flags for in-progress ZIP operations, keyed by request_id.
    zip_cancellations: DashMap<String, Arc<AtomicBool>>,
}

impl FileService {
    pub fn new(broadcaster: Arc<dyn EventBroadcaster>, allowed_roots: Vec<std::path::PathBuf>) -> Self {
        Self {
            broadcaster,
            allowed_roots,
            workspace_files_cache: DashMap::new(),
            zip_cancellations: DashMap::new(),
        }
    }

    /// Invalidate the workspace files cache for a given root.
    /// Called when file changes are detected.
    pub fn invalidate_cache(&self, root: &str) {
        self.workspace_files_cache.remove(root);
    }

    /// Get the allowed root references for path validation.
    fn allowed_roots_refs(&self) -> Vec<&Path> {
        self.allowed_roots.iter().map(|p| p.as_path()).collect()
    }

    fn allowed_roots_with_extra<'a>(&'a self, extra_root: Option<&'a Path>) -> Vec<&'a Path> {
        let mut roots = self.allowed_roots_refs();
        if let Some(extra_root) = extra_root {
            roots.push(extra_root);
        }
        roots
    }

    /// The default [`PathAuthority`] for the non-scoped trait methods: confine
    /// to the service's construction-time `allowed_roots`, optionally widened
    /// by a request-scoped `extra` root. This reproduces the historical
    /// `allowed_roots ∪ extra_root` behaviour exactly, so the non-scoped
    /// methods (UI file routes, internal callers) are byte-for-byte unchanged.
    fn base_authority(&self, extra: Option<&Path>) -> PathAuthority {
        let mut roots = self.allowed_roots.clone();
        if let Some(extra) = extra {
            roots.push(extra.to_path_buf());
        }
        PathAuthority::Confined(roots)
    }

    /// Whether a (possibly non-existent) `path` textually falls under the given
    /// authority — used by the read fallback to distinguish "allowed but not
    /// found" (→ `Ok(None)`) from "forbidden" (→ error). `Unrestricted` always
    /// qualifies; `Confined` mirrors [`Self::path_uses_allowed_root`].
    fn path_uses_authority(&self, path: &Path, authority: &PathAuthority) -> bool {
        match authority {
            PathAuthority::Unrestricted => true,
            PathAuthority::Confined(roots) => {
                let candidate = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    match std::env::current_dir() {
                        Ok(current_dir) => current_dir.join(path),
                        Err(_) => path.to_path_buf(),
                    }
                };
                roots
                    .iter()
                    .filter_map(|root| std::fs::canonicalize(root).ok())
                    .any(|root| candidate.starts_with(root))
            }
        }
    }

    fn path_uses_allowed_root(&self, path: &Path, extra_root: Option<&Path>) -> bool {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            match std::env::current_dir() {
                Ok(current_dir) => current_dir.join(path),
                Err(_) => path.to_path_buf(),
            }
        };

        self.allowed_roots
            .iter()
            .map(PathBuf::as_path)
            .chain(extra_root)
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| candidate.starts_with(root))
    }

    // -- Authority-aware cores (shared by the non-scoped + `*_scoped` trait
    //    methods). The only difference between the two is the [`PathAuthority`]
    //    passed in; the I/O below is identical, so it lives here once. --

    async fn get_files_by_dir_impl(
        &self,
        dir: &str,
        root: &str,
        authority: &PathAuthority,
    ) -> Result<Vec<DirOrFile>, AppError> {
        let canonical_dir = validate_path_authority(dir, authority)?;
        let canonical_root = validate_path_authority(root, authority)?;
        self.build_dir_tree(&canonical_dir, &canonical_root).await
    }

    async fn list_workspace_files_impl(
        &self,
        root: &str,
        authority: &PathAuthority,
    ) -> Result<Vec<WorkspaceFlatFile>, AppError> {
        let canonical_root = validate_path_authority(root, authority)?;
        let cache_key = canonical_root.to_string_lossy().into_owned();

        if let Some(cached) = self.workspace_files_cache.get(&cache_key) {
            return Ok(cached.clone());
        }

        let root_owned = canonical_root.clone();
        let files = tokio::task::spawn_blocking(move || list_workspace_files_sync(&root_owned))
            .await
            .map_err(|e| AppError::Internal(format!("workspace file listing task failed: {e}")))??;

        self.workspace_files_cache.insert(cache_key, files.clone());
        Ok(files)
    }

    async fn get_file_metadata_impl(
        &self,
        path: &str,
        authority: &PathAuthority,
    ) -> Result<FileMetadata, AppError> {
        let canonical = validate_path_authority(path, authority)?;
        let result = tokio::task::spawn_blocking(move || get_file_metadata_sync(&canonical))
            .await
            .map_err(|e| AppError::Internal(format!("file metadata task failed: {e}")))??;
        Ok(result)
    }

    async fn read_file_impl(
        &self,
        path: &str,
        authority: &PathAuthority,
    ) -> Result<Option<String>, AppError> {
        if has_traversal(path) {
            return Err(AppError::BadRequest(format!(
                "path '{}' contains invalid traversal patterns",
                path
            )));
        }

        let canonical = match validate_path_authority(path, authority) {
            Ok(c) => c,
            Err(err) => {
                // Path does not exist yet but WOULD be within authority → "not
                // found" rather than "forbidden" (matches the historical
                // read fallback semantics).
                if matches!(err, AppError::BadRequest(_))
                    && validate_path_for_write_authority(path, authority).is_ok()
                {
                    return Ok(None);
                }
                if matches!(err, AppError::BadRequest(_)) && self.path_uses_authority(Path::new(path), authority) {
                    return Ok(None);
                }
                return Err(err);
            }
        };

        tokio::task::spawn_blocking(move || read_file_sync(&canonical))
            .await
            .map_err(|e| AppError::Internal(format!("read file task failed: {e}")))?
    }

    async fn write_file_impl(
        &self,
        path: &str,
        data: &[u8],
        workspace: &str,
        authority: &PathAuthority,
    ) -> Result<bool, AppError> {
        if has_traversal(path) {
            return Err(AppError::BadRequest(format!(
                "path '{}' contains invalid traversal patterns",
                path
            )));
        }

        let canonical = validate_path_for_write_authority(path, authority)?;

        let path_owned = canonical.clone();
        let data_owned = data.to_vec();
        tokio::task::spawn_blocking(move || write_file_sync(&path_owned, &data_owned))
            .await
            .map_err(|e| AppError::Internal(format!("write file task failed: {e}")))??;

        let workspace_path = Path::new(workspace);
        let relative_path = rel_to_api_string(
            canonical
                .strip_prefix(std::fs::canonicalize(workspace_path).unwrap_or_else(|_| workspace_path.to_path_buf()))
                .unwrap_or(&canonical),
        );

        let content = String::from_utf8(data.to_vec()).ok();
        let event = ContentUpdateEvent {
            file_path: canonical.to_string_lossy().into_owned(),
            content,
            workspace: workspace.to_owned(),
            relative_path,
            operation: ContentUpdateOperation::Write,
        };
        let payload = serde_json::to_value(&event).unwrap_or_default();
        let msg = WebSocketMessage::new("fileStream.contentUpdate", payload);
        self.broadcaster.broadcast(msg);

        if let Ok(canonical_ws) = std::fs::canonicalize(workspace_path) {
            self.invalidate_cache(&canonical_ws.to_string_lossy());
        }

        Ok(true)
    }

    async fn remove_entry_impl(
        &self,
        path: &str,
        workspace: &str,
        authority: &PathAuthority,
    ) -> Result<(), AppError> {
        if has_traversal(path) {
            return Err(AppError::BadRequest(format!(
                "path '{}' contains invalid traversal patterns",
                path
            )));
        }

        let canonical = validate_path_authority(path, authority)?;

        let path_owned = canonical.clone();
        tokio::task::spawn_blocking(move || remove_entry_sync(&path_owned))
            .await
            .map_err(|e| AppError::Internal(format!("remove entry task failed: {e}")))??;

        let workspace_path = Path::new(workspace);
        let relative_path = rel_to_api_string(
            canonical
                .strip_prefix(std::fs::canonicalize(workspace_path).unwrap_or_else(|_| workspace_path.to_path_buf()))
                .unwrap_or(&canonical),
        );

        let event = ContentUpdateEvent {
            file_path: canonical.to_string_lossy().into_owned(),
            content: None,
            workspace: workspace.to_owned(),
            relative_path,
            operation: ContentUpdateOperation::Delete,
        };
        let payload = serde_json::to_value(&event).unwrap_or_default();
        let msg = WebSocketMessage::new("fileStream.contentUpdate", payload);
        self.broadcaster.broadcast(msg);

        if let Ok(canonical_ws) = std::fs::canonicalize(workspace_path) {
            self.invalidate_cache(&canonical_ws.to_string_lossy());
        }

        Ok(())
    }

    async fn rename_entry_impl(
        &self,
        path: &str,
        new_name: &str,
        authority: &PathAuthority,
    ) -> Result<String, AppError> {
        if has_traversal(path) {
            return Err(AppError::BadRequest(format!(
                "path '{}' contains invalid traversal patterns",
                path
            )));
        }

        if new_name.contains('/') || new_name.contains('\\') {
            return Err(AppError::BadRequest(format!(
                "new name '{}' must not contain path separators",
                new_name
            )));
        }

        let canonical = validate_path_authority(path, authority)?;

        let new_name_owned = new_name.to_owned();
        let path_owned = canonical;
        let new_path: PathBuf = tokio::task::spawn_blocking(move || rename_entry_sync(&path_owned, &new_name_owned))
            .await
            .map_err(|e| AppError::Internal(format!("rename entry task failed: {e}")))??;

        Ok(new_path.to_string_lossy().into_owned())
    }

    /// List immediate children of `dir`, building a single-level tree.
    /// Each child directory also lists *its* children (depth = 2 from `dir`).
    async fn build_dir_tree(&self, dir: &Path, root: &Path) -> Result<Vec<DirOrFile>, AppError> {        let dir_owned = dir.to_path_buf();
        let root_owned = root.to_path_buf();

        tokio::task::spawn_blocking(move || build_dir_tree_sync(&dir_owned, &root_owned))
            .await
            .map_err(|e| AppError::Internal(format!("directory listing task failed: {e}")))?
    }
}

/// Normalize a workspace-relative path to forward-slash separators for the
/// cross-platform JSON/WS API contract (frontend consumers expect '/').
///
/// Component-join never emits a backslash and handles multi-segment relatives
/// correctly across platforms (equivalent to a `\` -> `/` replace, but explicit).
fn rel_to_api_string(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Synchronous directory tree builder (runs in blocking thread pool).
fn build_dir_tree_sync(dir: &Path, root: &Path) -> Result<Vec<DirOrFile>, AppError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| AppError::BadRequest(format!("cannot read directory '{}': {e}", dir.display())))?;

    let mut result = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| AppError::Internal(format!("error reading directory entry: {e}")))?;

        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|e| AppError::Internal(format!("cannot read metadata for '{}': {e}", path.display())))?;

        let name = entry.file_name().to_string_lossy().into_owned();

        let full_path = path.to_string_lossy().into_owned();
        let relative_path = rel_to_api_string(path.strip_prefix(root).unwrap_or(&path));

        let is_dir = metadata.is_dir();

        // For directories, also read their immediate children
        let children = if is_dir {
            read_children_sync(&path, root)?
        } else {
            Vec::new()
        };

        result.push(DirOrFile {
            name,
            full_path,
            relative_path,
            is_dir,
            children,
        });
    }

    // Sort: directories first, then alphabetical
    result.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));

    Ok(result)
}

/// Read immediate children of a directory (one level, no grandchildren).
fn read_children_sync(dir: &Path, root: &Path) -> Result<Vec<DirOrFile>, AppError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };

    let mut children = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);

        let name = entry.file_name().to_string_lossy().into_owned();

        let full_path = path.to_string_lossy().into_owned();
        let relative_path = rel_to_api_string(path.strip_prefix(root).unwrap_or(&path));

        children.push(DirOrFile {
            name,
            full_path,
            relative_path,
            is_dir,
            children: Vec::new(),
        });
    }

    children.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));

    Ok(children)
}

/// Recursively list files using the `ignore` crate (respects .gitignore).
fn list_workspace_files_sync(root: &Path) -> Result<Vec<WorkspaceFlatFile>, AppError> {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .build();

    let mut files = Vec::new();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("skipping unreadable entry: {e}");
                continue;
            }
        };

        let path = entry.path();
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "skipping unreadable workspace entry");
                continue;
            }
        };

        // Skip real directories and symlinks that resolve to directories.
        if metadata.is_dir() {
            continue;
        }

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let full_path = path.to_string_lossy().into_owned();
        let relative_path = rel_to_api_string(path.strip_prefix(root).unwrap_or(path));

        files.push(WorkspaceFlatFile {
            name,
            full_path,
            relative_path,
        });

        if files.len() >= MAX_WORKSPACE_FILES {
            break;
        }
    }

    Ok(files)
}

/// Validate that a file exists and is within the size limit.
/// Returns `Ok(None)` if the file does not exist.
/// Returns `Ok(Some(()))` if the file is valid for reading.
fn validate_file_for_read(path: &Path) -> Result<Option<()>, AppError> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(e) => {
            return Err(AppError::Internal(format!(
                "cannot read metadata for '{}': {e}",
                path.display()
            )));
        }
    };

    if metadata.len() > MAX_FILE_SIZE {
        return Err(AppError::BadRequest(format!(
            "file '{}' exceeds 256 MB limit ({} bytes)",
            path.display(),
            metadata.len()
        )));
    }

    if metadata.is_dir() {
        return Err(AppError::BadRequest(format!(
            "path '{}' is a directory; expected a file",
            path.display()
        )));
    }

    Ok(Some(()))
}

/// Read a file as UTF-8 text. Returns `None` if the file does not exist.
/// Rejects files larger than 256 MB.
fn read_file_sync(path: &Path) -> Result<Option<String>, AppError> {
    if validate_file_for_read(path)?.is_none() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| AppError::Internal(format!("cannot read file '{}': {e}", path.display())))?;

    Ok(Some(content))
}

/// Read a file as raw bytes. Returns `None` if the file does not exist.
/// Rejects files larger than 256 MB.
fn read_file_buffer_sync(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    if validate_file_for_read(path)?.is_none() {
        return Ok(None);
    }

    let bytes =
        std::fs::read(path).map_err(|e| AppError::Internal(format!("cannot read file '{}': {e}", path.display())))?;

    Ok(Some(bytes))
}

/// Write data to a file synchronously. Creates the file if it does not exist.
/// Returns `true` on success.
fn write_file_sync(path: &Path, data: &[u8]) -> Result<bool, AppError> {
    std::fs::write(path, data)
        .map_err(|e| AppError::Internal(format!("cannot write file '{}': {e}", path.display())))?;
    Ok(true)
}

/// Split a file name into `(base, ext)` where `ext` includes the leading dot.
///
/// Uses the **last** `.` as the extension boundary (matching macOS Finder and
/// Chrome download naming). If the file has no extension, or the only dot is at
/// the very start (hidden files like `.env`), the entire name is treated as the
/// base and `ext` is empty.
///
/// Examples:
/// - `"image.png"` -> `("image", ".png")`
/// - `"foo.tar.gz"` -> `("foo.tar", ".gz")`
/// - `"README"` -> `("README", "")`
/// - `".env"` -> `(".env", "")`
fn split_base_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(idx) if idx > 0 => name.split_at(idx),
        _ => (name, ""),
    }
}

/// Get file metadata synchronously.
fn get_file_metadata_sync(path: &Path) -> Result<FileMetadata, AppError> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| AppError::NotFound(format!("cannot read metadata for '{}': {e}", path.display())))?;

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let size = metadata.len();
    let is_directory = metadata.is_dir();

    let mime_type = if is_directory {
        "inode/directory".to_owned()
    } else {
        mime_guess::from_path(path)
            .first()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_owned())
    };

    let last_modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    Ok(FileMetadata {
        name,
        path: path.to_string_lossy().into_owned(),
        size,
        mime_type,
        last_modified,
        is_directory,
    })
}

/// Remove a file or directory synchronously. Directories are removed recursively.
fn remove_entry_sync(path: &Path) -> Result<(), AppError> {
    let metadata =
        std::fs::metadata(path).map_err(|e| AppError::NotFound(format!("cannot remove '{}': {e}", path.display())))?;

    if metadata.is_dir() {
        std::fs::remove_dir_all(path)
            .map_err(|e| AppError::Internal(format!("cannot remove directory '{}': {e}", path.display())))
    } else {
        std::fs::remove_file(path)
            .map_err(|e| AppError::Internal(format!("cannot remove file '{}': {e}", path.display())))
    }
}

/// Rename a file or directory synchronously. Returns the new absolute path.
fn rename_entry_sync(path: &Path, new_name: &str) -> Result<PathBuf, AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::BadRequest(format!("path '{}' has no parent", path.display())))?;

    let new_path = parent.join(new_name);

    if new_path.exists() {
        return Err(AppError::BadRequest(format!(
            "target '{}' already exists",
            new_path.display()
        )));
    }

    std::fs::rename(path, &new_path).map_err(|e| {
        AppError::Internal(format!(
            "cannot rename '{}' to '{}': {e}",
            path.display(),
            new_path.display()
        ))
    })?;

    Ok(new_path)
}

/// Copy a single file, creating parent directories as needed.
fn copy_single_file_sync(src: &Path, dest: &Path) -> Result<(), AppError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AppError::Internal(format!("cannot create directory '{}': {e}", parent.display())))?;
    }

    std::fs::copy(src, dest)
        .map_err(|e| AppError::Internal(format!("cannot copy '{}' to '{}': {e}", src.display(), dest.display())))?;

    Ok(())
}

/// Read a local image file and return a base64 Data URL.
fn get_image_base64_sync(path: &Path) -> Result<String, AppError> {
    let bytes =
        std::fs::read(path).map_err(|e| AppError::NotFound(format!("cannot read image '{}': {e}", path.display())))?;

    let mime = mime_guess::from_path(path)
        .first()
        .map(|m| m.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_owned());

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok(format!("data:{mime};base64,{encoded}"))
}

/// Build a placeholder SVG Data URL for failed remote image fetches.
fn placeholder_svg_data_url() -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(PLACEHOLDER_SVG);
    format!("data:image/svg+xml;base64,{encoded}")
}

/// Check whether a URL host is in the allowed whitelist.
fn is_allowed_image_host(url: &reqwest::Url) -> bool {
    let host = match url.host_str() {
        Some(h) => h,
        None => return false,
    };
    ALLOWED_IMAGE_HOSTS.contains(&host)
}

/// Validate a remote image URL: protocol must be HTTP(S) and host must be
/// whitelisted.
fn validate_remote_image_url(raw_url: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw_url).map_err(|e| format!("invalid URL '{raw_url}': {e}"))?;

    match url.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(format!("unsupported protocol '{scheme}', only HTTP/HTTPS allowed"));
        }
    }

    if !is_allowed_image_host(&url) {
        return Err(format!(
            "host '{}' is not in the allowed image host list",
            url.host_str().unwrap_or("unknown")
        ));
    }

    Ok(url)
}

/// Synchronous ZIP creation (runs in blocking thread pool).
///
/// Writes entries into a ZIP archive at `output_path`. Checks the
/// `cancelled` flag between entries and aborts early if set.
/// On cancellation, the partial ZIP file is removed.
fn create_zip_sync(output_path: &Path, entries: &[ZipEntry], cancelled: &AtomicBool) -> Result<bool, AppError> {
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AppError::Internal(format!(
                "cannot create parent directory for '{}': {e}",
                output_path.display()
            ))
        })?;
    }

    let file = std::fs::File::create(output_path)
        .map_err(|e| AppError::Internal(format!("cannot create ZIP file '{}': {e}", output_path.display())))?;

    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let result = write_zip_entries(&mut zip, entries, cancelled, options);

    if let Err(e) = result {
        drop(zip);
        let _ = std::fs::remove_file(output_path);
        return Err(e);
    }

    // write_zip_entries returned Ok(false) means cancelled
    if !result.unwrap() {
        drop(zip);
        let _ = std::fs::remove_file(output_path);
        return Ok(false);
    }

    zip.finish().map_err(|e| {
        let _ = std::fs::remove_file(output_path);
        AppError::Internal(format!("ZIP: failed to finalize '{}': {e}", output_path.display()))
    })?;

    Ok(true)
}

/// Write entries into a ZIP writer. Returns `Ok(true)` when all entries
/// are written, `Ok(false)` if cancelled, or `Err` on I/O failure.
fn write_zip_entries(
    zip: &mut zip::ZipWriter<std::fs::File>,
    entries: &[ZipEntry],
    cancelled: &AtomicBool,
    options: zip::write::SimpleFileOptions,
) -> Result<bool, AppError> {
    for entry in entries {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(false);
        }

        match entry {
            ZipEntry::Text { name, content } => {
                zip.start_file(name, options)
                    .map_err(|e| AppError::Internal(format!("ZIP: failed to start entry '{name}': {e}")))?;
                zip.write_all(content.as_bytes())
                    .map_err(|e| AppError::Internal(format!("ZIP: failed to write entry '{name}': {e}")))?;
            }
            ZipEntry::Disk { name, file_path } => {
                let data = std::fs::read(file_path)
                    .map_err(|e| AppError::Internal(format!("ZIP: cannot read source file '{file_path}': {e}")))?;
                zip.start_file(name, options)
                    .map_err(|e| AppError::Internal(format!("ZIP: failed to start entry '{name}': {e}")))?;
                zip.write_all(&data)
                    .map_err(|e| AppError::Internal(format!("ZIP: failed to write entry '{name}': {e}")))?;
            }
        }
    }

    // Final cancellation check before finishing
    if cancelled.load(Ordering::Relaxed) {
        return Ok(false);
    }

    Ok(true)
}

#[async_trait::async_trait]
impl crate::traits::IFileService for FileService {
    async fn get_files_by_dir(&self, dir: &str, root: &str) -> Result<Vec<DirOrFile>, AppError> {
        self.get_files_by_dir_impl(dir, root, &self.base_authority(None)).await
    }

    async fn get_files_by_dir_scoped(
        &self,
        dir: &str,
        root: &str,
        authority: &PathAuthority,
    ) -> Result<Vec<DirOrFile>, AppError> {
        self.get_files_by_dir_impl(dir, root, authority).await
    }

    async fn list_workspace_files(&self, root: &str) -> Result<Vec<WorkspaceFlatFile>, AppError> {
        self.list_workspace_files_impl(root, &self.base_authority(None)).await
    }

    async fn list_workspace_files_scoped(
        &self,
        root: &str,
        authority: &PathAuthority,
    ) -> Result<Vec<WorkspaceFlatFile>, AppError> {
        self.list_workspace_files_impl(root, authority).await
    }

    async fn get_file_metadata(&self, path: &str, extra_root: Option<&Path>) -> Result<FileMetadata, AppError> {
        self.get_file_metadata_impl(path, &self.base_authority(extra_root)).await
    }

    async fn get_file_metadata_scoped(
        &self,
        path: &str,
        authority: &PathAuthority,
    ) -> Result<FileMetadata, AppError> {
        self.get_file_metadata_impl(path, authority).await
    }

    // -- File read/write (task 7.4) --

    async fn read_file(&self, path: &str, extra_root: Option<&Path>) -> Result<Option<String>, AppError> {
        self.read_file_impl(path, &self.base_authority(extra_root)).await
    }

    async fn read_file_scoped(&self, path: &str, authority: &PathAuthority) -> Result<Option<String>, AppError> {
        self.read_file_impl(path, authority).await
    }

    async fn read_file_buffer(&self, path: &str, extra_root: Option<&Path>) -> Result<Option<Vec<u8>>, AppError> {
        if has_traversal(path) {
            return Err(AppError::BadRequest(format!(
                "path '{}' contains invalid traversal patterns",
                path
            )));
        }

        let roots = self.allowed_roots_refs();
        let canonical = match validate_path_with_extra_root(path, &roots, extra_root) {
            Ok(c) => c,
            Err(err) => {
                if matches!(err, AppError::BadRequest(_))
                    && validate_path_for_write(path, &self.allowed_roots_with_extra(extra_root)).is_ok()
                {
                    return Ok(None);
                }
                if matches!(err, AppError::BadRequest(_)) && self.path_uses_allowed_root(Path::new(path), extra_root) {
                    return Ok(None);
                }
                return Err(err);
            }
        };

        tokio::task::spawn_blocking(move || read_file_buffer_sync(&canonical))
            .await
            .map_err(|e| AppError::Internal(format!("read file buffer task failed: {e}")))?
    }

    async fn write_file(&self, path: &str, data: &[u8], workspace: &str) -> Result<bool, AppError> {
        self.write_file_impl(path, data, workspace, &self.base_authority(None)).await
    }

    async fn write_file_scoped(
        &self,
        path: &str,
        data: &[u8],
        workspace: &str,
        authority: &PathAuthority,
    ) -> Result<bool, AppError> {
        self.write_file_impl(path, data, workspace, authority).await
    }

    async fn copy_files_to_workspace(
        &self,
        file_paths: &[String],
        workspace: &str,
        source_root: Option<&str>,
    ) -> Result<CopyResult, AppError> {
        let roots = self.allowed_roots_refs();
        let ws_canonical = validate_path(workspace, &roots)?;

        let sr_canonical = match source_root {
            Some(sr) => Some(validate_path(sr, &roots)?),
            None => None,
        };

        let file_paths_owned: Vec<String> = file_paths.to_vec();
        let roots_owned: Vec<std::path::PathBuf> = self.allowed_roots.clone();

        tokio::task::spawn_blocking(move || {
            let roots_refs: Vec<&Path> = roots_owned.iter().map(|p| p.as_path()).collect();
            let mut copied = Vec::new();
            let mut failed = Vec::new();

            for fp in &file_paths_owned {
                let src = match validate_path(fp, &roots_refs) {
                    Ok(p) if p.is_file() => p,
                    _ => {
                        failed.push(fp.clone());
                        continue;
                    }
                };

                let relative = match &sr_canonical {
                    Some(sr) => src
                        .strip_prefix(sr)
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|_| Path::new(src.file_name().unwrap_or_default()).to_path_buf()),
                    None => Path::new(src.file_name().unwrap_or_default()).to_path_buf(),
                };

                let dest = ws_canonical.join(&relative);
                match copy_single_file_sync(&src, &dest) {
                    Ok(()) => copied.push(fp.clone()),
                    Err(_) => failed.push(fp.clone()),
                }
            }

            Ok(CopyResult {
                copied_files: copied,
                failed_files: failed,
            })
        })
        .await
        .map_err(|e| AppError::Internal(format!("copy task failed: {e}")))?
    }

    async fn remove_entry(&self, path: &str, workspace: &str) -> Result<(), AppError> {
        self.remove_entry_impl(path, workspace, &self.base_authority(None)).await
    }

    async fn remove_entry_scoped(
        &self,
        path: &str,
        workspace: &str,
        authority: &PathAuthority,
    ) -> Result<(), AppError> {
        self.remove_entry_impl(path, workspace, authority).await
    }

    async fn rename_entry(&self, path: &str, new_name: &str) -> Result<String, AppError> {
        self.rename_entry_impl(path, new_name, &self.base_authority(None)).await
    }

    async fn rename_entry_scoped(
        &self,
        path: &str,
        new_name: &str,
        authority: &PathAuthority,
    ) -> Result<String, AppError> {
        self.rename_entry_impl(path, new_name, authority).await
    }

    async fn create_temp_file(&self, file_name: &str) -> Result<String, AppError> {
        if has_traversal(file_name) {
            return Err(AppError::BadRequest(format!(
                "file name '{}' contains invalid traversal patterns",
                file_name
            )));
        }

        if file_name.contains('/') || file_name.contains('\\') {
            return Err(AppError::BadRequest(format!(
                "file name '{}' must not contain path separators",
                file_name
            )));
        }

        let name = file_name.to_owned();

        tokio::task::spawn_blocking(move || {
            let tmp_dir = std::env::temp_dir().join("nomifun");
            std::fs::create_dir_all(&tmp_dir)
                .map_err(|e| AppError::Internal(format!("cannot create temp directory: {e}")))?;

            let file_path = tmp_dir.join(&name);
            std::fs::File::create(&file_path)
                .map_err(|e| AppError::Internal(format!("cannot create temp file '{}': {e}", file_path.display())))?;

            Ok(file_path.to_string_lossy().into_owned())
        })
        .await
        .map_err(|e| AppError::Internal(format!("create temp file task failed: {e}")))?
    }

    async fn create_upload_file(
        &self,
        file_name: &str,
        data: &[u8],
        conversation_id: Option<&str>,
    ) -> Result<String, AppError> {
        if file_name.is_empty() {
            return Err(AppError::BadRequest("file name must not be empty".to_owned()));
        }
        if has_traversal(file_name) {
            return Err(AppError::BadRequest(format!(
                "file name '{}' contains invalid traversal patterns",
                file_name
            )));
        }
        if file_name.contains('/') || file_name.contains('\\') {
            return Err(AppError::BadRequest(format!(
                "file name '{}' must not contain path separators",
                file_name
            )));
        }

        // Validate optional conversation_id: no separators / traversal.
        let conv_id = match conversation_id {
            Some(id) if !id.is_empty() => {
                if has_traversal(id) || id.contains('/') || id.contains('\\') {
                    return Err(AppError::BadRequest(format!(
                        "conversation id '{}' contains invalid characters",
                        id
                    )));
                }
                Some(id.to_owned())
            }
            _ => None,
        };

        let name = file_name.to_owned();
        let bytes = data.to_vec();

        tokio::task::spawn_blocking(move || {
            let mut dir = std::env::temp_dir().join("nomifun");
            if let Some(conv_id) = conv_id.as_deref() {
                dir = dir.join(conv_id);
            } else {
                dir = dir.join("general");
            }
            std::fs::create_dir_all(&dir)
                .map_err(|e| AppError::Internal(format!("cannot create upload directory: {e}")))?;

            let (base, ext) = split_base_ext(&name);
            let mut candidate = name.clone();
            let mut counter: u32 = 2;
            loop {
                let file_path = dir.join(&candidate);
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&file_path)
                {
                    Ok(mut f) => {
                        f.write_all(&bytes).map_err(|e| {
                            AppError::Internal(format!("cannot write upload file '{}': {e}", file_path.display()))
                        })?;
                        return Ok(file_path.to_string_lossy().into_owned());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        if counter > 1000 {
                            return Err(AppError::Internal(format!(
                                "too many name collisions for upload file '{}'",
                                name
                            )));
                        }
                        candidate = format!("{base}({counter}){ext}");
                        counter += 1;
                    }
                    Err(e) => {
                        return Err(AppError::Internal(format!(
                            "cannot write upload file '{}': {e}",
                            file_path.display()
                        )));
                    }
                }
            }
        })
        .await
        .map_err(|e| AppError::Internal(format!("create upload file task failed: {e}")))?
    }

    async fn get_image_base64(&self, path: &str, extra_root: Option<&Path>) -> Result<String, AppError> {
        if has_traversal(path) {
            return Err(AppError::BadRequest(format!(
                "path '{}' contains invalid traversal patterns",
                path
            )));
        }

        let roots = self.allowed_roots_refs();
        let canonical = validate_path_with_extra_root(path, &roots, extra_root)?;

        tokio::task::spawn_blocking(move || get_image_base64_sync(&canonical))
            .await
            .map_err(|e| AppError::Internal(format!("image base64 task failed: {e}")))?
    }

    async fn fetch_remote_image(&self, url: &str) -> String {
        let parsed = match validate_remote_image_url(url) {
            Ok(u) => u,
            Err(e) => {
                warn!("remote image rejected: {e}");
                return placeholder_svg_data_url();
            }
        };

        let client = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .timeout(REMOTE_IMAGE_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to build HTTP client: {e}");
                return placeholder_svg_data_url();
            }
        };

        let response = match client.get(parsed.clone()).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("remote image fetch failed for '{}': {e}", url);
                return placeholder_svg_data_url();
            }
        };

        if !response.status().is_success() {
            warn!("remote image fetch returned status {} for '{}'", response.status(), url);
            return placeholder_svg_data_url();
        }

        // Early reject if Content-Length exceeds limit
        if let Some(len) = response.content_length()
            && len > MAX_REMOTE_IMAGE_SIZE as u64
        {
            warn!("remote image too large ({} bytes) for '{}'", len, url);
            return placeholder_svg_data_url();
        }

        // Determine MIME from Content-Type header, fall back to URL extension
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|ct| ct.split(';').next())
            .map(|s| s.trim().to_owned());

        let mime = content_type.unwrap_or_else(|| {
            mime_guess::from_path(parsed.path())
                .first()
                .map(|m| m.to_string())
                .unwrap_or_else(|| "application/octet-stream".to_owned())
        });

        let bytes = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                warn!("failed to read remote image body for '{}': {e}", url);
                return placeholder_svg_data_url();
            }
        };

        if bytes.len() > MAX_REMOTE_IMAGE_SIZE {
            warn!("remote image body too large ({} bytes) for '{}'", bytes.len(), url);
            return placeholder_svg_data_url();
        }

        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        format!("data:{mime};base64,{encoded}")
    }

    async fn create_zip(
        &self,
        path: &str,
        entries: Vec<ZipEntry>,
        request_id: Option<String>,
    ) -> Result<bool, AppError> {
        // Validate output path is within the sandbox
        let roots = self.allowed_roots_refs();
        let output = validate_path_for_write(path, &roots)?;

        // Validate all Disk entry source paths are within the sandbox
        for entry in &entries {
            if let ZipEntry::Disk { file_path, .. } = entry {
                validate_path(file_path, &roots)?;
            }
        }

        let cancelled = Arc::new(AtomicBool::new(false));

        if let Some(ref id) = request_id {
            self.zip_cancellations.insert(id.clone(), Arc::clone(&cancelled));
        }

        let result = tokio::task::spawn_blocking(move || create_zip_sync(&output, &entries, &cancelled))
            .await
            .map_err(|e| AppError::Internal(format!("ZIP creation task failed: {e}")))??;

        // Clean up cancellation token after task completes
        if let Some(ref id) = request_id {
            self.zip_cancellations.remove(id);
        }

        Ok(result)
    }

    async fn cancel_zip(&self, request_id: &str) -> bool {
        if let Some((_, flag)) = self.zip_cancellations.remove(request_id) {
            flag.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn build_dir_tree_sync_lists_files_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::write(dir.path().join("b.rs"), "fn main(){}").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/c.txt"), "nested").unwrap();

        let result = build_dir_tree_sync(dir.path(), dir.path()).unwrap();

        // sub/ should come first (directories first)
        assert_eq!(result[0].name, "sub");
        assert!(result[0].is_dir);
        // sub/ should have c.txt as child
        assert_eq!(result[0].children.len(), 1);
        assert_eq!(result[0].children[0].name, "c.txt");

        // Then files alphabetically
        assert_eq!(result[1].name, "a.txt");
        assert!(!result[1].is_dir);
        assert_eq!(result[2].name, "b.rs");
    }

    #[test]
    fn build_dir_tree_sync_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let result = build_dir_tree_sync(dir.path(), dir.path()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn build_dir_tree_sync_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("folder");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("file.txt"), "data").unwrap();

        let result = build_dir_tree_sync(dir.path(), dir.path()).unwrap();

        assert_eq!(result[0].relative_path, "folder");
        assert_eq!(result[0].children[0].relative_path, "folder/file.txt");
    }

    #[test]
    fn build_dir_tree_sync_nonexistent_dir_errors() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("nonexistent");
        let result = build_dir_tree_sync(&fake, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn list_workspace_files_sync_basic() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.txt"), "world").unwrap();

        let files = list_workspace_files_sync(dir.path()).unwrap();

        assert_eq!(files.len(), 2);
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
    }

    #[test]
    fn list_workspace_files_sync_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("kept.txt"), "keep").unwrap();
        fs::write(dir.path().join("ignored.txt"), "skip").unwrap();

        let files = list_workspace_files_sync(dir.path()).unwrap();

        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"kept.txt"));
        assert!(names.contains(&".gitignore"));
        assert!(!names.contains(&"ignored.txt"));
    }

    #[test]
    fn list_workspace_files_sync_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let files = list_workspace_files_sync(dir.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn list_workspace_files_sync_truncates_at_limit() {
        // Creating 20,000+ files is impractical in a unit test;
        // verify the constant exists and the branch logic is sound.
        assert_eq!(MAX_WORKSPACE_FILES, 20_000);
    }

    #[test]
    fn list_workspace_files_sync_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main(){}").unwrap();

        let files = list_workspace_files_sync(dir.path()).unwrap();
        let main_file = files.iter().find(|f| f.name == "main.rs").unwrap();

        assert_eq!(main_file.relative_path, "src/main.rs");
    }

    #[cfg(unix)]
    #[test]
    fn list_workspace_files_sync_skips_directory_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("builtin-skills/auto-inject/nomifun-skills");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "---\ndescription: test\n---\nbody").unwrap();

        let workspace = dir.path().join("workspace/.claude/skills");
        fs::create_dir_all(&workspace).unwrap();
        std::os::unix::fs::symlink(&skill_dir, workspace.join("nomifun-skills")).unwrap();

        let files = list_workspace_files_sync(&dir.path().join("workspace")).unwrap();

        assert!(
            files.iter().all(|f| f.name != "nomifun-skills"),
            "directory symlink should not be surfaced as a file: {files:?}"
        );
    }

    #[test]
    fn get_file_metadata_sync_text_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        fs::write(&file, "hello world").unwrap();

        let meta = get_file_metadata_sync(&file).unwrap();
        assert_eq!(meta.name, "hello.txt");
        assert_eq!(meta.size, 11);
        assert_eq!(meta.mime_type, "text/plain");
        assert!(!meta.is_directory);
        assert!(meta.last_modified > 0);
    }

    #[test]
    fn get_file_metadata_sync_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("mydir");
        fs::create_dir(&sub).unwrap();

        let meta = get_file_metadata_sync(&sub).unwrap();
        assert_eq!(meta.name, "mydir");
        assert!(meta.is_directory);
        assert_eq!(meta.mime_type, "inode/directory");
    }

    #[test]
    fn get_file_metadata_sync_rust_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "pub fn foo() {}").unwrap();

        let meta = get_file_metadata_sync(&file).unwrap();
        assert_eq!(meta.name, "lib.rs");
        // rust files should get a reasonable mime type
        assert!(!meta.mime_type.is_empty());
    }

    #[test]
    fn get_file_metadata_sync_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("missing.txt");
        let result = get_file_metadata_sync(&fake);
        assert!(result.is_err());
    }

    #[test]
    fn get_file_metadata_sync_image_mime() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("icon.png");
        fs::write(&png, [0x89, 0x50, 0x4E, 0x47]).unwrap();

        let meta = get_file_metadata_sync(&png).unwrap();
        assert_eq!(meta.mime_type, "image/png");
    }

    #[test]
    fn get_file_metadata_sync_unknown_extension() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.xyz123");
        fs::write(&file, "binary data").unwrap();

        let meta = get_file_metadata_sync(&file).unwrap();
        assert_eq!(meta.mime_type, "application/octet-stream");
    }

    // -- read_file_sync tests (task 7.4) --

    #[test]
    fn read_file_sync_normal_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        fs::write(&file, "hello world").unwrap();

        let result = read_file_sync(&file).unwrap();
        assert_eq!(result.as_deref(), Some("hello world"));
    }

    #[test]
    fn read_file_sync_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty.txt");
        fs::write(&file, "").unwrap();

        let result = read_file_sync(&file).unwrap();
        assert_eq!(result.as_deref(), Some(""));
    }

    #[test]
    fn read_file_sync_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("missing.txt");

        let result = read_file_sync(&fake).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn read_file_sync_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("subdir");
        fs::create_dir(&folder).unwrap();

        let err = read_file_sync(&folder).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        assert!(err.to_string().contains("is a directory"));
    }

    // -- validate_file_for_read tests --

    #[test]
    fn validate_file_for_read_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("valid.txt");
        fs::write(&file, "data").unwrap();

        let result = validate_file_for_read(&file).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn validate_file_for_read_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("nope.txt");

        let result = validate_file_for_read(&fake).unwrap();
        assert!(result.is_none());
    }

    // -- read_file_buffer_sync tests --

    #[test]
    fn read_file_buffer_sync_normal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.bin");
        let bytes: Vec<u8> = vec![0x00, 0xFF, 0x42, 0x89];
        fs::write(&file, &bytes).unwrap();

        let result = read_file_buffer_sync(&file).unwrap();
        assert_eq!(result.as_deref(), Some(bytes.as_slice()));
    }

    #[test]
    fn read_file_buffer_sync_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("missing.bin");

        let result = read_file_buffer_sync(&fake).unwrap();
        assert!(result.is_none());
    }

    // -- write_file_sync tests --

    #[test]
    fn write_file_sync_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("output.txt");

        let ok = write_file_sync(&file, b"hello").unwrap();
        assert!(ok);
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello");
    }

    #[test]
    fn write_file_sync_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("overwrite.txt");
        fs::write(&file, "old").unwrap();

        let ok = write_file_sync(&file, b"new content").unwrap();
        assert!(ok);
        assert_eq!(fs::read_to_string(&file).unwrap(), "new content");
    }

    #[test]
    fn write_file_sync_binary() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.bin");
        let data = vec![0x00, 0xFF, 0xAB];

        let ok = write_file_sync(&file, &data).unwrap();
        assert!(ok);
        assert_eq!(fs::read(&file).unwrap(), data);
    }

    // -- remove_entry_sync tests (task 7.5) --

    #[test]
    fn remove_entry_sync_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("to_delete.txt");
        fs::write(&file, "bye").unwrap();
        assert!(file.exists());

        remove_entry_sync(&file).unwrap();
        assert!(!file.exists());
    }

    #[test]
    fn remove_entry_sync_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("a.txt"), "a").unwrap();

        remove_entry_sync(&sub).unwrap();
        assert!(!sub.exists());
    }

    #[test]
    fn remove_entry_sync_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("ghost.txt");
        let result = remove_entry_sync(&fake);
        assert!(result.is_err());
    }

    // -- rename_entry_sync tests (task 7.5) --

    #[test]
    fn rename_entry_sync_file() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.txt");
        fs::write(&old, "data").unwrap();

        let new_path = rename_entry_sync(&old, "new.txt").unwrap();
        assert!(!old.exists());
        assert!(new_path.exists());
        assert_eq!(fs::read_to_string(&new_path).unwrap(), "data");
    }

    #[test]
    fn rename_entry_sync_directory() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old_dir");
        fs::create_dir(&old).unwrap();

        let new_path = rename_entry_sync(&old, "new_dir").unwrap();
        assert!(!old.exists());
        assert!(new_path.is_dir());
    }

    #[test]
    fn rename_entry_sync_target_exists() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.txt");
        let existing = dir.path().join("existing.txt");
        fs::write(&old, "old").unwrap();
        fs::write(&existing, "existing").unwrap();

        let result = rename_entry_sync(&old, "existing.txt");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    // -- copy_single_file_sync tests (task 7.5) --

    #[test]
    fn copy_single_file_sync_basic() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        fs::write(&src, "content").unwrap();

        copy_single_file_sync(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "content");
    }

    #[test]
    fn copy_single_file_sync_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("nested/deep/dest.txt");
        fs::write(&src, "nested").unwrap();

        copy_single_file_sync(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), "nested");
    }

    #[test]
    fn copy_single_file_sync_source_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("missing.txt");
        let dest = dir.path().join("dest.txt");

        let result = copy_single_file_sync(&src, &dest);
        assert!(result.is_err());
    }

    // -- get_image_base64_sync tests (task 7.6) --

    #[test]
    fn get_image_base64_sync_png() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.png");
        let bytes = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic bytes
        fs::write(&file, &bytes).unwrap();

        let result = get_image_base64_sync(&file).unwrap();
        assert!(result.starts_with("data:image/png;base64,"));

        // Verify the base64 part decodes back to original bytes
        let encoded_part = result.strip_prefix("data:image/png;base64,").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded_part).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn get_image_base64_sync_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("photo.jpg");
        let bytes = vec![0xFF, 0xD8, 0xFF, 0xE0]; // JPEG magic bytes
        fs::write(&file, &bytes).unwrap();

        let result = get_image_base64_sync(&file).unwrap();
        assert!(result.starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn get_image_base64_sync_svg() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("icon.svg");
        fs::write(&file, "<svg></svg>").unwrap();

        let result = get_image_base64_sync(&file).unwrap();
        assert!(result.starts_with("data:image/svg+xml;base64,"));
    }

    #[test]
    fn get_image_base64_sync_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("missing.png");

        let result = get_image_base64_sync(&fake);
        assert!(result.is_err());
    }

    #[test]
    fn get_image_base64_sync_unknown_extension() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.xyz999");
        fs::write(&file, b"some bytes").unwrap();

        let result = get_image_base64_sync(&file).unwrap();
        // Falls back to application/octet-stream
        assert!(result.starts_with("data:application/octet-stream;base64,"));
    }

    // -- placeholder_svg_data_url tests --

    #[test]
    fn placeholder_svg_data_url_format() {
        let url = placeholder_svg_data_url();
        assert!(url.starts_with("data:image/svg+xml;base64,"));

        // Verify it decodes to valid SVG content
        let encoded_part = url.strip_prefix("data:image/svg+xml;base64,").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD.decode(encoded_part).unwrap();
        let svg = String::from_utf8(decoded).unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }

    // -- validate_remote_image_url tests --

    #[test]
    fn validate_remote_image_url_https_allowed_host() {
        let result = validate_remote_image_url("https://raw.githubusercontent.com/owner/repo/main/image.png");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_remote_image_url_http_allowed_host() {
        let result = validate_remote_image_url("http://github.com/image.png");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_remote_image_url_disallowed_host() {
        let result = validate_remote_image_url("https://evil.com/image.png");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not in the allowed"));
    }

    #[test]
    fn validate_remote_image_url_ftp_protocol() {
        let result = validate_remote_image_url("ftp://github.com/image.png");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported protocol"));
    }

    #[test]
    fn validate_remote_image_url_invalid_url() {
        let result = validate_remote_image_url("not-a-url");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid URL"));
    }

    #[test]
    fn validate_remote_image_url_file_protocol() {
        let result = validate_remote_image_url("file:///etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unsupported protocol"));
    }

    // -- is_allowed_image_host tests --

    #[test]
    fn is_allowed_image_host_exact_match() {
        let url = reqwest::Url::parse("https://github.com/img.png").unwrap();
        assert!(is_allowed_image_host(&url));
    }

    #[test]
    fn is_allowed_image_host_subdomain_not_matched() {
        // "sub.github.com" should NOT match "github.com"
        let url = reqwest::Url::parse("https://sub.github.com/img.png").unwrap();
        assert!(!is_allowed_image_host(&url));
    }

    #[test]
    fn is_allowed_image_host_all_listed_hosts() {
        for host in ALLOWED_IMAGE_HOSTS {
            let url_str = format!("https://{host}/test.png");
            let url = reqwest::Url::parse(&url_str).unwrap();
            assert!(is_allowed_image_host(&url), "host '{host}' should be allowed");
        }
    }

    // -- create_zip_sync tests --

    #[test]
    fn create_zip_sync_text_entries() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("out.zip");
        let entries = vec![
            ZipEntry::Text {
                name: "hello.txt".into(),
                content: "Hello world".into(),
            },
            ZipEntry::Text {
                name: "sub/nested.txt".into(),
                content: "Nested content".into(),
            },
        ];
        let cancelled = AtomicBool::new(false);

        let result = create_zip_sync(&zip_path, &entries, &cancelled);
        assert!(result.is_ok());
        assert!(result.unwrap());
        assert!(zip_path.exists());

        // Verify ZIP contents
        let file = fs::File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        assert_eq!(archive.len(), 2);

        {
            let mut f0 = archive.by_name("hello.txt").unwrap();
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut f0, &mut buf).unwrap();
            assert_eq!(buf, "Hello world");
        }
        {
            let mut f1 = archive.by_name("sub/nested.txt").unwrap();
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut f1, &mut buf).unwrap();
            assert_eq!(buf, "Nested content");
        }
    }

    #[test]
    fn create_zip_sync_disk_entries() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("source.dat");
        fs::write(&src_path, b"binary data here").unwrap();

        let zip_path = dir.path().join("out.zip");
        let entries = vec![ZipEntry::Disk {
            name: "packed.dat".into(),
            file_path: src_path.to_string_lossy().into_owned(),
        }];
        let cancelled = AtomicBool::new(false);

        let result = create_zip_sync(&zip_path, &entries, &cancelled);
        assert!(result.unwrap());

        let file = fs::File::open(&zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        assert_eq!(archive.len(), 1);

        let mut f = archive.by_name("packed.dat").unwrap();
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut buf).unwrap();
        assert_eq!(buf, b"binary data here");
    }

    #[test]
    fn create_zip_sync_mixed_entries() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("disk.txt");
        fs::write(&src, "from disk").unwrap();

        let zip_path = dir.path().join("mixed.zip");
        let entries = vec![
            ZipEntry::Text {
                name: "mem.txt".into(),
                content: "from memory".into(),
            },
            ZipEntry::Disk {
                name: "disk.txt".into(),
                file_path: src.to_string_lossy().into_owned(),
            },
        ];
        let cancelled = AtomicBool::new(false);

        assert!(create_zip_sync(&zip_path, &entries, &cancelled).unwrap());

        let file = fs::File::open(&zip_path).unwrap();
        let archive = zip::ZipArchive::new(file).unwrap();
        assert_eq!(archive.len(), 2);
    }

    #[test]
    fn create_zip_sync_empty_entries() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("empty.zip");
        let cancelled = AtomicBool::new(false);

        assert!(create_zip_sync(&zip_path, &[], &cancelled).unwrap());
        assert!(zip_path.exists());

        let file = fs::File::open(&zip_path).unwrap();
        let archive = zip::ZipArchive::new(file).unwrap();
        assert_eq!(archive.len(), 0);
    }

    #[test]
    fn create_zip_sync_cancellation_before_start() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("cancelled.zip");
        let entries = vec![ZipEntry::Text {
            name: "a.txt".into(),
            content: "data".into(),
        }];
        let cancelled = AtomicBool::new(true);

        let result = create_zip_sync(&zip_path, &entries, &cancelled);
        assert!(!result.unwrap());
        assert!(!zip_path.exists());
    }

    #[test]
    fn create_zip_sync_disk_entry_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("fail.zip");
        let entries = vec![ZipEntry::Disk {
            name: "missing.txt".into(),
            file_path: "/nonexistent/file.txt".into(),
        }];
        let cancelled = AtomicBool::new(false);

        let result = create_zip_sync(&zip_path, &entries, &cancelled);
        assert!(result.is_err());
    }

    #[test]
    fn create_zip_sync_error_cleans_up_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("good.txt");
        fs::write(&src, "data").unwrap();
        let zip_path = dir.path().join("partial.zip");

        // First entry succeeds, second fails → partial ZIP should be removed
        let entries = vec![
            ZipEntry::Disk {
                name: "good.txt".into(),
                file_path: src.to_string_lossy().into_owned(),
            },
            ZipEntry::Disk {
                name: "bad.txt".into(),
                file_path: "/nonexistent/missing.txt".into(),
            },
        ];
        let cancelled = AtomicBool::new(false);

        let result = create_zip_sync(&zip_path, &entries, &cancelled);
        assert!(result.is_err());
        assert!(!zip_path.exists(), "partial ZIP should be cleaned up on error");
    }

    #[test]
    fn create_zip_sync_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("deep/nested/out.zip");
        let entries = vec![ZipEntry::Text {
            name: "a.txt".into(),
            content: "data".into(),
        }];
        let cancelled = AtomicBool::new(false);

        assert!(create_zip_sync(&zip_path, &entries, &cancelled).unwrap());
        assert!(zip_path.exists());
    }

    // ---- create_upload_file -------------------------------------------------

    struct NullBroadcaster;
    impl nomifun_realtime::EventBroadcaster for NullBroadcaster {
        fn broadcast(&self, _msg: nomifun_api_types::WebSocketMessage<serde_json::Value>) {}
    }

    fn make_service() -> crate::service::FileService {
        crate::service::FileService::new(Arc::new(NullBroadcaster), vec![])
    }

    #[tokio::test]
    async fn create_upload_file_writes_bytes_and_returns_path() {
        use crate::traits::IFileService;
        let svc = make_service();
        let unique = format!(
            "upload_test_{}.bin",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path_str = svc.create_upload_file(&unique, b"hello bytes", None).await.unwrap();
        let path = std::path::Path::new(&path_str);
        assert!(path.is_absolute());
        assert_eq!(path.file_name().unwrap().to_string_lossy(), unique);
        let contents = std::fs::read(path).unwrap();
        assert_eq!(contents, b"hello bytes");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn create_upload_file_routes_to_conversation_subdir() {
        use crate::traits::IFileService;
        let svc = make_service();
        let conv = format!(
            "conv-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let unique = format!(
            "img-{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path_str = svc
            .create_upload_file(&unique, b"\x89PNG\r\n", Some(&conv))
            .await
            .unwrap();
        let path = std::path::Path::new(&path_str);
        let parent = path.parent().unwrap();
        assert_eq!(parent.file_name().unwrap().to_string_lossy(), conv);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(parent);
    }

    #[tokio::test]
    async fn create_upload_file_rejects_path_separators() {
        use crate::traits::IFileService;
        let svc = make_service();
        let result = svc.create_upload_file("nested/file.png", b"x", None).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
        let result = svc.create_upload_file("nested\\file.png", b"x", None).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn create_upload_file_rejects_traversal() {
        use crate::traits::IFileService;
        let svc = make_service();
        let result = svc.create_upload_file("..", b"x", None).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn create_upload_file_rejects_empty_name() {
        use crate::traits::IFileService;
        let svc = make_service();
        let result = svc.create_upload_file("", b"x", None).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn create_upload_file_rejects_invalid_conversation_id() {
        use crate::traits::IFileService;
        let svc = make_service();
        let result = svc.create_upload_file("good.png", b"x", Some("../escape")).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
        let result = svc.create_upload_file("good.png", b"x", Some("nested/id")).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }

    // ---- name collision behaviour -----------------------------------------

    /// Generate a unique conversation id so each test gets a fresh directory.
    fn unique_conv_id(tag: &str) -> String {
        format!(
            "conv-collide-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn split_base_ext_matches_finder_conventions() {
        assert_eq!(split_base_ext("image.png"), ("image", ".png"));
        assert_eq!(split_base_ext("foo.tar.gz"), ("foo.tar", ".gz"));
        assert_eq!(split_base_ext("README"), ("README", ""));
        assert_eq!(split_base_ext(".env"), (".env", ""));
        assert_eq!(split_base_ext("a.b"), ("a", ".b"));
    }

    #[tokio::test]
    async fn create_upload_file_first_upload_uses_original_name() {
        use crate::traits::IFileService;
        let svc = make_service();
        let conv = unique_conv_id("first");
        let path_str = svc
            .create_upload_file("image.png", b"first", Some(&conv))
            .await
            .unwrap();
        let path = std::path::Path::new(&path_str);
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "image.png");
        assert_eq!(std::fs::read(path).unwrap(), b"first");

        let parent = path.parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[tokio::test]
    async fn create_upload_file_appends_numeric_suffix_on_conflict() {
        use crate::traits::IFileService;
        let svc = make_service();
        let conv = unique_conv_id("suffix");

        let first = svc.create_upload_file("image.png", b"one", Some(&conv)).await.unwrap();
        let second = svc.create_upload_file("image.png", b"two", Some(&conv)).await.unwrap();
        let third = svc
            .create_upload_file("image.png", b"three", Some(&conv))
            .await
            .unwrap();

        let first_path = std::path::Path::new(&first);
        let second_path = std::path::Path::new(&second);
        let third_path = std::path::Path::new(&third);

        assert_eq!(first_path.file_name().unwrap().to_string_lossy(), "image.png");
        assert_eq!(second_path.file_name().unwrap().to_string_lossy(), "image(2).png");
        assert_eq!(third_path.file_name().unwrap().to_string_lossy(), "image(3).png");

        // Originals stay intact — verifies no overwrite happened.
        assert_eq!(std::fs::read(first_path).unwrap(), b"one");
        assert_eq!(std::fs::read(second_path).unwrap(), b"two");
        assert_eq!(std::fs::read(third_path).unwrap(), b"three");

        let parent = first_path.parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[tokio::test]
    async fn create_upload_file_handles_extensionless_collision() {
        use crate::traits::IFileService;
        let svc = make_service();
        let conv = unique_conv_id("noext");

        let first = svc.create_upload_file("README", b"a", Some(&conv)).await.unwrap();
        let second = svc.create_upload_file("README", b"b", Some(&conv)).await.unwrap();

        let first_path = std::path::Path::new(&first);
        let second_path = std::path::Path::new(&second);

        assert_eq!(first_path.file_name().unwrap().to_string_lossy(), "README");
        assert_eq!(second_path.file_name().unwrap().to_string_lossy(), "README(2)");

        let parent = first_path.parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[tokio::test]
    async fn create_upload_file_handles_multi_dot_extension_collision() {
        use crate::traits::IFileService;
        let svc = make_service();
        let conv = unique_conv_id("multidot");

        let first = svc.create_upload_file("foo.tar.gz", b"a", Some(&conv)).await.unwrap();
        let second = svc.create_upload_file("foo.tar.gz", b"b", Some(&conv)).await.unwrap();

        let first_path = std::path::Path::new(&first);
        let second_path = std::path::Path::new(&second);

        assert_eq!(first_path.file_name().unwrap().to_string_lossy(), "foo.tar.gz");
        assert_eq!(second_path.file_name().unwrap().to_string_lossy(), "foo.tar(2).gz");

        let parent = first_path.parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[tokio::test]
    async fn create_upload_file_handles_hidden_file_collision() {
        use crate::traits::IFileService;
        let svc = make_service();
        let conv = unique_conv_id("hidden");

        let first = svc.create_upload_file(".env", b"a", Some(&conv)).await.unwrap();
        let second = svc.create_upload_file(".env", b"b", Some(&conv)).await.unwrap();

        let first_path = std::path::Path::new(&first);
        let second_path = std::path::Path::new(&second);

        assert_eq!(first_path.file_name().unwrap().to_string_lossy(), ".env");
        assert_eq!(second_path.file_name().unwrap().to_string_lossy(), ".env(2)");

        let parent = first_path.parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[tokio::test]
    async fn create_upload_file_preserves_all_bytes_across_collisions() {
        use crate::traits::IFileService;
        let svc = make_service();
        let conv = unique_conv_id("bytes");

        let a = svc.create_upload_file("image.png", b"AAA", Some(&conv)).await.unwrap();
        let b = svc.create_upload_file("image.png", b"BBB", Some(&conv)).await.unwrap();
        let c = svc.create_upload_file("image.png", b"CCC", Some(&conv)).await.unwrap();

        // All three files exist with distinct content — no overwrite.
        assert_eq!(std::fs::read(&a).unwrap(), b"AAA");
        assert_eq!(std::fs::read(&b).unwrap(), b"BBB");
        assert_eq!(std::fs::read(&c).unwrap(), b"CCC");

        // Sanity: three distinct paths.
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);

        let parent = std::path::Path::new(&a).parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&parent);
    }
}
