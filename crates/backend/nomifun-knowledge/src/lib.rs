//! `nomifun-knowledge` — the Knowledge Base platform domain: user-curated
//! directories of markdown documents, registered globally and mounted
//! (junction/symlink) into session workspaces as an extended knowledge
//! source. Sessions with the write-back ("回血") switch enabled are told via
//! prompt contract that they may persist new knowledge back into the mounted
//! directories.
//!
//! Layering: `service` owns registry CRUD + file access + mount planning;
//! `mount` is the platform-aware link engine (junction on Windows, symlink on
//! Unix, recursive copy fallback); `routes`/`state` are the `/api/knowledge/*`
//! surface; `events` pushes WS notifications.
//!
//! The directory is the source of truth for content — the database only
//! stores registration metadata, so users may drop `.md` files in at any
//! time. Consumers other than conversations (terminal, companion) reuse the same
//! `(target_kind, target_id)` binding storage; the companion integration is
//! intentionally deferred (no code here depends on conversation or companion).

pub mod autogen;
pub mod connector;
pub mod connector_feishu;
pub mod context;
pub mod events;
pub mod export;
pub mod feishu_md;
pub mod mcp_server;
pub mod mount;
pub mod routes;
pub mod service;
pub mod source_url;
pub mod state;
pub mod turn_writeback;
pub mod workpath;

#[cfg(test)]
pub(crate) mod testutil;

pub use autogen::KnowledgeCompleter;
pub use context::{KnowledgeContextFormat, KnowledgeContextOptions, WritebackEagerness, WritebackMode, build_knowledge_context};
pub use events::KnowledgeEventEmitter;
pub use mcp_server::KnowledgeMcpServer;
pub use routes::knowledge_routes;
pub use service::{
    AutogenOutcome, ConsumerInfo, InboxDiff, InboxEntry, InboxMergeResult, KB_INBOX_REL_DIR, KnowledgeBinding,
    KnowledgeService, MountOutcome, RefreshSourceSummary, TurnWritebackFailure, TurnWritebackReport,
    TurnWritebackPhase, TurnWritebackRequest, TurnWritebackStatus, WriteMode, WriteOp, WriteOutcome, WritePolicy,
    WriteRequest, WriteResolution, WriteSurface, WriteTargetSpec, decode_doc_handle, encode_doc_handle,
    resolve_write_policy,
};
pub use source_url::{HttpFetcher, PageFetcher, UrlFetcher};
pub use state::KnowledgeRouterState;
pub use turn_writeback::{TurnWritebackCandidate, TurnWritebackOutput};
pub use workpath::{DEFAULT_WORKPATH_KEY, WORKPATH_BINDING_KIND, session_workpath_key, workpath_key};

/// Workspace-relative directory where knowledge bases are mounted. Lives
/// under the hidden `.nomi/` folder — the same agent-facing namespace as
/// `.nomi/skills` / `.nomi/plans` — so mounting into a user's own project
/// directory stays unobtrusive (the mount dir self-ignores via its own
/// `.gitignore`, see `mount.rs`).
pub const KB_MOUNT_REL_DIR: &str = ".nomi/knowledge";

/// Pre-`.nomi` mount location. Kept solely so `mount::sync_mounts` can sweep
/// leftover links/scaffolding out of workspaces created before the rename —
/// never mount anything here.
pub const KB_LEGACY_MOUNT_REL_DIR: &str = ".nomifun/knowledge";

/// Subdirectory of the backend data dir that hosts managed base directories:
/// `{data_dir}/knowledge/{kb_id}/`.
pub const KB_MANAGED_REL_DIR: &str = "knowledge";
