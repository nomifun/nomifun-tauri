//! File system operations: read/write, path safety, file watching, snapshots, and zip.
pub mod browse;
pub mod path_safety;
pub mod routes;
pub mod service;
pub mod snapshot_service;
pub mod traits;
pub mod types;
pub mod watch_service;
pub mod workspace_listing;

pub use path_safety::{PathAuthority, has_traversal, validate_path, validate_path_for_write};
pub use routes::{FileRouterState, file_routes};
pub use service::FileService;
pub use snapshot_service::SnapshotService;
pub use traits::{
    FileServiceRef, FileWatchServiceRef, IFileService, IFileWatchService, ISnapshotService, SnapshotServiceRef,
};
pub use types::{
    CompareResult, ContentUpdateEvent, ContentUpdateOperation, CopyResult, DirOrFile, FileChangeInfo, FileMetadata,
    FileWatchEvent, OfficeFileAddedEvent, SnapshotInfo, SnapshotMode, WorkspaceFlatFile, ZipEntry,
};
pub use watch_service::FileWatchService;
pub use workspace_listing::{MAX_DIR_DEPTH, list_workspace_level};
