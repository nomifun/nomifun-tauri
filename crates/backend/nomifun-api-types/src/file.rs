use nomifun_common::FileChangeOperation;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// A. Core file operations — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/fs/dir` — get files by directory.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetFilesByDirRequest {
    pub dir: String,
    pub root: String,
}

/// Request body for `POST /api/fs/list` — list workspace files.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListWorkspaceFilesRequest {
    pub root: String,
}

/// Request body for `POST /api/fs/metadata` — get file metadata.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetFileMetadataRequest {
    pub path: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Request body for `POST /api/fs/read` — read file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileRequest {
    pub path: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Request body for `POST /api/fs/read-buffer` — read file as binary.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileBufferRequest {
    pub path: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Request body for `POST /api/fs/write` — write file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteFileRequest {
    pub path: String,
    pub data: String,
    /// Workspace root, used to compute `relativePath` in the
    /// `fileStream.contentUpdate` event.  Falls back to the file's
    /// parent directory when absent.
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Request body for `POST /api/fs/copy` — copy files to workspace.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyFilesRequest {
    pub file_paths: Vec<String>,
    pub workspace: String,
    #[serde(default)]
    pub source_root: Option<String>,
}

/// Request body for `POST /api/fs/remove` — remove file or directory.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveEntryRequest {
    pub path: String,
    /// Workspace root, used to compute `relativePath` in the
    /// `fileStream.contentUpdate` event.  Falls back to the file's
    /// parent directory when absent.
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Request body for `POST /api/fs/rename` — rename file or directory.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameRequest {
    pub path: String,
    pub new_name: String,
}

/// Request body for `POST /api/fs/temp` — create temp file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTempFileRequest {
    pub file_name: String,
}

/// Request body for `POST /api/fs/image-base64` — get image as base64.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetImageBase64Request {
    pub path: String,
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Request body for `POST /api/fs/fetch-remote-image` — fetch remote image.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchRemoteImageRequest {
    pub url: String,
}

/// A single entry in a ZIP creation request.
#[derive(Debug, Clone, Deserialize)]
pub struct ZipFileEntry {
    pub name: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
}

/// Request body for `POST /api/fs/zip` — create ZIP archive.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZipRequest {
    pub path: String,
    #[serde(default)]
    pub request_id: Option<String>,
    pub files: Vec<ZipFileEntry>,
}

/// Request body for `POST /api/fs/zip/cancel` — cancel ZIP creation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelZipRequest {
    pub request_id: String,
}

/// Query parameters for `GET /api/fs/browse` — shallow directory browser.
///
/// Unlike `/api/fs/dir` (which returns a recursive tree scoped to a workspace
/// root), `browse` is a WebUI-only host-file picker: it lists a single
/// directory level, surfaces navigation hints (`can_go_up`, `parent_path`),
/// and on Windows supports a `__ROOT__` sentinel for the drive-list screen.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseDirectoryQuery {
    /// Directory to list. Empty string means "use default" (Windows: drive
    /// list; Unix: current working directory). `"__ROOT__"` on Windows is
    /// treated the same as an empty path.
    #[serde(default)]
    pub path: Option<String>,
    /// When true, include regular files in the response. Defaults to false
    /// (directories only).
    #[serde(default)]
    pub show_files: Option<String>,
}

/// A single entry in a `/api/fs/browse` response.
///
/// Uses camelCase on the wire to match the original Express contract the
/// frontend `DirectorySelectionModal` still consumes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BrowseEntry {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
    pub is_file: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Last-modified time as milliseconds since the unix epoch. Absent when
    /// the entry has no readable metadata (e.g. a Windows drive stub).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
}

/// Response body for `GET /api/fs/browse`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BrowseDirectoryResponse {
    /// The resolved directory currently being listed. Empty string when the
    /// response is a Windows drive-list screen.
    pub current_path: String,
    /// Path to navigate to when the user clicks "up". `None` when already at
    /// the root. Value `"__ROOT__"` is a sentinel used on Windows to mean
    /// "return to the drive-list screen".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_path: Option<String>,
    pub items: Vec<BrowseEntry>,
    pub can_go_up: bool,
    pub truncated: bool,
    /// True when the response represents the Windows drive-list screen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_root: Option<bool>,
}

