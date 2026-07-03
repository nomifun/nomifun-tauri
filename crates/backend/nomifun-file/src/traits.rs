use std::path::Path;
use std::sync::Arc;

use nomifun_common::{AppError, FileChangeOperation};

use crate::path_safety::PathAuthority;
use crate::types::{CompareResult, CopyResult, DirOrFile, FileMetadata, SnapshotInfo, WorkspaceFlatFile, ZipEntry};

/// Core file operations: directory browsing, file read/write, management,
/// image processing, and ZIP packaging.
///
/// All path parameters MUST be validated against the sandbox rules (see
/// `path_safety` module) before reaching this trait's implementations.
#[async_trait::async_trait]
pub trait IFileService: Send + Sync {
    // -- Directory browsing --

    /// List the immediate children of `dir`, returning a tree with one level
    /// of depth. `root` is the workspace root used to compute relative paths.
    async fn get_files_by_dir(&self, dir: &str, root: &str) -> Result<Vec<DirOrFile>, AppError>;

    /// Recursively list all files under `root` as a flat list.
    /// Returns at most 20,000 entries.
    async fn list_workspace_files(&self, root: &str) -> Result<Vec<WorkspaceFlatFile>, AppError>;

    /// Get metadata for a single file or directory.
    async fn get_file_metadata(&self, path: &str, extra_root: Option<&Path>) -> Result<FileMetadata, AppError>;

    // -- File read/write --

    /// Read a file as UTF-8 text. Returns `None` if the file does not exist.
    /// Files larger than 256 MB are rejected.
    async fn read_file(&self, path: &str, extra_root: Option<&Path>) -> Result<Option<String>, AppError>;

    /// Read a file as raw bytes. Returns `None` if the file does not exist.
    /// Files larger than 256 MB are rejected.
    async fn read_file_buffer(&self, path: &str, extra_root: Option<&Path>) -> Result<Option<Vec<u8>>, AppError>;

    /// Write `data` to `path`. On success, emits a
    /// `fileStream.contentUpdate` event with `operation = write`.
    async fn write_file(&self, path: &str, data: &[u8], workspace: &str) -> Result<bool, AppError>;

    // -- File management --

    /// Copy files into `workspace`, preserving directory structure relative to
    /// `source_root`. Returns lists of copied and failed paths.
    async fn copy_files_to_workspace(
        &self,
        file_paths: &[String],
        workspace: &str,
        source_root: Option<&str>,
    ) -> Result<CopyResult, AppError>;

    /// Remove a file or directory (recursively). On success, emits a
    /// `fileStream.contentUpdate` event with `operation = delete`.
    async fn remove_entry(&self, path: &str, workspace: &str) -> Result<(), AppError>;

    /// Rename a file or directory. Returns the new absolute path.
    async fn rename_entry(&self, path: &str, new_name: &str) -> Result<String, AppError>;

    /// Create an empty temporary file and return its absolute path.
    async fn create_temp_file(&self, file_name: &str) -> Result<String, AppError>;

    /// Write `data` to a temporary file and return its absolute path.
    ///
    /// When `conversation_id` is provided, the file is placed under a
    /// per-conversation sub-directory (`<tmp>/nomifun/<conversation_id>/`);
    /// otherwise the shared `<tmp>/nomifun/` directory is used (same as
    /// [`create_temp_file`](Self::create_temp_file)).
    ///
    /// `file_name` must not contain path separators or traversal patterns.
    async fn create_upload_file(
        &self,
        file_name: &str,
        data: &[u8],
        conversation_id: Option<&str>,
    ) -> Result<String, AppError>;

    // -- Image processing --

    /// Read a local image and return a base64 Data URL
    /// (e.g. `data:image/png;base64,...`).
    async fn get_image_base64(&self, path: &str, extra_root: Option<&Path>) -> Result<String, AppError>;

    /// Download a remote image and return a base64 Data URL.
    /// On failure, returns a placeholder SVG Data URL.
    async fn fetch_remote_image(&self, url: &str) -> String;

    // -- ZIP --

    /// Create a ZIP archive at `path` from `entries`.
    /// If `request_id` is provided, the operation can be cancelled via
    /// [`cancel_zip`](Self::cancel_zip).
    async fn create_zip(
        &self,
        path: &str,
        entries: Vec<ZipEntry>,
        request_id: Option<String>,
    ) -> Result<bool, AppError>;

    /// Cancel an in-progress ZIP operation by its `request_id`.
    /// Returns `true` if a matching operation was found and cancelled.
    async fn cancel_zip(&self, request_id: &str) -> bool;

    // -- Surface-scoped variants (trust-aware path authority) ----------------
    //
    // These mirror the path-scoped operations above but take an explicit
    // [`PathAuthority`] resolved from the caller's trust surface, instead of
    // implicitly confining to the service's construction-time `allowed_roots`.
    // A trusted local desktop caller passes `Unrestricted` (OS-user authority);
    // an external channel/remote caller passes `Confined([workspace])`. The
    // non-scoped methods above are equivalent to calling these with
    // `Confined(allowed_roots ∪ workspace)`, so existing callers are unchanged.

    /// [`get_files_by_dir`](Self::get_files_by_dir) under an explicit authority.
    async fn get_files_by_dir_scoped(
        &self,
        dir: &str,
        root: &str,
        authority: &PathAuthority,
    ) -> Result<Vec<DirOrFile>, AppError>;

    /// [`list_workspace_files`](Self::list_workspace_files) under an explicit authority.
    async fn list_workspace_files_scoped(
        &self,
        root: &str,
        authority: &PathAuthority,
    ) -> Result<Vec<WorkspaceFlatFile>, AppError>;

    /// [`get_file_metadata`](Self::get_file_metadata) under an explicit authority.
    async fn get_file_metadata_scoped(
        &self,
        path: &str,
        authority: &PathAuthority,
    ) -> Result<FileMetadata, AppError>;

    /// [`read_file`](Self::read_file) under an explicit authority.
    async fn read_file_scoped(
        &self,
        path: &str,
        authority: &PathAuthority,
    ) -> Result<Option<String>, AppError>;

    /// [`write_file`](Self::write_file) under an explicit authority. `workspace`
    /// is used only for the `contentUpdate` event's relative-path scoping.
    async fn write_file_scoped(
        &self,
        path: &str,
        data: &[u8],
        workspace: &str,
        authority: &PathAuthority,
    ) -> Result<bool, AppError>;

    /// [`remove_entry`](Self::remove_entry) under an explicit authority.
    async fn remove_entry_scoped(
        &self,
        path: &str,
        workspace: &str,
        authority: &PathAuthority,
    ) -> Result<(), AppError>;

    /// [`rename_entry`](Self::rename_entry) under an explicit authority.
    async fn rename_entry_scoped(
        &self,
        path: &str,
        new_name: &str,
        authority: &PathAuthority,
    ) -> Result<String, AppError>;
}

/// File system watching: single-file changes and workspace Office file
/// additions.
#[async_trait::async_trait]
pub trait IFileWatchService: Send + Sync {
    /// Start watching a single file for changes.
    /// Emits `fileWatch.fileChanged` events on the broadcast channel.
    async fn start_watch(&self, file_path: &str) -> Result<(), AppError>;

    /// Stop watching a previously registered file.
    async fn stop_watch(&self, file_path: &str) -> Result<(), AppError>;

    /// Stop all active file watches.
    async fn stop_all_watches(&self) -> Result<(), AppError>;

    /// Start watching a workspace directory for new Office files
    /// (.pptx, .docx, .xlsx).
    /// Emits `workspaceOfficeWatch.fileAdded` events.
    async fn start_office_watch(&self, workspace: &str) -> Result<(), AppError>;

    /// Stop watching a workspace directory for Office files.
    async fn stop_office_watch(&self, workspace: &str) -> Result<(), AppError>;
}

/// Git-based workspace snapshot system for tracking file changes.
///
/// Supports two modes:
/// - **git-repo**: directory already has `.git` — uses it directly.
/// - **snapshot**: no `.git` — creates a temporary repo under
///   `/tmp/nomifun-snapshot-*`.
#[async_trait::async_trait]
pub trait ISnapshotService: Send + Sync {
    /// Initialize the snapshot system for a workspace.
    /// Auto-detects `git-repo` or `snapshot` mode.
    async fn init(&self, workspace: &str) -> Result<SnapshotInfo, AppError>;

    /// Get the current snapshot mode and branch info.
    async fn get_info(&self, workspace: &str) -> Result<SnapshotInfo, AppError>;

    /// Compare workspace state against the baseline.
    /// Returns staged and unstaged changes.
    async fn compare(&self, workspace: &str) -> Result<CompareResult, AppError>;

    /// Get the baseline (HEAD) content of a file.
    /// Returns `None` for new/untracked files.
    async fn get_baseline_content(&self, workspace: &str, file_path: &str) -> Result<Option<String>, AppError>;

    /// Stage a single file (git-repo mode only).
    async fn stage_file(&self, workspace: &str, file_path: &str) -> Result<(), AppError>;

    /// Stage all changes.
    async fn stage_all(&self, workspace: &str) -> Result<(), AppError>;

    /// Unstage a single file.
    async fn unstage_file(&self, workspace: &str, file_path: &str) -> Result<(), AppError>;

    /// Unstage all staged changes.
    async fn unstage_all(&self, workspace: &str) -> Result<(), AppError>;

    /// Discard changes to a file (restore to baseline).
    async fn discard_file(
        &self,
        workspace: &str,
        file_path: &str,
        operation: FileChangeOperation,
    ) -> Result<(), AppError>;

    /// Reset a file to its baseline state.
    async fn reset_file(
        &self,
        workspace: &str,
        file_path: &str,
        operation: FileChangeOperation,
    ) -> Result<(), AppError>;

    /// List git branches (git-repo mode only).
    async fn get_branches(&self, workspace: &str) -> Result<Vec<String>, AppError>;

    /// Clean up snapshot resources.
    /// For snapshot mode, deletes the temporary git repository.
    async fn dispose(&self, workspace: &str) -> Result<(), AppError>;
}

/// Convenience alias for an Arc-wrapped file service.
pub type FileServiceRef = Arc<dyn IFileService>;

/// Convenience alias for an Arc-wrapped file watch service.
pub type FileWatchServiceRef = Arc<dyn IFileWatchService>;

/// Convenience alias for an Arc-wrapped snapshot service.
pub type SnapshotServiceRef = Arc<dyn ISnapshotService>;