// ---------------------------------------------------------------------------
// A. Core file operations — Response DTOs
// ---------------------------------------------------------------------------

/// A node in the directory tree returned by `getFilesByDir`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirOrFileResponse {
    pub name: String,
    pub full_path: String,
    pub relative_path: String,
    pub is_dir: bool,
    pub is_file: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<DirOrFileResponse>>,
}

/// A flat file entry returned by `listWorkspaceFiles`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceFlatFileResponse {
    pub name: String,
    pub full_path: String,
    pub relative_path: String,
}

/// File metadata returned by `getFileMetadata`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadataResponse {
    pub name: String,
    pub path: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub mime_type: String,
    pub last_modified: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_directory: Option<bool>,
}

/// Result of a batch copy operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyFilesResponse {
    pub copied_files: Vec<String>,
    pub failed_files: Vec<String>,
}

/// Result of a rename operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameResponse {
    pub new_path: String,
}

// ---------------------------------------------------------------------------
// D. File watch — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for `POST /api/fs/watch/start` and `/stop`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileWatchRequest {
    pub file_path: String,
}

/// Request body for `POST /api/fs/office-watch/start` and `/stop`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOfficeWatchRequest {
    pub workspace: String,
}

// ---------------------------------------------------------------------------
// E. Workspace snapshot — Request DTOs
// ---------------------------------------------------------------------------

/// Request body for snapshot init / getInfo / compare / stageAll / unstageAll / dispose.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotWorkspaceRequest {
    pub workspace: String,
}

/// Request body for snapshot getBaselineContent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotBaselineRequest {
    pub workspace: String,
    pub file_path: String,
}

/// Request body for snapshot stageFile / unstageFile.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotStageRequest {
    pub workspace: String,
    pub file_path: String,
}

/// Request body for snapshot discardFile / resetFile.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotDiscardRequest {
    pub workspace: String,
    pub file_path: String,
    pub operation: FileChangeOperation,
}

// ---------------------------------------------------------------------------
// E. Workspace snapshot — Response DTOs
// ---------------------------------------------------------------------------

/// Snapshot mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotMode {
    GitRepo,
    Snapshot,
    /// Snapshot tracking was refused for this workspace (e.g. drive root,
    /// a well-known system directory, or too large to safely snapshot).
    /// The human-readable cause is carried separately in
    /// [`SnapshotInfoResponse::reason`].
    Disabled,
}

/// Information about a workspace snapshot.
///
/// API Spec: `branch: string | null` — always present in JSON output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfoResponse {
    pub mode: SnapshotMode,
    pub branch: Option<String>,
    /// Present only for [`SnapshotMode::Disabled`]: why snapshot tracking was
    /// refused. `null` for the active `git-repo` / `snapshot` modes.
    #[serde(default)]
    pub reason: Option<String>,
}

/// A single file change entry in a compare result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileChangeInfoResponse {
    pub file_path: String,
    pub relative_path: String,
    pub operation: FileChangeOperation,
}

/// Result of comparing workspace changes (staged vs unstaged).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotCompareResponse {
    pub staged: Vec<FileChangeInfoResponse>,
    pub unstaged: Vec<FileChangeInfoResponse>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Request deserialization tests --

    #[test]
    fn get_files_by_dir_request_deserialization() {
        let raw = r#"{"dir":"/home/user/project","root":"/home/user"}"#;
        let req: GetFilesByDirRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.dir, "/home/user/project");
        assert_eq!(req.root, "/home/user");
    }

    #[test]
    fn copy_files_request_snake_case() {
        let raw = json!({
            "file_paths": ["/a.txt", "/b.txt"],
            "workspace": "/ws",
            "source_root": "/src"
        });
        let req: CopyFilesRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.file_paths, vec!["/a.txt", "/b.txt"]);
        assert_eq!(req.workspace, "/ws");
        assert_eq!(req.source_root.as_deref(), Some("/src"));
    }

    #[test]
    fn copy_files_request_optional_source_root() {
        let raw = json!({
            "file_paths": ["/a.txt"],
            "workspace": "/ws"
        });
        let req: CopyFilesRequest = serde_json::from_value(raw).unwrap();
        assert!(req.source_root.is_none());
    }

    #[test]
    fn rename_request_snake_case() {
        let raw = r#"{"path":"/ws/old.txt","new_name":"new.txt"}"#;
        let req: RenameRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.path, "/ws/old.txt");
        assert_eq!(req.new_name, "new.txt");
    }

    #[test]
    fn zip_request_snake_case() {
        let raw = json!({
            "path": "/out.zip",
            "request_id": "req-1",
            "files": [
                { "name": "a.txt", "content": "hello" },
                { "name": "b.bin", "file_path": "/src/b.bin" }
            ]
        });
        let req: ZipRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.path, "/out.zip");
        assert_eq!(req.request_id.as_deref(), Some("req-1"));
        assert_eq!(req.files.len(), 2);
        assert_eq!(req.files[0].content.as_deref(), Some("hello"));
        assert!(req.files[0].file_path.is_none());
        assert!(req.files[1].content.is_none());
        assert_eq!(req.files[1].file_path.as_deref(), Some("/src/b.bin"));
    }

    #[test]
    fn zip_request_optional_request_id() {
        let raw = json!({
            "path": "/out.zip",
            "files": [{ "name": "a.txt", "content": "x" }]
        });
        let req: ZipRequest = serde_json::from_value(raw).unwrap();
        assert!(req.request_id.is_none());
    }

    #[test]
    fn file_watch_request_snake_case() {
        let raw = r#"{"file_path":"/path/to/file.txt"}"#;
        let req: FileWatchRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.file_path, "/path/to/file.txt");
    }

    #[test]
    fn snapshot_discard_request_deserialization() {
        let raw = json!({
            "workspace": "/ws",
            "file_path": "src/main.rs",
            "operation": "modify"
        });
        let req: SnapshotDiscardRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.workspace, "/ws");
        assert_eq!(req.file_path, "src/main.rs");
        assert_eq!(req.operation, FileChangeOperation::Modify);
    }

    // -- Response serialization tests --

    #[test]
    fn dir_or_file_response_serialization() {
        let resp = DirOrFileResponse {
            name: "src".into(),
            full_path: "/project/src".into(),
            relative_path: "src".into(),
            is_dir: true,
            is_file: false,
            children: Some(vec![DirOrFileResponse {
                name: "main.rs".into(),
                full_path: "/project/src/main.rs".into(),
                relative_path: "src/main.rs".into(),
                is_dir: false,
                is_file: true,
                children: None,
            }]),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["name"], "src");
        assert_eq!(json["full_path"], "/project/src");
        assert_eq!(json["relative_path"], "src");
        assert_eq!(json["is_dir"], true);
        assert_eq!(json["is_file"], false);
        assert_eq!(json["children"][0]["name"], "main.rs");
    }

    #[test]
    fn dir_or_file_response_no_children_omitted() {
        let resp = DirOrFileResponse {
            name: "file.txt".into(),
            full_path: "/file.txt".into(),
            relative_path: "file.txt".into(),
            is_dir: false,
            is_file: true,
            children: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("children").is_none());
    }

    #[test]
    fn workspace_flat_file_response_serialization() {
        let resp = WorkspaceFlatFileResponse {
            name: "lib.rs".into(),
            full_path: "/project/src/lib.rs".into(),
            relative_path: "src/lib.rs".into(),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["name"], "lib.rs");
        assert_eq!(json["full_path"], "/project/src/lib.rs");
        assert_eq!(json["relative_path"], "src/lib.rs");
    }

    #[test]
    fn file_metadata_response_serialization() {
        let resp = FileMetadataResponse {
            name: "readme.md".into(),
            path: "/project/readme.md".into(),
            size: 1024,
            mime_type: "text/markdown".into(),
            last_modified: 1700000000000,
            is_directory: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["name"], "readme.md");
        assert_eq!(json["path"], "/project/readme.md");
        assert_eq!(json["size"], 1024);
        assert_eq!(json["type"], "text/markdown");
        assert_eq!(json["last_modified"], 1700000000000_i64);
        assert!(json.get("is_directory").is_none());
    }

    #[test]
    fn file_metadata_response_with_directory_flag() {
        let resp = FileMetadataResponse {
            name: "src".into(),
            path: "/project/src".into(),
            size: 0,
            mime_type: "".into(),
            last_modified: 1700000000000,
            is_directory: Some(true),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["is_directory"], true);
    }

    #[test]
    fn copy_files_response_serialization() {
        let resp = CopyFilesResponse {
            copied_files: vec!["/ws/a.txt".into()],
            failed_files: vec!["/missing.txt".into()],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["copied_files"][0], "/ws/a.txt");
        assert_eq!(json["failed_files"][0], "/missing.txt");
    }

    #[test]
    fn snapshot_mode_serialization() {
        assert_eq!(serde_json::to_value(SnapshotMode::GitRepo).unwrap(), "git-repo");
        assert_eq!(serde_json::to_value(SnapshotMode::Snapshot).unwrap(), "snapshot");
    }

    #[test]
    fn snapshot_mode_deserialization() {
        let mode: SnapshotMode = serde_json::from_str(r#""git-repo""#).unwrap();
        assert_eq!(mode, SnapshotMode::GitRepo);
        let mode: SnapshotMode = serde_json::from_str(r#""snapshot""#).unwrap();
        assert_eq!(mode, SnapshotMode::Snapshot);
    }

    #[test]
    fn snapshot_info_response_git_repo() {
        let resp = SnapshotInfoResponse {
            mode: SnapshotMode::GitRepo,
            branch: Some("main".into()),
            reason: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["mode"], "git-repo");
        assert_eq!(json["branch"], "main");
        assert!(json["reason"].is_null());
    }

    #[test]
    fn snapshot_info_response_snapshot_mode() {
        let resp = SnapshotInfoResponse {
            mode: SnapshotMode::Snapshot,
            branch: None,
            reason: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["mode"], "snapshot");
        // API Spec: branch is always present, null when snapshot mode
        assert!(json["branch"].is_null());
        assert!(json["reason"].is_null());
    }

    #[test]
    fn snapshot_info_response_disabled_mode() {
        let resp = SnapshotInfoResponse {
            mode: SnapshotMode::Disabled,
            branch: None,
            reason: Some("workspace too large to snapshot".into()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["mode"], "disabled");
        assert!(json["branch"].is_null());
        assert_eq!(json["reason"], "workspace too large to snapshot");
    }

    #[test]
    fn snapshot_compare_response_serialization() {
        let resp = SnapshotCompareResponse {
            staged: vec![FileChangeInfoResponse {
                file_path: "/ws/a.txt".into(),
                relative_path: "a.txt".into(),
                operation: FileChangeOperation::Create,
            }],
            unstaged: vec![FileChangeInfoResponse {
                file_path: "/ws/b.txt".into(),
                relative_path: "b.txt".into(),
                operation: FileChangeOperation::Modify,
            }],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["staged"][0]["file_path"], "/ws/a.txt");
        assert_eq!(json["staged"][0]["relative_path"], "a.txt");
        assert_eq!(json["staged"][0]["operation"], "create");
        assert_eq!(json["unstaged"][0]["operation"], "modify");
    }

    #[test]
    fn snapshot_compare_response_deserialization() {
        let raw = json!({
            "staged": [
                { "file_path": "/ws/x.rs", "relative_path": "x.rs", "operation": "delete" }
            ],
            "unstaged": []
        });
        let resp: SnapshotCompareResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.staged.len(), 1);
        assert_eq!(resp.staged[0].operation, FileChangeOperation::Delete);
        assert!(resp.unstaged.is_empty());
    }
}
