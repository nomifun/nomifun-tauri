//! `KnowledgeService` — registry CRUD, markdown file access, and mount
//! planning for the Knowledge Base platform.
//!
//! The directory is the source of truth: the user may add/remove `.md` files
//! out-of-band at any time, so file listings/stats are computed on demand
//! rather than cached in the database.

use std::collections::HashSet;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use futures_util::{StreamExt, stream};
use nomifun_api_types::{ConnectorCredentialSummary, ConnectorSyncState, KnowledgeMountInfo, KnowledgeSource, KnowledgeSourceEntry, KnowledgeSourceMode, KnowledgeTag, UpdateKnowledgeTagRequest};
use nomifun_common::{AppError, TimestampMs, generate_prefixed_id, now_ms};
use nomifun_db::models::{CreateKnowledgeTagParams, KnowledgeBaseRow, KnowledgeBindingRow};
use nomifun_db::{IConnectorCredentialRepository, IKnowledgeRepository};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::autogen::{self, KnowledgeCompleter};
use crate::connector::{ConnectorCredential, ConnectorIdentity, ConnectorScope, KnowledgeConnector, RemoteDocRef, SyncCursor, SyncPage};
use crate::events::KnowledgeEventEmitter;
use crate::mount::{self, MountSpec};
use crate::source_url::{self, HttpFetcher, PageFetcher};
use crate::workpath::{WORKPATH_BINDING_KIND, workpath_key};
use crate::{KB_MANAGED_REL_DIR, KB_MOUNT_REL_DIR};

/// Binding target kinds accepted by the API. `workpath` is the primary kind
/// for conversation/terminal sessions since the session-list unification
/// (its `target_id` is a normalized [`workpath_key`]); `conversation` and
/// `terminal` are the legacy per-session bindings still honored as a read
/// fallback (see [`KnowledgeService::ensure_mounts_for_session`]); `companion`
/// binds a companion's sessions.
pub const BINDING_KINDS: &[&str] = &["workpath", "conversation", "terminal", "companion"];

/// Write-back modes. `staged` confines agent writes to
/// `_inbox/{conversation_id}/` inside the base (conflict-free across
/// concurrent sessions); `direct` lets the agent edit the base body.
pub const WRITEBACK_MODES: &[&str] = &["staged", "direct"];

/// Accepted write-back dispositions ("回写意识"), orthogonal to
/// [`WRITEBACK_MODES`]: `conservative` (restrained, the default) only persists
/// clearly-worth-keeping knowledge; `aggressive` captures anything plausibly
/// relevant. Both are prompt-contract wording only.
pub const WRITEBACK_EAGERNESS: &[&str] = &["conservative", "aggressive"];

/// Subdirectory of a base root that holds staged (unreviewed) write-backs.
/// Excluded from the prompt TOC — unreviewed content is not authoritative
/// navigation.
pub const KB_INBOX_REL_DIR: &str = "_inbox";

/// Opaque, copy-pasteable document handle: `kdoc_` + URL-safe base64 (no pad)
/// of `{kb_id}\x1f{rel_path}`. The model treats it as an opaque token and
/// never parses or builds paths — `knowledge_search` emits it, `knowledge_read`
/// and `knowledge_write` consume it, closing a zero-path-arithmetic loop.
const DOC_HANDLE_PREFIX: &str = "kdoc_";
const DOC_HANDLE_SEP: char = '\u{1f}';

/// Encode a stable `(kb_id, rel_path)` document handle. See [`DOC_HANDLE_PREFIX`].
pub fn encode_doc_handle(kb_id: &str, rel_path: &str) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let raw = format!("{kb_id}{DOC_HANDLE_SEP}{rel_path}");
    format!("{DOC_HANDLE_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw.as_bytes()))
}

/// Decode a document handle back to `(kb_id, rel_path)`. Returns `None` for any
/// malformed input (wrong prefix, bad base64, non-UTF8, missing separator, or
/// an empty component).
pub fn decode_doc_handle(handle: &str) -> Option<(String, String)> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let body = handle.strip_prefix(DOC_HANDLE_PREFIX)?;
    let bytes = URL_SAFE_NO_PAD.decode(body.as_bytes()).ok()?;
    let raw = String::from_utf8(bytes).ok()?;
    let (kb_id, rel_path) = raw.split_once(DOC_HANDLE_SEP)?;
    if kb_id.is_empty() || rel_path.is_empty() {
        return None;
    }
    Some((kb_id.to_owned(), rel_path.to_owned()))
}

/// Hard cap on `source.entries` per knowledge base — every entry costs a
/// network fetch at create/refresh time, so an unbounded list would let one
/// request fan out arbitrarily.
pub const MAX_SOURCE_ENTRIES: usize = 16;

/// How many source entries are fetched concurrently per batch.
const SOURCE_FETCH_CONCURRENCY: usize = 4;

/// A knowledge base plus directory statistics, as returned by the API.
#[derive(Debug, Clone, Serialize)]
pub struct KnowledgeBaseInfo {
    pub id: String,
    /// Legacy display-only sequence number; no longer sourced from the
    /// registry (the `seq` column was dropped in the primary-key rework) —
    /// always `None`, kept on the wire until the frontend stops reading it.
    pub seq: Option<i64>,
    pub name: String,
    pub description: String,
    pub root_path: String,
    pub managed: bool,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub file_count: u64,
    pub total_size: u64,
    /// `false` when the registered root directory no longer exists on disk.
    pub root_exists: bool,
    /// URL source configuration (`extra.source`) when the base has one;
    /// `None` (and off the wire) for plain directory bases. Carried by
    /// every path that serializes this struct (list/get/create/update
    /// responses and `knowledge.base-*` events) so the frontend detail
    /// page can render mode / URL count / lastFetchedAt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<KnowledgeSource>,
    /// Create-time URL-source fetch summary. Populated only on the response
    /// of a create that carried a snapshot-mode source; `None` (and off the
    /// wire) everywhere else (list/get/update, events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_fetch: Option<RefreshSourceSummary>,
    /// User-defined tag keys assigned to this base (empty = untagged).
    #[serde(default)]
    pub tags: Vec<String>,
    /// UI type discriminator, derived from `managed` + `extra.source`:
    /// `"blank"` | `"local"` | `"web"` | `"feishu"`.
    pub kind: String,
    /// Number of pending inbox proposals for this base (drives list badge /
    /// detail tab count).
    pub pending_inbox: u64,
}

/// One `search_bases` hit. `rel_path` is relative to the base root.
#[derive(Debug, Clone, serde::Serialize)]
pub struct KnowledgeSearchHit {
    pub kb_id: String,
    pub kb_name: String,
    pub rel_path: String,
    pub heading: String,
    pub snippet: String,
    pub score: u32,
}

/// One markdown file inside a base, path relative to the base root
/// (forward slashes on every platform).
#[derive(Debug, Clone, Serialize)]
pub struct KbFileEntry {
    pub rel_path: String,
    pub size: u64,
    pub modified_at: Option<TimestampMs>,
}

/// One immediate child in the knowledge-base document tree. Directories are
/// browse-only; files are markdown documents that can be read/edited.
#[derive(Debug, Clone, Serialize)]
pub struct KbTreeEntry {
    pub name: String,
    pub rel_path: String,
    pub is_dir: bool,
    pub is_file: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub modified_at: Option<TimestampMs>,
}

/// Content payload for a single file read.
#[derive(Debug, Clone, Serialize)]
pub struct KbFileContent {
    pub rel_path: String,
    pub content: String,
    pub size: u64,
    pub modified_at: Option<TimestampMs>,
}

/// One staged write-back proposal living under `_inbox/{scope}/{rel_path}`.
/// `scope` is the first path segment (the conversation/session id that staged
/// it); `rel_path` mirrors the original base-relative path.
#[derive(Debug, Clone, Serialize)]
pub struct InboxEntry {
    pub scope: String,
    pub rel_path: String,
    pub size: u64,
    pub modified_at: Option<TimestampMs>,
}

/// A staged proposal vs. its current base version, for the review panel.
/// `base_content` is `None` (and `is_new` true) when the proposal would create
/// a brand-new document. `unified_diff` is a server-computed unified diff
/// (`similar`), ready to hand to the frontend diff renderer.
#[derive(Debug, Clone, Serialize)]
pub struct InboxDiff {
    pub scope: String,
    pub rel_path: String,
    pub inbox_content: String,
    pub base_content: Option<String>,
    pub unified_diff: String,
    pub is_new: bool,
}

/// Result of accepting a staged proposal (the base path now holding the merged
/// content).
#[derive(Debug, Clone, Serialize)]
pub struct InboxMergeResult {
    pub merged_path: String,
}

/// One consumer (binding) of a knowledge base — a workspace/conversation/etc.
/// that has this base mounted. Includes disabled bindings (greyed in the UI).
#[derive(Debug, Clone, Serialize)]
pub struct ConsumerInfo {
    pub target_kind: String,
    pub target_id: Option<String>,
    pub enabled: bool,
}

/// Per-target mount configuration (the public shape of a binding row).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeBinding {
    pub enabled: bool,
    pub writeback: bool,
    #[serde(default = "default_writeback_mode")]
    pub writeback_mode: String,
    #[serde(default = "default_writeback_eagerness")]
    pub writeback_eagerness: String,
    /// External IM channel write opt-in (forced staged). Default false —
    /// channel master-agent writes are disabled unless re-enabled here.
    #[serde(default)]
    pub channel_write_enabled: bool,
    #[serde(default)]
    pub kb_ids: Vec<String>,
}

fn default_writeback_mode() -> String {
    "staged".to_owned()
}

fn default_writeback_eagerness() -> String {
    "conservative".to_owned()
}

impl Default for KnowledgeBinding {
    fn default() -> Self {
        Self {
            enabled: false,
            writeback: false,
            writeback_mode: default_writeback_mode(),
            writeback_eagerness: default_writeback_eagerness(),
            channel_write_enabled: false,
            kb_ids: Vec::new(),
        }
    }
}

/// Result of a mount sync for one target: what is mounted and whether the
/// write-back contract applies. Consumed by the conversation service to
/// inject prompt context.
#[derive(Debug, Clone, Default)]
pub struct MountOutcome {
    pub mounts: Vec<KnowledgeMountInfo>,
    pub writeback: bool,
    /// `staged` or `direct`; meaningful only while `writeback` is true.
    pub writeback_mode: String,
    /// `conservative` or `aggressive` ("回写意识"); meaningful only while
    /// `writeback` is true.
    pub writeback_eagerness: String,
    /// Raw `channel_write_enabled` opt-in from the binding. Carried verbatim
    /// (independent of `writeback`) so the nomi factory can resolve the
    /// external-IM-channel write policy with the SAME value the ACP path reads
    /// at write time — without it the nomi path reconstructs the binding with a
    /// `false` default and channel write-back is permanently disabled.
    pub channel_write_enabled: bool,
}

/// What the model addressed for a write: an opaque `handle` (preferred — from
/// `knowledge_search`/`knowledge_read`) or an explicit base + relative path
/// (browse / create).
#[derive(Debug, Clone)]
pub enum WriteTargetSpec {
    Handle(String),
    Path { kb_id: String, rel_path: String },
}

/// Result of resolving a write target to a canonical, base-relative document.
#[derive(Debug, Clone)]
pub struct WriteResolution {
    pub kb_id: String,
    pub canonical_rel_path: String,
    pub op: WriteOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOp {
    Update,
    Create,
}

/// Where a write originates — selects the write policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteSurface {
    RegularChat,
    Companion,
    TerminalAcp,
    ExternalChannel,
}

/// Code-enforced placement. `Staged{scope}` confines writes to
/// `_inbox/{scope}/…`; `Direct` writes the base body; `Disabled` refuses.
#[derive(Debug, Clone)]
pub enum WriteMode {
    Disabled,
    Staged { scope: String },
    Direct,
}

#[derive(Debug, Clone)]
pub struct WritePolicy {
    pub mode: WriteMode,
    pub allow_create: bool,
    pub surface: WriteSurface,
}

/// A fully-specified write through the single canonical path.
#[derive(Debug, Clone)]
pub struct WriteRequest {
    pub spec: WriteTargetSpec,
    pub content: String,
    pub policy: WritePolicy,
    pub bound_kb_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WriteOutcome {
    pub kb_id: String,
    pub final_rel_path: String,
    pub op: WriteOp,
    pub staged: bool,
}

/// Per-surface write policy. Regular chat / terminal honor the binding's
/// staged|direct choice (staged default); companion always writes direct when
/// write-back is on; external IM channels are hard-disabled in P1 (the opt-in
/// re-enable toggle is P2). `scope` is the staged inbox namespace
/// (conversation/companion id).
pub fn resolve_write_policy(surface: WriteSurface, binding: &KnowledgeBinding, scope: &str) -> WritePolicy {
    let writeback = binding.enabled && binding.writeback;
    let mode = if !writeback {
        WriteMode::Disabled
    } else {
        match surface {
            WriteSurface::Companion => WriteMode::Direct,
            // External IM channel: disabled by default; the opt-in toggle
            // (`channel_write_enabled`) re-enables it, but ALWAYS staged —
            // an unattended bot's writes go through the review inbox.
            WriteSurface::ExternalChannel => {
                if binding.channel_write_enabled {
                    WriteMode::Staged { scope: scope.to_owned() }
                } else {
                    WriteMode::Disabled
                }
            }
            WriteSurface::RegularChat | WriteSurface::TerminalAcp => match binding.writeback_mode.as_str() {
                "direct" => WriteMode::Direct,
                _ => WriteMode::Staged { scope: scope.to_owned() },
            },
        }
    };
    WritePolicy { mode, allow_create: true, surface }
}

/// Result of an AI overview generation (`POST /bases/{id}/autogen`).
#[derive(Debug, Clone, Serialize)]
pub struct AutogenOutcome {
    /// The (possibly clamped) description after the run.
    pub description: String,
    /// Whether this run replaced the registry description.
    pub description_updated: bool,
    /// Whether this run wrote `{root}/README.md`.
    pub readme_written: bool,
    pub base: KnowledgeBaseInfo,
}

/// Result of a URL-source fetch batch (`POST /bases/{id}/refresh-source`,
/// also attached to the create response via `source_fetch`).
#[derive(Debug, Clone, Serialize)]
pub struct RefreshSourceSummary {
    /// Entries whose snapshot was (re)written.
    pub fetched: usize,
    pub failed: usize,
    /// One `"{url}: {error}"` line per failed entry.
    pub errors: Vec<String>,
    /// `extra.source.last_fetched_at` after this run: re-stamped only when
    /// at least one entry was fetched; a fully-failed run reports the
    /// previous value (possibly `None`) unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fetched_at: Option<TimestampMs>,
}

/// Per-file cached content for `search_bases`, keyed by absolute path + mtime.
struct CachedDoc {
    mtime_ms: u64,
    content: Arc<str>,
    heading: Arc<str>,
    bytes: usize,
}

/// mtime-keyed content cache backing `search_bases`. A pure read-through
/// optimization: it avoids re-reading + UTF-8-decoding unchanged `.md` files on
/// every query. Invalidation is per-file via mtime (write_file's atomic rename
/// and delete_file both bump it), so the cache self-heals with NO watcher and NO
/// index — results are byte-for-byte identical to the uncached path. Bounded by
/// [`MAX_SEARCH_CACHE_BYTES`]; oversized files are simply not cached.
#[derive(Default)]
struct SearchCacheInner {
    entries: std::collections::HashMap<PathBuf, CachedDoc>,
    total_bytes: usize,
}

const MAX_SEARCH_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SEARCH_CACHE_FILE_BYTES: usize = 1024 * 1024;

pub struct KnowledgeService {
    repo: Arc<dyn IKnowledgeRepository>,
    data_dir: PathBuf,
    emitter: KnowledgeEventEmitter,
    /// LLM seam for autogen / snapshot compression. Late-wired (the agent
    /// stack is built after this service); `None` ⇒ autogen endpoints fail
    /// with a clear 409 and best-effort call sites skip silently.
    completer: RwLock<Option<Arc<dyn KnowledgeCompleter>>>,
    /// Page-fetching backend for URL knowledge sources. A trait object so a
    /// rendering backend (`BrowserFetcher`, late-wired from `nomifun-ai-agent`)
    /// can replace the default HTTP fetcher without the knowledge crate
    /// depending on the browser engine (P3 anti-cycle decision ②).
    fetcher: Arc<dyn PageFetcher>,
    /// **P3-K2: optional rendering page-fetcher** (the engine-backed
    /// `BrowserFetcher`, late-wired from `nomifun-ai-agent` when the `browser-use`
    /// feature is on). `None` ⇒ no browser backend available; every source uses
    /// [`Self::fetcher`] (the HTTP default — current behaviour, zero regression).
    /// K2 only *provides* this backend; **per-source backend selection (the
    /// `rendered` flag → pick this vs. the HTTP fetcher) is K3's job** and lives at
    /// the [`Self::prepare_snapshot_body`] dispatch site, which K2 leaves untouched.
    /// Behind a `RwLock` so it can be late-wired on the shared `Arc<KnowledgeService>`
    /// after construction (same discipline as [`Self::completer`]).
    render_fetcher: RwLock<Option<Arc<dyn PageFetcher>>>,
    /// mtime-keyed content cache for `search_bases` (perf only; see
    /// [`SearchCacheInner`]). Cloned into the search `spawn_blocking` closure.
    search_cache: Arc<RwLock<SearchCacheInner>>,
    /// **P3 connectors**: registered source connectors keyed by `kind()`
    /// (e.g. `"feishu"`). Late-wired at boot (`register_connector`) — same
    /// discipline as [`Self::completer`], since a connector may depend on the
    /// agent/http stack built after this service.
    connectors: RwLock<HashMap<&'static str, Arc<dyn KnowledgeConnector>>>,
    /// **P3 connectors**: encrypted credential store. `None` until late-wired
    /// (`set_connector_credentials`); credential endpoints fail with a clear
    /// 409 until then. Paired with [`Self::cred_key`].
    cred_repo: RwLock<Option<Arc<dyn IConnectorCredentialRepository>>>,
    /// **P3 connectors**: AES-256-GCM key (machine-bound, derived from the JWT
    /// secret — same key the provider api-key column uses) for encrypting
    /// credential payloads at rest. Late-wired alongside [`Self::cred_repo`].
    cred_key: RwLock<Option<[u8; 32]>>,
}

/// One source entry's fetched-and-condensed body, ready to be slugged and
/// written to disk (the serial phase of [`KnowledgeService::fetch_source_snapshots`]).
struct PreparedSnapshot {
    /// Page `<title>` (HTML responses only) — backfills an empty entry title.
    title: Option<String>,
    body: String,
}

impl KnowledgeService {
    pub fn new(repo: Arc<dyn IKnowledgeRepository>, data_dir: &Path, emitter: KnowledgeEventEmitter) -> Self {
        Self {
            repo,
            data_dir: data_dir.to_path_buf(),
            emitter,
            completer: RwLock::new(None),
            fetcher: Arc::new(HttpFetcher::default()),
            render_fetcher: RwLock::new(None),
            search_cache: Arc::new(RwLock::new(SearchCacheInner::default())),
            connectors: RwLock::new(HashMap::new()),
            cred_repo: RwLock::new(None),
            cred_key: RwLock::new(None),
        }
    }

    /// Replace the URL fetcher. Accepts any [`PageFetcher`] (tests pass a
    /// loopback-permitting [`HttpFetcher`]; the production rendering backend
    /// late-wires its `BrowserFetcher`), wrapping it in the `Arc<dyn …>` the
    /// service stores.
    pub fn with_url_fetcher(mut self, fetcher: impl PageFetcher + 'static) -> Self {
        self.fetcher = Arc::new(fetcher);
        self
    }

    /// Late-wire the production LLM completer (see `nomifun-ai-agent`'s
    /// `LiveKnowledgeCompleter`).
    pub fn set_completer(&self, completer: Arc<dyn KnowledgeCompleter>) {
        *self.completer.write().expect("knowledge completer lock poisoned") = Some(completer);
    }

    /// **P3-K2: late-wire the rendering page-fetcher** (the engine-backed
    /// `BrowserFetcher` from `nomifun-ai-agent`, wired by the app layer when the
    /// `browser-use` feature is on). Interior-mutable so it can be set on the shared
    /// `Arc<KnowledgeService>` after construction (the agent stack is built after
    /// this service — same late-wire timing as [`Self::set_completer`]).
    ///
    /// This only *registers* the backend. It does **not** change which sources use
    /// it: the default [`Self::fetcher`] (HTTP) stays the active path for every
    /// source, so HTTP knowledge sources are unaffected (zero regression). Routing
    /// a source to this backend (the `rendered` flag) is K3.
    pub fn set_render_fetcher(&self, fetcher: Arc<dyn PageFetcher>) {
        *self.render_fetcher.write().expect("knowledge render fetcher lock poisoned") = Some(fetcher);
    }

    /// The wired rendering page-fetcher, if any (K3 reads this to route `rendered`
    /// sources). `None` ⇒ no browser backend → fall back to the HTTP [`Self::fetcher`].
    fn render_fetcher(&self) -> Option<Arc<dyn PageFetcher>> {
        self.render_fetcher.read().ok().and_then(|guard| guard.clone())
    }

    // ── P3 connectors: registry + credential store (late-wired) ───────

    /// Register a source connector (keyed by its `kind()`). Boot-time
    /// late-wire, mirroring [`Self::set_completer`] — connectors may depend on
    /// the http/agent stack constructed after this service.
    pub fn register_connector(&self, connector: Arc<dyn KnowledgeConnector>) {
        let kind = connector.kind();
        self.connectors
            .write()
            .expect("knowledge connectors lock poisoned")
            .insert(kind, connector);
    }

    /// The connector registered for `kind`, if any.
    fn connector_for(&self, kind: &str) -> Option<Arc<dyn KnowledgeConnector>> {
        self.connectors.read().ok()?.get(kind).cloned()
    }

    /// Late-wire the encrypted credential store: the repository plus the
    /// machine-bound AES key (derive it once from the JWT secret, same key the
    /// provider api-key column uses). Until this is called, every credential
    /// endpoint returns a clear 409.
    pub fn set_connector_credentials(&self, repo: Arc<dyn IConnectorCredentialRepository>, key: [u8; 32]) {
        *self.cred_repo.write().expect("knowledge cred_repo lock poisoned") = Some(repo);
        *self.cred_key.write().expect("knowledge cred_key lock poisoned") = Some(key);
    }

    fn cred_repo(&self) -> Result<Arc<dyn IConnectorCredentialRepository>, AppError> {
        self.cred_repo
            .read()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| AppError::Conflict("connector credential store is not configured".into()))
    }

    fn cred_key(&self) -> Result<[u8; 32], AppError> {
        self.cred_key
            .read()
            .ok()
            .and_then(|g| *g)
            .ok_or_else(|| AppError::Conflict("connector credential store is not configured".into()))
    }

    /// Decrypt a stored credential into the in-memory [`ConnectorCredential`]
    /// the connector authenticates with.
    async fn load_credential(&self, id: &str) -> Result<ConnectorCredential, AppError> {
        let repo = self.cred_repo()?;
        let key = self.cred_key()?;
        let row = repo
            .get(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("connector credential not found: {id}")))?;
        let plaintext = nomifun_common::decrypt_string(&row.payload_encrypted, &key)?;
        let payload: serde_json::Value = serde_json::from_str(&plaintext)
            .map_err(|e| AppError::Internal(format!("credential payload decode failed: {e}")))?;
        Ok(ConnectorCredential { id: row.id, kind: row.kind, name: row.name, payload })
    }

    /// All stored credentials (secret-free summaries).
    pub async fn list_credentials(&self) -> Result<Vec<ConnectorCredentialSummary>, AppError> {
        let repo = self.cred_repo()?;
        let rows = repo.list().await?;
        Ok(rows
            .into_iter()
            .map(|r| ConnectorCredentialSummary { id: r.id, kind: r.kind, name: r.name, created_at: r.created_at })
            .collect())
    }

    /// Validate then store a new credential. The payload (e.g. Feishu
    /// `{ app_id, app_secret }`) is probed against the remote before being
    /// AES-encrypted at rest — bad secrets fail fast and are never persisted.
    pub async fn create_credential(
        &self,
        kind: &str,
        name: &str,
        payload: serde_json::Value,
    ) -> Result<ConnectorCredentialSummary, AppError> {
        let connector = self
            .connector_for(kind)
            .ok_or_else(|| AppError::BadRequest(format!("no connector registered for kind \"{kind}\"")))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::BadRequest("credential name must not be empty".into()));
        }
        // Fail fast: never persist a credential that can't authenticate.
        let probe = ConnectorCredential {
            id: String::new(),
            kind: kind.to_owned(),
            name: name.to_owned(),
            payload: payload.clone(),
        };
        connector.validate_credentials(&probe).await?;

        let key = self.cred_key()?;
        let repo = self.cred_repo()?;
        let plaintext =
            serde_json::to_string(&payload).map_err(|e| AppError::Internal(format!("payload encode failed: {e}")))?;
        let encrypted = nomifun_common::encrypt_string(&plaintext, &key)?;
        let row = repo.create(kind, name, &encrypted).await?;
        Ok(ConnectorCredentialSummary { id: row.id, kind: row.kind, name: row.name, created_at: row.created_at })
    }

    pub async fn delete_credential(&self, id: &str) -> Result<(), AppError> {
        let repo = self.cred_repo()?;
        repo.delete(id).await?;
        Ok(())
    }

    /// Re-probe a stored credential against its remote (the UI "test
    /// connection" action). Returns the connector identity on success.
    pub async fn test_credential(&self, id: &str) -> Result<ConnectorIdentity, AppError> {
        let cred = self.load_credential(id).await?;
        let connector = self
            .connector_for(&cred.kind)
            .ok_or_else(|| AppError::BadRequest(format!("no connector registered for kind \"{}\"", cred.kind)))?;
        connector.validate_credentials(&cred).await
    }

    /// **P3 sync orchestrator** — pull a connector-backed base's remote
    /// documents into `{root}/snapshots/*.md` (the snapshot-as-seam invariant:
    /// produces the same markdown shape the URL source does, so retrieval /
    /// mount / search stay untouched). Paginates `list_documents`, serially
    /// fetches each (the connector guards its own rate limit), compresses
    /// oversized bodies via the completer, moves vanished docs to
    /// `snapshots/_trash/`, then persists the cursor + last-sync stamp.
    pub async fn sync_connector_source(&self, kb_id: &str) -> Result<RefreshSourceSummary, AppError> {
        let mut row = self.require_base(kb_id).await?;
        let mut source = source_from_extra(&row.extra)
            .ok_or_else(|| AppError::BadRequest("knowledge base has no source to sync".into()))?;
        if source.kind == "url" {
            return Err(AppError::BadRequest(
                "URL sources use the refresh endpoint, not connector sync".into(),
            ));
        }
        let connector = self.connector_for(&source.kind).ok_or_else(|| {
            AppError::BadRequest(format!("no connector registered for kind \"{}\"", source.kind))
        })?;
        let cred_ref = source
            .credential_ref
            .clone()
            .ok_or_else(|| AppError::BadRequest("connector source is missing credential_ref".into()))?;
        let cred = self.load_credential(&cred_ref).await?;
        let scope = ConnectorScope(source.scope.clone().unwrap_or(serde_json::Value::Null));

        // Resume the incremental cursor from persisted sync state. The
        // watermark (remote max edit_time) is the real filter input; legacy
        // rows without one fall back to `last_sync_at`.
        let prev_sync = source.sync.clone().unwrap_or_default();
        let prev_watermark = prev_sync.watermark.or(prev_sync.last_sync_at);
        let cursor = SyncCursor { last_sync_at: prev_watermark, opaque: prev_sync.cursor.clone() };

        let root = PathBuf::from(&row.root_path);
        let snap_dir = root.join(source_url::SNAPSHOT_REL_DIR);

        // Phase 1 — paginate list_documents, collecting refs + tombstones.
        // A mid-pagination failure is NON-fatal: keep whatever pages we got and
        // record the error (the watermark is held below so the run is retried),
        // rather than discarding a large partial sync.
        let mut all_docs: Vec<RemoteDocRef> = Vec::new();
        let mut deleted_ids: Vec<String> = Vec::new();
        let mut page_token: Option<String> = None;
        let mut last_opaque = prev_sync.cursor.clone();
        let mut list_errored = false;
        loop {
            match connector.list_documents(&cred, &scope, &cursor, page_token.as_deref()).await {
                Ok(page) => {
                    let SyncPage { docs, deleted_ids: dels, next_page_token, updated_cursor } = page;
                    all_docs.extend(docs);
                    deleted_ids.extend(dels);
                    last_opaque = updated_cursor.opaque;
                    match next_page_token {
                        Some(tok) if !tok.is_empty() => page_token = Some(tok),
                        _ => break,
                    }
                }
                Err(e) => {
                    // The very first page failing with nothing collected is a
                    // genuine sync failure — surface it. A later page failing
                    // leaves a usable partial set.
                    if all_docs.is_empty() && deleted_ids.is_empty() {
                        return Err(e);
                    }
                    tracing::warn!(error = %e, "feishu pagination failed mid-run; syncing partial set");
                    list_errored = true;
                    break;
                }
            }
        }
        // Remote high-water-mark across ALL pages (the per-page cursor only sees
        // the last page since the same input cursor is passed each page).
        let batch_max_edit = all_docs.iter().map(|d| d.edit_time).max();

        // Phase 2 — serial fetch → compress → snapshot (deterministic slug per
        // remote_id, so deletions below can target files without frontmatter
        // matching).
        let completer = self.completer();
        let mut fetched = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut used_slugs: HashSet<String> = HashSet::new();
        for doc in &all_docs {
            let fetched_doc = match connector.fetch_document(&cred, doc).await {
                Ok(d) => d,
                Err(e) => {
                    errors.push(format!("{}: {e}", doc.title));
                    continue;
                }
            };

            let body = condense_snapshot_body(fetched_doc.markdown, completer.as_deref()).await;

            let slug_base = connector_doc_slug(&fetched_doc.remote_id);
            let mut slug = slug_base.clone();
            let mut n = 2;
            while !used_slugs.insert(slug.clone()) {
                slug = format!("{slug_base}-{n}");
                n += 1;
            }

            let fetched_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            // Prefer the canonical web URL for citation; fall back to a stable
            // synthetic id so every snapshot still carries a source_url line.
            let src_url = fetched_doc
                .source_url
                .clone()
                .unwrap_or_else(|| format!("{}://{}", source.kind, fetched_doc.remote_id));
            let content =
                source_url::snapshot_markdown(&src_url, &fetched_at, Some(&fetched_doc.title), &body);
            match write_text_atomic(&snap_dir.join(format!("{slug}.md")), &content).await {
                Ok(()) => fetched += 1,
                Err(e) => {
                    tracing::warn!(remote_id = %fetched_doc.remote_id, error = %e, "failed to write connector snapshot");
                    errors.push(format!("{}: {e}", fetched_doc.title));
                }
            }
        }

        // Phase 3 — move vanished docs to `snapshots/_trash/` (never hard-delete;
        // "the directory is the truth", users audit the trash).
        if !deleted_ids.is_empty() {
            let trash_dir = snap_dir.join("_trash");
            for id in &deleted_ids {
                let slug = connector_doc_slug(id);
                let from = snap_dir.join(format!("{slug}.md"));
                if tokio::fs::try_exists(&from).await.unwrap_or(false) {
                    if let Err(e) = tokio::fs::create_dir_all(&trash_dir).await {
                        tracing::warn!(error = %e, "failed to create connector snapshot _trash dir");
                        continue;
                    }
                    let to = trash_dir.join(format!("{slug}.md"));
                    if let Err(e) = tokio::fs::rename(&from, &to).await {
                        tracing::warn!(remote_id = id, error = %e, "failed to move deleted snapshot to _trash");
                    }
                }
            }
        }

        // Persist the cursor + watermark + last-sync stamp + any error summary.
        let had_errors = list_errored || !errors.is_empty();
        let last_error = if errors.is_empty() {
            list_errored.then(|| "pagination failed mid-run; partial sync".to_owned())
        } else {
            Some(errors.join("; "))
        };
        let last_sync_at = Some(now_ms());
        // Advance the remote watermark ONLY on a fully clean run. If any doc
        // fetch/write failed (or pagination was partial), hold the previous
        // watermark so every changed doc — including the failed ones — is
        // re-evaluated next run instead of being skipped forever.
        let watermark = if had_errors {
            prev_watermark
        } else {
            match (prev_watermark, batch_max_edit) {
                (Some(p), Some(b)) => Some(p.max(b)),
                (p, b) => b.or(p),
            }
        };
        source.sync = Some(ConnectorSyncState {
            interval_minutes: prev_sync.interval_minutes,
            last_sync_at,
            watermark,
            cursor: last_opaque,
            last_error,
        });
        // Keep the URL-source stamp coherent too (mount/info reads it).
        if fetched > 0 {
            source.last_fetched_at = last_sync_at;
        }
        self.persist_source(&mut row, &source).await?;
        let info = self.row_to_info(row).await;
        self.emitter.emit_base_updated(&info);
        Ok(RefreshSourceSummary {
            fetched,
            failed: errors.len(),
            errors,
            last_fetched_at: source.last_fetched_at,
        })
    }

    /// **P3-K3 backend selection**: pick the page-fetcher for one source entry.
    /// `rendered == true` AND a [`Self::render_fetcher`] is wired ⇒ the browser
    /// backend (`BrowserFetcher`); every other case ⇒ the default HTTP
    /// [`Self::fetcher`]. In particular `rendered == true` with **no** render
    /// backend wired (`browser-use` feature off / not injected) gracefully
    /// degrades to HTTP rather than failing — the flag is best-effort, never a
    /// hard requirement. Returns an owned `Arc` clone so the caller can `.await`
    /// across the fetch without holding the `RwLock`.
    fn fetcher_for(&self, rendered: bool) -> Arc<dyn PageFetcher> {
        if rendered && let Some(render) = self.render_fetcher() {
            render
        } else {
            Arc::clone(&self.fetcher)
        }
    }

    fn completer(&self) -> Option<Arc<dyn KnowledgeCompleter>> {
        self.completer.read().ok().and_then(|guard| guard.clone())
    }

    /// The wired completer, or the autogen-wide 409 ("configure a model
    /// provider first") shared by every AI endpoint of this crate.
    fn require_completer(&self) -> Result<Arc<dyn KnowledgeCompleter>, AppError> {
        self.completer().ok_or_else(|| {
            AppError::Conflict(
                "knowledge autogen unavailable: no AI completer is configured (add an enabled model provider first)"
                    .into(),
            )
        })
    }

    // ── Base registry ───────────────────────────────────────────────

    pub async fn list_bases(&self) -> Result<Vec<KnowledgeBaseInfo>, AppError> {
        let rows = self.repo.list_bases().await?;
        // Materialize each base concurrently (bounded), preserving registry
        // order. Sequentially, one slow/NAS-bound base's walk would block
        // materialization of every other (fast, local) base and could push the
        // whole list past the client timeout; `.buffered` caps that to roughly
        // one base's [`BASE_WALK_BUDGET`] regardless of how many bases exist.
        let infos = stream::iter(rows.into_iter().map(|row| self.row_to_info(row)))
            .buffered(LIST_BASES_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        Ok(infos)
    }

    /// Registered base ids only, straight from the registry (DB) — performs NO
    /// filesystem access. Callers that merely need to validate that an id
    /// exists (knowledge binding, `ensure_known_kb_ids`) MUST use this rather
    /// than [`Self::list_bases`], which walks every base's directory tree and
    /// would pay a full (possibly NAS-bound) walk just to produce a set of ids.
    pub async fn list_base_ids(&self) -> Result<Vec<String>, AppError> {
        Ok(self.repo.list_bases().await?.into_iter().map(|r| r.id).collect())
    }

    pub async fn get_base_info(&self, id: &str) -> Result<KnowledgeBaseInfo, AppError> {
        let row = self.require_base(id).await?;
        Ok(self.row_to_info(row).await)
    }

    /// Create a base. With `root_path = None` the directory is provisioned
    /// under `{data_dir}/knowledge/{id}/` (managed); otherwise the given
    /// existing directory is registered as-is (external reference — never
    /// structurally modified by us).
    ///
    /// An optional URL `source` is persisted into the registry row's
    /// `extra.source`. `live` mode stores it without fetching (the URLs are
    /// surfaced to agents as realtime sources at mount time); `snapshot`
    /// mode fetches every entry synchronously into `{root}/snapshots/` and
    /// then chains a best-effort AI overview run (silently skipped when no
    /// completer is wired).
    pub async fn create_base(
        &self,
        name: &str,
        description: &str,
        root_path: Option<&str>,
        source: Option<KnowledgeSource>,
    ) -> Result<KnowledgeBaseInfo, AppError> {
        let (row, info, snapshot_source) = self.register_base(name, description, root_path, source).await?;
        match snapshot_source {
            Some(src) => self.fetch_source_and_autogen(row, src).await,
            None => Ok(info),
        }
    }

    /// [`Self::create_base`] for MCP/gateway callers: identical registration
    /// (the response already carries id/seq and `extra.source` is persisted),
    /// but a snapshot-mode fetch and its chained autogen are dispatched to a
    /// background task instead of running before the response. The worst-case
    /// synchronous fetch (16 URLs × redirect hops × LLM compression) takes
    /// minutes — far beyond MCP client timeouts, after which the client gives
    /// up while the server keeps working and the agent may retry-create a
    /// duplicate. The returned info therefore never carries a `source_fetch`
    /// summary; completion is announced by the `knowledge.base-updated` event
    /// the pipeline emits when it finishes. Background failures are warn-only
    /// — the base stays registered and usable, and `refresh_source` can retry.
    pub async fn create_base_with_background_fetch(
        self: Arc<Self>,
        name: &str,
        description: &str,
        root_path: Option<&str>,
        source: Option<KnowledgeSource>,
    ) -> Result<KnowledgeBaseInfo, AppError> {
        let (row, info, snapshot_source) = self.register_base(name, description, root_path, source).await?;
        if let Some(src) = snapshot_source {
            // Same pattern as the import handler's spawned autogen
            // (`routes.rs::import_base`): the task holds its own Arc so it
            // outlives the request.
            let service = Arc::clone(&self);
            tokio::spawn(async move {
                let kb_id = row.id.clone();
                if let Err(e) = service.fetch_source_and_autogen(row, src).await {
                    tracing::warn!(kb_id, error = %e, "background knowledge source fetch failed");
                }
            });
        }
        Ok(info)
    }

    /// Shared first phase of base creation: validate, provision/verify the
    /// root directory, insert the row (with `extra.source` already
    /// persisted), and emit `knowledge.base-created`. Returns the row, its
    /// info, and — for snapshot-mode sources — the source whose fetch the
    /// caller still owes.
    async fn register_base(
        &self,
        name: &str,
        description: &str,
        root_path: Option<&str>,
        source: Option<KnowledgeSource>,
    ) -> Result<(KnowledgeBaseRow, KnowledgeBaseInfo, Option<KnowledgeSource>), AppError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::BadRequest("knowledge base name must not be empty".into()));
        }
        let mut source = source;
        if let Some(src) = &mut source {
            validate_source(src)?;
            // Server-assigned; a client-sent value would lie until the first fetch.
            src.last_fetched_at = None;
        }

        let id = generate_prefixed_id("kb");
        let (root, managed) = match root_path.map(str::trim).filter(|p| !p.is_empty()) {
            None => {
                let dir = self.data_dir.join(KB_MANAGED_REL_DIR).join(&id);
                tokio::fs::create_dir_all(&dir)
                    .await
                    .map_err(|e| AppError::Internal(format!("failed to create knowledge dir: {e}")))?;
                (dir, true)
            }
            Some(path) => {
                let dir = PathBuf::from(path);
                if !dir.is_absolute() {
                    return Err(AppError::BadRequest("external root_path must be absolute".into()));
                }
                // Off the async worker: an external root may be a slow/stale NAS
                // mount and `Path::is_dir()` is a blocking stat — running it on a
                // tokio worker thread would stall the runtime. `tokio::fs` routes
                // the stat to the blocking pool instead.
                let is_dir = tokio::fs::metadata(&dir).await.map(|m| m.is_dir()).unwrap_or(false);
                if !is_dir {
                    return Err(AppError::BadRequest(format!("directory does not exist: {path}")));
                }
                (dir, false)
            }
        };

        let extra = match &source {
            Some(src) => serde_json::json!({ "source": src }).to_string(),
            None => "{}".into(),
        };
        let now = now_ms();
        let row = KnowledgeBaseRow {
            id,
            name: name.to_owned(),
            description: description.trim().to_owned(),
            root_path: root.to_string_lossy().to_string(),
            managed,
            extra,
            created_at: now,
            updated_at: now,
            tags: None,
        };
        self.repo.insert_base(&row).await?;
        let info = self.row_to_info(row.clone()).await;
        self.emitter.emit_base_created(&info);
        // Only URL sources owe a create-time snapshot fetch. Connector-backed
        // snapshot sources (empty URL `entries`) are populated by the explicit
        // `sync_connector_source` pipeline instead, so they skip this path.
        let snapshot_source =
            source.filter(|s| s.mode == KnowledgeSourceMode::Snapshot && s.kind == "url");
        Ok((row, info, snapshot_source))
    }

    /// Boot-time resume of interrupted snapshot fetches. A snapshot-mode
    /// source is persisted into `extra.source` BEFORE its (possibly
    /// background) fetch runs, so a base whose source has entries but no
    /// `lastFetchedAt` stamp means the app exited mid-fetch (or the fetch
    /// never started). Re-run the regular fetch+autogen pipeline for each —
    /// slugs derive from the configured URLs, so a re-run overwrites in place
    /// (idempotent). Live-mode sources and already-stamped bases are never
    /// touched. Failures stay warn-only; the next boot or a manual refresh
    /// can retry.
    pub async fn resume_pending_source_fetches(self: Arc<Self>) {
        let rows = match self.repo.list_bases().await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "knowledge boot-resume: listing bases failed");
                return;
            }
        };
        let pending: Vec<(KnowledgeBaseRow, KnowledgeSource)> = rows
            .into_iter()
            .filter_map(|row| {
                let src = source_from_extra(&row.extra)?;
                (src.mode == KnowledgeSourceMode::Snapshot
                    && !src.entries.is_empty()
                    && src.last_fetched_at.is_none())
                .then_some((row, src))
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        tracing::info!(
            count = pending.len(),
            "knowledge boot-resume: re-fetching interrupted snapshot sources"
        );
        for (row, src) in pending {
            let kb_id = row.id.clone();
            if let Err(e) = self.fetch_source_and_autogen(row, src).await {
                tracing::warn!(kb_id, error = %e, "knowledge boot-resume fetch failed");
            }
        }
    }

    /// Create-time snapshot pipeline, shared by the synchronous REST create
    /// and the gateway's background dispatch: fetch every entry into
    /// `{root}/snapshots/`, persist the (title-backfilled, stamped) source,
    /// chain the best-effort autogen, then re-read + re-emit
    /// `knowledge.base-updated` so clients see the final stats/description.
    /// The returned info carries the per-entry `source_fetch` summary (the
    /// sync REST path returns it to the creating client; the background path
    /// drops it). Errs only when the final re-read fails (e.g. the base was
    /// deleted mid-run).
    async fn fetch_source_and_autogen(
        &self,
        mut row: KnowledgeBaseRow,
        mut src: KnowledgeSource,
    ) -> Result<KnowledgeBaseInfo, AppError> {
        let root = PathBuf::from(&row.root_path);
        let (fetched, errors) = self.fetch_source_snapshots(&root, &mut src.entries).await;
        if !errors.is_empty() {
            tracing::warn!(kb_id = %row.id, ?errors, "some URL sources failed to fetch at create time");
        }
        let prev_stamp = src.last_fetched_at;
        if fetched > 0 {
            src.last_fetched_at = Some(now_ms());
        }
        // The stamp the summary may honestly claim: only a PERSISTED stamp
        // counts. When persisting fails, the registry still holds the old
        // value — reporting the aspirational new stamp would lie to the
        // client about freshness state.
        let persisted_stamp = match self.persist_source(&mut row, &src).await {
            Ok(()) => src.last_fetched_at,
            Err(e) => {
                tracing::warn!(kb_id = %row.id, error = %e, "failed to persist source fetch state");
                prev_stamp
            }
        };
        // Chained creation-time autogen: best-effort, silently skipped
        // when no completer is wired (or nothing was fetched). A
        // user-supplied description is preserved — autogen only
        // backfills an empty one (the README is generated either way).
        // `None`: server-driven background curation always uses the
        // completer's default model — never a transient UI model pick.
        if fetched > 0 && self.completer().is_some() {
            if let Err(e) = self.generate_overview_opts(&row.id, false, true, None).await {
                tracing::warn!(kb_id = %row.id, error = %e, "create-time knowledge autogen skipped");
            }
        }
        // Re-read + re-emit so clients see final stats/description.
        let row = self.require_base(&row.id).await?;
        let mut info = self.row_to_info(row).await;
        self.emitter.emit_base_updated(&info);
        // Surface the per-entry fetch outcome to the creating client
        // (response-only; events/list/get never carry it).
        info.source_fetch = Some(RefreshSourceSummary {
            fetched,
            failed: errors.len(),
            errors,
            last_fetched_at: persisted_stamp,
        });
        Ok(info)
    }

    pub async fn update_base(
        &self,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
        tags: Option<Vec<String>>,
    ) -> Result<KnowledgeBaseInfo, AppError> {
        let mut row = self.require_base(id).await?;
        if let Some(name) = name {
            let name = name.trim();
            if name.is_empty() {
                return Err(AppError::BadRequest("knowledge base name must not be empty".into()));
            }
            row.name = name.to_owned();
        }
        if let Some(description) = description {
            row.description = description.trim().to_owned();
        }
        if let Some(ref tag_keys) = tags {
            row.tags = if tag_keys.is_empty() {
                None
            } else {
                Some(serde_json::to_string(tag_keys).unwrap())
            };
        }
        row.updated_at = now_ms();
        self.repo.update_base(&row).await?;
        let info = self.row_to_info(row).await;
        self.emitter.emit_base_updated(&info);
        Ok(info)
    }

    /// Delete a base registration. `purge` additionally removes the files on
    /// disk — allowed only for managed bases (the guard re-checks the path is
    /// really under `{data_dir}/knowledge/` so a corrupted row can never
    /// point a purge at user data).
    pub async fn delete_base(&self, id: &str, purge: bool) -> Result<(), AppError> {
        let row = self.require_base(id).await?;
        self.repo.delete_base(id).await?;

        if purge && row.managed {
            let root = PathBuf::from(&row.root_path);
            let managed_parent = self.data_dir.join(KB_MANAGED_REL_DIR);
            if root.starts_with(&managed_parent) && root != managed_parent {
                if let Err(e) = tokio::fs::remove_dir_all(&root).await {
                    tracing::warn!(path = %root.display(), error = %e, "failed to purge knowledge base dir");
                }
            } else {
                tracing::warn!(path = %root.display(), "purge refused: root not under managed knowledge dir");
            }
        }
        self.emitter.emit_base_deleted(id);
        Ok(())
    }

    // ── File access (md only, directory is the source of truth) ─────

    pub async fn list_files(&self, id: &str) -> Result<Vec<KbFileEntry>, AppError> {
        let row = self.require_base(id).await?;
        let root = PathBuf::from(&row.root_path);
        // Bounded so a slow/stale NAS root degrades to an empty listing instead
        // of hanging the detail view (and the agent write-path collision check).
        Ok(bounded_blocking(BASE_WALK_BUDGET, Vec::new(), move || list_md_files(&root)).await)
    }

    pub async fn list_tree(&self, id: &str, rel_path: &str) -> Result<Vec<KbTreeEntry>, AppError> {
        let row = self.require_base(id).await?;
        let root = PathBuf::from(&row.root_path);
        let rel_path = normalize_tree_rel_path(rel_path)?;
        Ok(bounded_blocking(BASE_WALK_BUDGET, Ok(Vec::new()), move || {
            list_tree_level(&root, &rel_path)
        })
        .await?)
    }

    pub async fn create_folder(&self, id: &str, rel_path: &str) -> Result<KbTreeEntry, AppError> {
        let row = self.require_base(id).await?;
        let root = PathBuf::from(&row.root_path);
        let rel_path = normalize_tree_rel_path(rel_path)?;
        if rel_path.is_empty() {
            return Err(AppError::BadRequest("folder path must not be empty".into()));
        }
        tokio::task::spawn_blocking(move || create_tree_folder(&root, &rel_path))
            .await
            .map_err(|e| AppError::Internal(format!("folder create task join error: {e}")))?
    }

    pub async fn delete_folder(&self, id: &str, rel_path: &str) -> Result<(), AppError> {
        let row = self.require_base(id).await?;
        let root = PathBuf::from(&row.root_path);
        let rel_path = normalize_tree_rel_path(rel_path)?;
        if rel_path.is_empty() {
            return Err(AppError::BadRequest("folder path must not be empty".into()));
        }
        tokio::task::spawn_blocking(move || delete_tree_folder(&root, &rel_path))
            .await
            .map_err(|e| AppError::Internal(format!("folder delete task join error: {e}")))?
    }

    pub async fn rename_tree_entry(&self, id: &str, rel_path: &str, new_name: &str) -> Result<KbTreeEntry, AppError> {
        let row = self.require_base(id).await?;
        let root = PathBuf::from(&row.root_path);
        let rel_path = normalize_tree_rel_path(rel_path)?;
        if rel_path.is_empty() {
            return Err(AppError::BadRequest("path must not be empty".into()));
        }
        let new_name = validate_tree_entry_name(new_name)?;
        tokio::task::spawn_blocking(move || rename_tree_entry(&root, &rel_path, &new_name))
            .await
            .map_err(|e| AppError::Internal(format!("tree rename task join error: {e}")))?
    }

    pub async fn read_file(&self, id: &str, rel_path: &str) -> Result<KbFileContent, AppError> {
        let row = self.require_base(id).await?;
        let path = safe_md_path(Path::new(&row.root_path), rel_path)?;
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|_| AppError::NotFound(format!("file not found: {rel_path}")))?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| AppError::Internal(format!("failed to read file: {e}")))?;
        Ok(KbFileContent {
            rel_path: rel_path.replace('\\', "/"),
            content,
            size: meta.len(),
            modified_at: modified_ms(&meta),
        })
    }

    /// Create or overwrite a markdown file (atomic temp + rename).
    pub async fn write_file(&self, id: &str, rel_path: &str, content: &str) -> Result<(), AppError> {
        let row = self.require_base(id).await?;
        let path = safe_md_path(Path::new(&row.root_path), rel_path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| AppError::Internal(format!("failed to create parent dirs: {e}")))?;
        }
        let tmp = atomic_tmp_path(&path);
        tokio::fs::write(&tmp, content)
            .await
            .map_err(|e| AppError::Internal(format!("failed to write file: {e}")))?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| AppError::Internal(format!("failed to finalize file: {e}")))?;
        Ok(())
    }

    // ── P4 inbox review (staged write-back proposals) ─────────────────

    /// List staged write-back proposals under `_inbox/{scope}/…` (the panel
    /// groups them by `scope` client-side).
    pub async fn list_inbox(&self, id: &str) -> Result<Vec<InboxEntry>, AppError> {
        let row = self.require_base(id).await?;
        let root = PathBuf::from(&row.root_path);
        tokio::task::spawn_blocking(move || list_inbox_entries(&root))
            .await
            .map_err(|e| AppError::Internal(format!("inbox list task join error: {e}")))
    }

    /// Unified diff of a staged proposal vs. the current base document
    /// (full-content "new document" when the base file does not yet exist).
    pub async fn inbox_diff(&self, id: &str, scope: &str, rel_path: &str) -> Result<InboxDiff, AppError> {
        let row = self.require_base(id).await?;
        let root = PathBuf::from(&row.root_path);
        validate_inbox_scope(scope)?;
        let inbox_abs = safe_md_path(&root, &format!("{KB_INBOX_REL_DIR}/{scope}/{rel_path}"))?;
        let inbox_content = tokio::fs::read_to_string(&inbox_abs)
            .await
            .map_err(|_| AppError::NotFound(format!("inbox proposal not found: {scope}/{rel_path}")))?;
        let base_abs = safe_md_path(&root, rel_path)?;
        let base_content = tokio::fs::read_to_string(&base_abs).await.ok();
        let is_new = base_content.is_none();
        let unified_diff = unified_md_diff(base_content.as_deref().unwrap_or(""), &inbox_content, rel_path);
        Ok(InboxDiff {
            scope: scope.to_owned(),
            rel_path: rel_path.replace('\\', "/"),
            inbox_content,
            base_content,
            unified_diff,
            is_new,
        })
    }

    /// Accept a staged proposal: overwrite the base document with it, delete
    /// the inbox copy, prune emptied scope dirs, and emit `base-updated`.
    /// Idempotent across a mid-merge crash (re-running overwrites identically).
    pub async fn merge_inbox(&self, id: &str, scope: &str, rel_path: &str) -> Result<InboxMergeResult, AppError> {
        let row = self.require_base(id).await?;
        let root = PathBuf::from(&row.root_path);
        validate_inbox_scope(scope)?;
        let inbox_abs = safe_md_path(&root, &format!("{KB_INBOX_REL_DIR}/{scope}/{rel_path}"))?;
        let content = tokio::fs::read_to_string(&inbox_abs)
            .await
            .map_err(|_| AppError::NotFound(format!("inbox proposal not found: {scope}/{rel_path}")))?;
        // Overwrite the base body first; only then drop the staged copy, so a
        // crash in between leaves the proposal recoverable (re-merge is a no-op).
        self.write_file(id, rel_path, &content).await?;
        let _ = tokio::fs::remove_file(&inbox_abs).await;
        if let Some(parent) = inbox_abs.parent() {
            prune_empty_inbox_dirs(&root.join(KB_INBOX_REL_DIR), parent.to_path_buf()).await;
        }
        let info = self.row_to_info(row).await;
        self.emitter.emit_base_updated(&info);
        Ok(InboxMergeResult { merged_path: rel_path.replace('\\', "/") })
    }

    /// Discard a staged proposal (delete the inbox copy + prune emptied dirs).
    pub async fn discard_inbox(&self, id: &str, scope: &str, rel_path: &str) -> Result<(), AppError> {
        let row = self.require_base(id).await?;
        let root = PathBuf::from(&row.root_path);
        validate_inbox_scope(scope)?;
        let inbox_abs = safe_md_path(&root, &format!("{KB_INBOX_REL_DIR}/{scope}/{rel_path}"))?;
        tokio::fs::remove_file(&inbox_abs)
            .await
            .map_err(|_| AppError::NotFound(format!("inbox proposal not found: {scope}/{rel_path}")))?;
        if let Some(parent) = inbox_abs.parent() {
            prune_empty_inbox_dirs(&root.join(KB_INBOX_REL_DIR), parent.to_path_buf()).await;
        }
        Ok(())
    }

    /// Accept all staged proposals for a base (optionally filtered by scope).
    /// Returns the number of proposals merged.
    pub async fn merge_all_inbox(&self, id: &str, scope: Option<&str>) -> Result<usize, AppError> {
        let entries = self.list_inbox(id).await?;
        let filtered: Vec<_> = entries
            .into_iter()
            .filter(|e| scope.map_or(true, |s| e.scope == s))
            .collect();
        let count = filtered.len();
        for entry in filtered {
            self.merge_inbox(id, &entry.scope, &entry.rel_path).await?;
        }
        Ok(count)
    }

    /// Discard all staged proposals for a base (optionally filtered by scope).
    /// Returns the number of proposals discarded.
    pub async fn discard_all_inbox(&self, id: &str, scope: Option<&str>) -> Result<usize, AppError> {
        let entries = self.list_inbox(id).await?;
        let filtered: Vec<_> = entries
            .into_iter()
            .filter(|e| scope.map_or(true, |s| e.scope == s))
            .collect();
        let count = filtered.len();
        for entry in filtered {
            self.discard_inbox(id, &entry.scope, &entry.rel_path).await?;
        }
        Ok(count)
    }

    /// Bindings currently mounting this base (enabled AND disabled — the UI
    /// greys the disabled ones). Powers the "who is using this base?" view.
    pub async fn list_consumers(&self, id: &str) -> Result<Vec<ConsumerInfo>, AppError> {
        self.require_base(id).await?;
        let rows = self.repo.list_bindings_using_kb(id).await?;
        Ok(rows
            .into_iter()
            .map(|r| ConsumerInfo { target_kind: r.target_kind.clone(), target_id: r.target_id(), enabled: r.enabled })
            .collect())
    }

    /// Total staged-proposal count across every base (powers the sidebar
    /// "unreviewed" red dot). Walks each base's `_inbox/` off the async
    /// runtime; `0` means nothing awaits review.
    pub async fn count_pending_inbox(&self) -> Result<usize, AppError> {
        let rows = self.repo.list_bases().await?;
        let roots: Vec<PathBuf> = rows.iter().map(|r| PathBuf::from(&r.root_path)).collect();
        // Bounded: this backs the app-wide sidebar red-dot (fires on every page
        // load), so a single slow/stale NAS base's `_inbox` walk must degrade to
        // 0 rather than stall unrelated navigation.
        Ok(bounded_blocking(BASE_WALK_BUDGET, 0usize, move || {
            roots.iter().map(|root| list_inbox_entries(root).len()).sum()
        })
        .await)
    }

    /// Resolve a model-supplied write target to a canonical document + op.
    /// `bound_kb_ids` scopes what the session may write to — a handle or path
    /// pointing outside it is `Forbidden`. A `Path` whose basename matches
    /// exactly one existing file elsewhere returns a `Conflict` suggesting the
    /// handle, rather than silently creating a duplicate (the wrong-folder bug).
    pub async fn resolve_write_target(
        &self,
        bound_kb_ids: &[String],
        spec: &WriteTargetSpec,
    ) -> Result<WriteResolution, AppError> {
        match spec {
            WriteTargetSpec::Handle(handle) => {
                let (kb_id, rel_path) = decode_doc_handle(handle)
                    .ok_or_else(|| AppError::BadRequest(format!("invalid document handle: {handle}")))?;
                if !bound_kb_ids.iter().any(|b| b == &kb_id) {
                    return Err(AppError::Forbidden("handle points to a base not mounted in this session".into()));
                }
                let row = self.require_base(&kb_id).await?;
                let abs = safe_md_path(Path::new(&row.root_path), &rel_path)?;
                if !tokio::fs::try_exists(&abs).await.unwrap_or(false) {
                    return Err(AppError::NotFound(format!("document for handle no longer exists: {rel_path}")));
                }
                Ok(WriteResolution { kb_id, canonical_rel_path: rel_path, op: WriteOp::Update })
            }
            WriteTargetSpec::Path { kb_id, rel_path } => {
                if !bound_kb_ids.iter().any(|b| b == kb_id) {
                    return Err(AppError::Forbidden("target base is not mounted in this session".into()));
                }
                let canonical = deconfuse_rel_path(rel_path);
                let row = self.require_base(kb_id).await?;
                let abs = safe_md_path(Path::new(&row.root_path), &canonical)?;
                if tokio::fs::try_exists(&abs).await.unwrap_or(false) {
                    return Ok(WriteResolution { kb_id: kb_id.clone(), canonical_rel_path: canonical, op: WriteOp::Update });
                }
                // No exact match: a unique basename match elsewhere is almost
                // certainly the intended update — suggest the handle, don't dupe.
                let basename = canonical.rsplit('/').next().unwrap_or(&canonical).to_owned();
                let files = self.list_files(kb_id).await.unwrap_or_default();
                let collisions: Vec<&KbFileEntry> = files
                    .iter()
                    .filter(|f| f.rel_path.rsplit('/').next().is_some_and(|n| n.eq_ignore_ascii_case(&basename)))
                    .collect();
                if collisions.len() == 1 {
                    let existing = &collisions[0].rel_path;
                    let handle = encode_doc_handle(kb_id, existing);
                    return Err(AppError::Conflict(format!(
                        "No document at \"{canonical}\". A document named \"{basename}\" already exists at \"{existing}\". \
                         To UPDATE it, call knowledge_write with handle=\"{handle}\". To create a new file, choose a distinct rel_path."
                    )));
                }
                Ok(WriteResolution { kb_id: kb_id.clone(), canonical_rel_path: canonical, op: WriteOp::Create })
            }
        }
    }

    /// The single canonical agent write path. Resolves the target (fixing
    /// mount-path confusion / locating the existing doc), enforces the policy
    /// (disabled refused, create gated), applies placement (direct = base body;
    /// staged = `_inbox/{scope}/{rel_path}` mirroring the original, which is left
    /// untouched), and writes atomically. All agent surfaces funnel here.
    pub async fn write_document(&self, req: WriteRequest) -> Result<WriteOutcome, AppError> {
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("refusing to write empty knowledge content".into()));
        }
        if matches!(req.policy.mode, WriteMode::Disabled) {
            return Err(AppError::Forbidden("write-back is disabled for this session".into()));
        }
        let res = self.resolve_write_target(&req.bound_kb_ids, &req.spec).await?;
        if res.op == WriteOp::Create && !req.policy.allow_create {
            return Err(AppError::Forbidden("creating new knowledge documents is not allowed for this session".into()));
        }
        let (final_rel_path, staged) = match &req.policy.mode {
            WriteMode::Staged { scope } => (
                format!("{KB_INBOX_REL_DIR}/{}/{}", scope.trim_matches('/'), res.canonical_rel_path),
                true,
            ),
            WriteMode::Direct => (res.canonical_rel_path.clone(), false),
            WriteMode::Disabled => unreachable!("disabled handled above"),
        };
        self.write_file(&res.kb_id, &final_rel_path, &req.content).await?;
        Ok(WriteOutcome { kb_id: res.kb_id, final_rel_path, op: res.op, staged })
    }

    pub async fn delete_file(&self, id: &str, rel_path: &str) -> Result<(), AppError> {
        let row = self.require_base(id).await?;
        let path = safe_md_path(Path::new(&row.root_path), rel_path)?;
        tokio::fs::remove_file(&path)
            .await
            .map_err(|_| AppError::NotFound(format!("file not found: {rel_path}")))?;
        Ok(())
    }

    // ── AI autogen & URL sources ────────────────────────────────────

    /// Generate (LLM) and persist a registry description + root `README.md`
    /// for the base. With `overwrite_readme = false` an existing README is
    /// left untouched (the description is still refreshed). `model_override`
    /// pins an explicit `(provider_id, model)` for the LLM call (the
    /// knowledge UI's per-run model picker); `None` uses the completer's
    /// default model. Completion is announced via the regular
    /// `knowledge.base-updated` event.
    pub async fn generate_overview(
        &self,
        kb_id: &str,
        overwrite_readme: bool,
        model_override: Option<(String, String)>,
    ) -> Result<AutogenOutcome, AppError> {
        self.generate_overview_opts(kb_id, overwrite_readme, false, model_override).await
    }

    /// [`Self::generate_overview`] with one extra knob:
    /// `preserve_existing_description` keeps a non-empty registry
    /// description as-is (used by the post-import hook, which must only
    /// backfill missing descriptions). `model_override` is threaded through
    /// to the LLM call exactly as in [`Self::generate_overview`].
    pub async fn generate_overview_opts(
        &self,
        kb_id: &str,
        overwrite_readme: bool,
        preserve_existing_description: bool,
        model_override: Option<(String, String)>,
    ) -> Result<AutogenOutcome, AppError> {
        let completer = self.require_completer()?;
        let row = self.require_base(kb_id).await?;
        let root = PathBuf::from(&row.root_path);
        let samples = autogen::sample_base_files(&root).await;
        if samples.is_empty() {
            return Err(AppError::BadRequest(
                "knowledge base has no markdown documents to summarize".into(),
            ));
        }

        let user = autogen::build_overview_prompt(&row.name, &row.description, &samples);
        // One retry on parse failure (the model occasionally wraps in prose);
        // provider failures propagate immediately.
        let mut parsed = None;
        let mut last_err = String::new();
        for attempt in 0..2 {
            let raw =
                complete_overview(completer.as_ref(), &user, model_override.as_ref()).await?;
            match autogen::parse_overview_output(&raw) {
                Ok(output) => {
                    parsed = Some(output);
                    break;
                }
                Err(e) => {
                    last_err = e;
                    tracing::debug!(attempt, kb_id, error = %last_err, "knowledge overview output unparseable");
                }
            }
        }
        let Some(output) = parsed else {
            return Err(AppError::BadGateway(format!(
                "knowledge autogen output unparseable: {last_err}"
            )));
        };

        let description = autogen::clamp_description(&output.description);
        let readme = output.readme_markdown.trim();

        let mut readme_written = false;
        if !readme.is_empty() {
            // Case-insensitive existence check: on case-sensitive filesystems
            // an existing `readme.md` must count as "README exists" (and be
            // overwritten in place) — never get a parallel `README.md`.
            let existing = find_readme_path(&root).await;
            if overwrite_readme || existing.is_none() {
                let readme_path = existing.unwrap_or_else(|| root.join("README.md"));
                write_text_atomic(&readme_path, &format!("{readme}\n")).await?;
                readme_written = true;
            }
        }

        let keep_description =
            description.is_empty() || (preserve_existing_description && !row.description.trim().is_empty());
        let description_arg = (!keep_description).then_some(description.as_str());
        // `update_base` re-emits `knowledge.base-updated` (also bumps
        // updated_at when only the README changed).
        let base = self.update_base(kb_id, None, description_arg, None).await?;
        Ok(AutogenOutcome {
            description: base.description.clone(),
            description_updated: description_arg.is_some(),
            readme_written,
            base,
        })
    }

    /// Stateless companion of [`Self::generate_overview_opts`] for the
    /// create-base form: sample an arbitrary on-disk directory and generate a
    /// registry description only — no README, nothing persisted, no events
    /// (the base may not exist yet). `name` may be blank. `model_override`
    /// pins an explicit `(provider_id, model)` for the call; `None` uses the
    /// completer's default. Validation mirrors `register_base`'s external
    /// root_path rules (absolute + existing dir).
    pub async fn generate_description_for_path(
        &self,
        name: &str,
        root_path: &str,
        model_override: Option<(String, String)>,
    ) -> Result<String, AppError> {
        let completer = self.require_completer()?;
        let root_path = root_path.trim();
        if root_path.is_empty() {
            return Err(AppError::BadRequest("root_path must not be empty".into()));
        }
        let root = PathBuf::from(root_path);
        if !root.is_absolute() {
            return Err(AppError::BadRequest("root_path must be absolute".into()));
        }
        if !root.is_dir() {
            return Err(AppError::BadRequest(format!("directory does not exist: {root_path}")));
        }
        let samples = autogen::sample_base_files(&root).await;
        if samples.is_empty() {
            return Err(AppError::BadRequest(
                "directory has no markdown documents to summarize".into(),
            ));
        }
        let user = autogen::build_description_prompt(name, &samples);
        complete_description(
            completer.as_ref(),
            autogen::DESCRIPTION_SYSTEM,
            &user,
            model_override.as_ref(),
        )
        .await
    }

    /// Stateless: rewrite a user-typed draft into a polished registry
    /// description. Nothing is persisted — the caller decides what to do
    /// with the result. `name` may be blank. `model_override` pins an
    /// explicit `(provider_id, model)`; `None` uses the completer's default.
    pub async fn polish_description(
        &self,
        name: &str,
        draft: &str,
        model_override: Option<(String, String)>,
    ) -> Result<String, AppError> {
        let completer = self.require_completer()?;
        let draft = draft.trim();
        if draft.is_empty() {
            return Err(AppError::BadRequest("draft must not be empty".into()));
        }
        let user = autogen::build_polish_prompt(name, draft);
        complete_description(
            completer.as_ref(),
            autogen::POLISH_SYSTEM,
            &user,
            model_override.as_ref(),
        )
        .await
    }

    /// Re-fetch every URL-source entry into `{root}/snapshots/` (overwriting
    /// older snapshots) and stamp `extra.source.last_fetched_at` when at
    /// least one entry was fetched. Works for both snapshot- and live-mode
    /// sources (a live base gains/refreshes its point-in-time snapshots
    /// without changing its realtime contract).
    pub async fn refresh_source(&self, kb_id: &str) -> Result<RefreshSourceSummary, AppError> {
        let mut row = self.require_base(kb_id).await?;
        let mut source = source_from_extra(&row.extra)
            .ok_or_else(|| AppError::BadRequest("knowledge base has no URL source to refresh".into()))?;
        if source.entries.is_empty() {
            return Err(AppError::BadRequest("URL source has no entries".into()));
        }

        let root = PathBuf::from(&row.root_path);
        let (fetched, errors) = self.fetch_source_snapshots(&root, &mut source.entries).await;
        // Align with the create path: only a run that actually wrote a
        // snapshot may claim a fresh stamp — after a fully-failed refresh
        // the snapshots on disk are still the old ones.
        if fetched > 0 {
            source.last_fetched_at = Some(now_ms());
        }
        self.persist_source(&mut row, &source).await?;
        // The entry list may have shrunk since the snapshots were written —
        // sweep snapshot files whose frontmatter source_url no longer matches
        // any configured entry. User-authored files (no source_url
        // frontmatter) are never touched.
        prune_orphan_snapshots(&root, &source.entries).await;
        let last_fetched_at = source.last_fetched_at;
        let info = self.row_to_info(row).await;
        self.emitter.emit_base_updated(&info);
        Ok(RefreshSourceSummary {
            fetched,
            failed: errors.len(),
            errors,
            last_fetched_at,
        })
    }

    /// Fetch every entry and write `{root}/snapshots/{slug}.md` (frontmatter
    /// + markdown body). Per-entry failures are collected, never fatal.
    /// Pages larger than the compression threshold are condensed via the
    /// completer when one is wired (raw-but-truncated otherwise). Entries
    /// without a title are backfilled from the page `<title>`.
    ///
    /// The network/LLM work runs concurrently ([`SOURCE_FETCH_CONCURRENCY`]
    /// at a time); slug assignment, title backfill and the disk writes then
    /// happen serially in entry order, so duplicate-slug numbering and error
    /// aggregation stay deterministic regardless of completion order.
    async fn fetch_source_snapshots(
        &self,
        root: &Path,
        entries: &mut [KnowledgeSourceEntry],
    ) -> (usize, Vec<String>) {
        let completer = self.completer();
        let snap_dir = root.join(source_url::SNAPSHOT_REL_DIR);

        // Phase 1 — fetch (and condense oversized pages) concurrently,
        // re-indexed by entry position. The futures own their URL (and are
        // collected eagerly) so the stream type stays free of per-entry
        // borrows — axum handlers need the whole call graph to be Send.
        let fetches: Vec<_> = entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let url = entry.url.clone();
                let rendered = entry.rendered;
                let completer = completer.clone();
                async move { (idx, self.prepare_snapshot_body(&url, rendered, completer.as_deref()).await) }
            })
            .collect();
        let results = stream::iter(fetches)
            .buffer_unordered(SOURCE_FETCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        let mut prepared: Vec<Option<Result<PreparedSnapshot, String>>> =
            entries.iter().map(|_| None).collect();
        for (idx, result) in results {
            prepared[idx] = Some(result);
        }

        // Phase 2 — serial, in entry order.
        let mut used_slugs: HashSet<String> = HashSet::new();
        let mut fetched = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for (entry, result) in entries.iter_mut().zip(prepared) {
            let page = match result.expect("every entry yields exactly one fetch result") {
                Ok(page) => page,
                Err(line) => {
                    errors.push(line);
                    continue;
                }
            };

            // Slug derives from the configured URL (stable across redirects);
            // duplicate slugs within one batch get a numeric suffix.
            let slug_base = Url::parse(entry.url.trim())
                .map(|u| source_url::slug_for_url(&u))
                .unwrap_or_else(|_| "page".into());
            let mut slug = slug_base.clone();
            let mut n = 2;
            while !used_slugs.insert(slug.clone()) {
                slug = format!("{slug_base}-{n}");
                n += 1;
            }

            if entry.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).is_none()
                && let Some(title) = &page.title
            {
                entry.title = Some(title.clone());
            }

            let fetched_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let content = source_url::snapshot_markdown(&entry.url, &fetched_at, entry.title.as_deref(), &page.body);
            match write_text_atomic(&snap_dir.join(format!("{slug}.md")), &content).await {
                Ok(()) => fetched += 1,
                Err(e) => {
                    tracing::warn!(url = %entry.url, error = %e, "failed to write knowledge snapshot");
                    errors.push(format!("{}: {e}", entry.url));
                }
            }
        }
        (fetched, errors)
    }

    /// Fetch one source URL and condense/truncate the body to snapshot size.
    /// Errors come back as the ready-to-aggregate `"{url}: {error}"` line.
    ///
    /// **P3-K3 backend selection**: when `rendered` is set AND a rendering
    /// backend is wired ([`Self::render_fetcher`], the engine-backed
    /// `BrowserFetcher`), the URL is fetched through the real browser so JS-heavy
    /// pages yield their post-render content. Otherwise — `rendered == false`, or
    /// `rendered == true` but no browser backend is available (`browser-use`
    /// feature off / not injected) — it gracefully falls back to the default HTTP
    /// [`Self::fetcher`] (no error: a missing browser backend degrades to HTTP,
    /// never blocks the snapshot). See [`Self::fetcher_for`].
    async fn prepare_snapshot_body(
        &self,
        url: &str,
        rendered: bool,
        completer: Option<&dyn KnowledgeCompleter>,
    ) -> Result<PreparedSnapshot, String> {
        let fetcher = self.fetcher_for(rendered);
        let page = fetcher.fetch_page(url).await.map_err(|e| {
            tracing::warn!(url, rendered, error = %e, "knowledge source fetch failed");
            format!("{url}: {e}")
        })?;

        let body = condense_snapshot_body(page.markdown, completer).await;
        Ok(PreparedSnapshot {
            title: page.title,
            body,
        })
    }

    /// Write `source` back into the row's `extra.source` and persist the row
    /// (other `extra` keys are preserved).
    async fn persist_source(&self, row: &mut KnowledgeBaseRow, source: &KnowledgeSource) -> Result<(), AppError> {
        let mut extra: serde_json::Value =
            serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        if !extra.is_object() {
            extra = serde_json::json!({});
        }
        extra["source"] =
            serde_json::to_value(source).map_err(|e| AppError::Internal(format!("source serialize failed: {e}")))?;
        row.extra = extra.to_string();
        row.updated_at = now_ms();
        self.repo.update_base(row).await?;
        Ok(())
    }

    /// Attach, replace, or clear a base's source config (`extra.source`).
    /// `Some(src)` validates + persists it (server clears any client-sent
    /// `last_fetched_at`); `None` removes the source. Used to wire a connector
    /// (Feishu, …) onto an existing base, or to detach one. Emits
    /// `base-updated`. Does NOT fetch/sync — callers trigger
    /// `sync_connector_source` / `refresh_source` afterward.
    pub async fn set_source(
        &self,
        kb_id: &str,
        source: Option<KnowledgeSource>,
    ) -> Result<KnowledgeBaseInfo, AppError> {
        let mut row = self.require_base(kb_id).await?;
        match source {
            Some(mut src) => {
                validate_source(&src)?;
                src.last_fetched_at = None;
                self.persist_source(&mut row, &src).await?;
            }
            None => {
                let mut extra: serde_json::Value =
                    serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
                if let Some(obj) = extra.as_object_mut() {
                    obj.remove("source");
                }
                row.extra = extra.to_string();
                row.updated_at = now_ms();
                self.repo.update_base(&row).await?;
            }
        }
        let info = self.row_to_info(row).await;
        self.emitter.emit_base_updated(&info);
        Ok(info)
    }

    // ── Bindings & mounting ─────────────────────────────────────────

    pub async fn get_binding(&self, kind: &str, target_id: &str) -> Result<KnowledgeBinding, AppError> {
        validate_kind(kind)?;
        let target_id = canonical_target_id(kind, target_id);
        let row = self.repo.get_binding(kind, &target_id).await?;
        Ok(row.map(binding_from_row).unwrap_or_default())
    }

    pub async fn set_binding(
        &self,
        kind: &str,
        target_id: &str,
        binding: KnowledgeBinding,
    ) -> Result<KnowledgeBinding, AppError> {
        validate_kind(kind)?;
        let target_id = canonical_target_id(kind, target_id);
        if target_id.trim().is_empty() {
            return Err(AppError::BadRequest("target_id must not be empty".into()));
        }
        if !WRITEBACK_MODES.contains(&binding.writeback_mode.as_str()) {
            return Err(AppError::BadRequest(format!(
                "unsupported writeback_mode: {}",
                binding.writeback_mode
            )));
        }
        if !WRITEBACK_EAGERNESS.contains(&binding.writeback_eagerness.as_str()) {
            return Err(AppError::BadRequest(format!(
                "unsupported writeback_eagerness: {}",
                binding.writeback_eagerness
            )));
        }
        self.repo
            .set_binding(
                kind,
                &target_id,
                &binding.kb_ids,
                binding.enabled,
                binding.writeback,
                &binding.writeback_mode,
                &binding.writeback_eagerness,
                binding.channel_write_enabled,
                now_ms(),
            )
            .await?;
        self.emitter.emit_binding_changed(&serde_json::json!({
            "target_kind": kind,
            "target_id": target_id,
            "enabled": binding.enabled,
            "writeback": binding.writeback,
            "writeback_mode": binding.writeback_mode,
            "writeback_eagerness": binding.writeback_eagerness,
            "channel_write_enabled": binding.channel_write_enabled,
            "kb_ids": binding.kb_ids,
        }));
        Ok(binding)
    }

    /// Remove a target's knowledge binding row entirely. For cleanup when the
    /// target itself goes away (e.g. a deleted companion → `("companion", companion_id)`);
    /// mirrors the conversation-delete hook below. Deleting a missing row is
    /// a no-op.
    pub async fn delete_binding(&self, kind: &str, target_id: &str) -> Result<(), AppError> {
        validate_kind(kind)?;
        let target_id = canonical_target_id(kind, target_id);
        self.repo.delete_binding(kind, &target_id).await?;
        Ok(())
    }

    /// Workpath-first mount resolution for conversation/terminal sessions
    /// (session-list unification spec §7): the binding belongs to the
    /// workspace path, not to the individual session. Looks up the
    /// `('workpath', workpath_key)` binding ROW first; only when no such
    /// row exists at all does it fall back to the legacy per-session
    /// binding `(legacy_kind, legacy_id)`, so pre-workpath local data keeps
    /// mounting. An existing-but-disabled workpath row is an explicit user
    /// choice — NOT a miss — and shadows the legacy binding. Companion sessions
    /// never come through here (they keep `ensure_mounts_for_target` with
    /// `('companion', companion_id)`).
    pub async fn ensure_mounts_for_session(
        &self,
        workpath: &str,
        legacy_kind: &str,
        legacy_id: &str,
        workspace: &Path,
    ) -> MountOutcome {
        let key = workpath_key(workpath);
        let has_workpath_row = match self.repo.get_binding(WORKPATH_BINDING_KIND, &key).await {
            Ok(row) => row.is_some(),
            Err(e) => {
                tracing::warn!(error = %e, workpath = %key, "workpath knowledge binding lookup failed");
                false
            }
        };
        if has_workpath_row {
            self.ensure_mounts_for_target(WORKPATH_BINDING_KIND, &key, workspace).await
        } else {
            self.ensure_mounts_for_target(legacy_kind, legacy_id, workspace).await
        }
    }

    /// Synchronize workspace mounts for a target according to its binding.
    /// Deleted/missing bases are skipped (no FK by design); a disabled or
    /// empty binding clears previously created mounts. Never fails the
    /// session start — errors degrade to an empty outcome with warnings.
    pub async fn ensure_mounts_for_target(&self, kind: &str, target_id: &str, workspace: &Path) -> MountOutcome {
        // Safety guard: when the workspace is the backend data root (or one
        // of its ancestors), the mount sync / legacy cleanup would run their
        // destructive sweeps over the directory tree that CONTAINS the
        // managed knowledge bases. Skip mounting entirely for such targets.
        if self.workspace_overlaps_managed_root(workspace) {
            tracing::warn!(
                kind,
                target_id,
                workspace = %workspace.display(),
                "knowledge mounts skipped: workspace overlaps the backend data root"
            );
            return MountOutcome::default();
        }
        let binding = match self.get_binding(kind, target_id).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, kind, target_id, "knowledge binding lookup failed");
                return MountOutcome::default();
            }
        };

        let mut specs: Vec<MountSpec> = Vec::new();
        let mut metas: Vec<(String, KnowledgeBaseRow)> = Vec::new();
        if binding.enabled {
            let mut used_names: HashSet<String> = HashSet::new();
            for kb_id in &binding.kb_ids {
                let row = match self.repo.get_base(kb_id).await {
                    Ok(Some(row)) => row,
                    Ok(None) => {
                        tracing::warn!(kb_id, "bound knowledge base no longer exists; skipping");
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(kb_id, error = %e, "knowledge base lookup failed; skipping");
                        continue;
                    }
                };
                let link_name = unique_link_name(&row, &mut used_names);
                specs.push(MountSpec {
                    link_name: link_name.clone(),
                    target: PathBuf::from(&row.root_path),
                });
                metas.push((link_name, row));
            }
        }

        let present = mount::sync_mounts(workspace, specs).await;
        let present: HashSet<String> = present.into_iter().collect();

        let kept: Vec<(String, KnowledgeBaseRow)> = metas
            .into_iter()
            .filter(|(link_name, _)| present.contains(link_name))
            .collect();

        // Full per-base listings first, then the shared per-KB/global budget
        // (`context::apply_toc_budgets`) so the prompt cost stays bounded no
        // matter how many bases are mounted.
        let mut tocs: Vec<Vec<String>> = Vec::with_capacity(kept.len());
        for (_, row) in &kept {
            tocs.push(build_toc(Path::new(&row.root_path)).await);
        }
        crate::context::apply_toc_budgets(&mut tocs);

        let mut mounts = Vec::with_capacity(kept.len());
        for ((link_name, row), toc) in kept.into_iter().zip(tocs) {
            let summary = read_base_summary(Path::new(&row.root_path)).await;
            // Live-mode URL sources surface as realtime URLs in the context.
            let live_sources = source_from_extra(&row.extra)
                .filter(|s| s.mode == KnowledgeSourceMode::Live)
                .map(|s| s.entries)
                .unwrap_or_default();
            mounts.push(KnowledgeMountInfo {
                id: row.id,
                name: row.name,
                description: row.description,
                rel_path: format!("{KB_MOUNT_REL_DIR}/{link_name}"),
                toc,
                summary,
                live_sources,
            });
        }
        MountOutcome {
            mounts,
            writeback: binding.enabled && binding.writeback,
            writeback_mode: binding.writeback_mode,
            writeback_eagerness: binding.writeback_eagerness,
            channel_write_enabled: binding.channel_write_enabled,
        }
    }

    // ── Internals ───────────────────────────────────────────────────

    /// Search the given bases for `query`, returning up to `limit` ranked hits.
    /// Walks each base's REAL `root_path` directly (managed or external),
    /// applying the same `.md`-only + `_inbox`-excluded rules as `build_toc`.
    /// This bypasses the workspace mount's hidden-dir + self-`.gitignore`
    /// blindness entirely. Unknown ids are skipped (not an error).
    pub async fn search_bases(
        &self,
        kb_ids: &[String],
        query: &str,
        limit: usize,
    ) -> Result<Vec<KnowledgeSearchHit>, AppError> {
        let query = query.trim();
        if query.is_empty() || kb_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut roots: Vec<(String, String, PathBuf)> = Vec::new();
        for id in kb_ids {
            if let Ok(Some(row)) = self.repo.get_base(id).await {
                roots.push((row.id.clone(), row.name.clone(), PathBuf::from(&row.root_path)));
            }
        }
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let query = query.to_owned();
        let cache = Arc::clone(&self.search_cache);
        let mut hits = bounded_blocking(SEARCH_WALK_BUDGET, Vec::new(), move || {
            let query_lc = query.to_lowercase();
            let terms = query_terms(&query_lc);
            let mut all: Vec<KnowledgeSearchHit> = Vec::new();
            for (kb_id, kb_name, root) in &roots {
                for entry in vault_walker(root) {
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let path = entry.path();
                    if !is_md(path) {
                        continue;
                    }
                    let Ok(rel) = path.strip_prefix(root) else { continue };
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    if rel == KB_INBOX_REL_DIR || rel.starts_with(&format!("{KB_INBOX_REL_DIR}/")) {
                        continue;
                    }
                    // mtime-keyed cache: reuse decoded content+heading when the
                    // file is unchanged; otherwise read, score, and cache.
                    let (mtime_ms, size) = match entry.metadata() {
                        Ok(m) => (
                            m.modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0),
                            m.len(),
                        ),
                        Err(_) => (0, 0),
                    };
                    let abs = path.to_path_buf();
                    // Invalidate on mtime OR size change (size guards against
                    // coarse-mtime filesystems where an edit keeps the tick).
                    let cached = {
                        let guard = cache.read().unwrap_or_else(|e| e.into_inner());
                        guard
                            .entries
                            .get(&abs)
                            .filter(|c| c.mtime_ms == mtime_ms && c.bytes as u64 == size)
                            .map(|c| (Arc::clone(&c.content), Arc::clone(&c.heading)))
                    };
                    let (content, heading): (Arc<str>, Arc<str>) = if let Some(hit) = cached {
                        hit
                    } else {
                        let Ok(raw) = std::fs::read_to_string(path) else { continue };
                        let heading: Arc<str> = first_heading_text(&raw).into();
                        let content: Arc<str> = raw.into();
                        if content.len() <= MAX_SEARCH_CACHE_FILE_BYTES {
                            let mut guard = cache.write().unwrap_or_else(|e| e.into_inner());
                            if let Some(old) = guard.entries.remove(&abs) {
                                guard.total_bytes = guard.total_bytes.saturating_sub(old.bytes);
                            }
                            if guard.total_bytes + content.len() <= MAX_SEARCH_CACHE_BYTES {
                                guard.total_bytes += content.len();
                                guard.entries.insert(
                                    abs,
                                    CachedDoc {
                                        mtime_ms,
                                        content: Arc::clone(&content),
                                        heading: Arc::clone(&heading),
                                        bytes: content.len(),
                                    },
                                );
                            }
                        }
                        (content, heading)
                    };
                    if let Some((score, snippet)) = score_md(&rel, &heading, &content, &query_lc, &terms) {
                        all.push(KnowledgeSearchHit {
                            kb_id: kb_id.clone(),
                            kb_name: kb_name.clone(),
                            rel_path: rel,
                            heading: heading.to_string(),
                            snippet,
                            score,
                        });
                    }
                }
            }
            all
        })
        .await;

        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.rel_path.cmp(&b.rel_path)));
        hits.truncate(limit);
        Ok(hits)
    }

    /// Drop search-cache entries whose files no longer exist (e.g. after a base
    /// delete). Off the hot path — the size cap already bounds waste.
    pub fn prune_search_cache(&self) {
        let mut guard = self.search_cache.write().unwrap_or_else(|e| e.into_inner());
        let mut freed = 0usize;
        guard.entries.retain(|path, doc| {
            if path.exists() {
                true
            } else {
                freed += doc.bytes;
                false
            }
        });
        guard.total_bytes = guard.total_bytes.saturating_sub(freed);
    }

    /// Empty the search content cache (forced refresh / test isolation).
    pub fn clear_search_cache(&self) {
        let mut guard = self.search_cache.write().unwrap_or_else(|e| e.into_inner());
        guard.entries.clear();
        guard.total_bytes = 0;
    }

    #[cfg(test)]
    fn search_cache_len(&self) -> usize {
        self.search_cache.read().unwrap_or_else(|e| e.into_inner()).entries.len()
    }

    /// Resolve which knowledge base IDs a caller at `cwd` may search.
    ///
    /// - `cwd` empty OR maps to `DEFAULT_WORKPATH_KEY` (backend-managed temp
    ///   workspace) OR no `workpath` binding exists for the key → returns ALL
    ///   registered base IDs (broadest fallback — the model still cannot widen
    ///   scope beyond what is registered).
    /// - Otherwise → returns the `kb_ids` from the workpath binding (same set
    ///   `ensure_mounts_for_session` would mount).
    ///
    /// Used by [`crate::mcp_server`] to resolve the search scope at runtime
    /// from the caller's cwd rather than relying on baked `kb_ids`.
    pub async fn resolve_kb_ids_for_cwd(&self, cwd: &str) -> Vec<String> {
        use crate::workpath::{DEFAULT_WORKPATH_KEY, WORKPATH_BINDING_KIND, session_workpath_key};

        let key = if cwd.trim().is_empty() {
            DEFAULT_WORKPATH_KEY.to_owned()
        } else {
            session_workpath_key(std::path::Path::new(cwd), &self.data_dir)
        };

        // DEFAULT_WORKPATH_KEY → all bases (no per-path scoping).
        if key == DEFAULT_WORKPATH_KEY {
            return self.all_base_ids().await;
        }

        // Look up the workpath binding — same row `ensure_mounts_for_session` uses.
        match self.get_binding(WORKPATH_BINDING_KIND, &key).await {
            Ok(binding) if binding.enabled && !binding.kb_ids.is_empty() => binding.kb_ids,
            // No row, disabled, or empty → fallback to all bases.
            _ => self.all_base_ids().await,
        }
    }

    /// All registered base IDs (the broadest search scope). Used as fallback
    /// when no workpath binding narrows it.
    async fn all_base_ids(&self) -> Vec<String> {
        match self.repo.list_bases().await {
            Ok(rows) => rows.into_iter().map(|r| r.id).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "failed to list knowledge bases for scope resolution");
                Vec::new()
            }
        }
    }

    /// Resolve the WRITE context for an ACP/terminal caller's cwd: the bound
    /// kb_ids (scope), the governing workpath binding (drives the write policy),
    /// and a stable workpath key for staged-inbox placement. Mirrors
    /// [`Self::resolve_kb_ids_for_cwd`] but also returns the binding + key the
    /// write path needs. An empty/temp cwd (DEFAULT_WORKPATH_KEY) yields the
    /// default binding (writeback off → policy Disabled), so a CLI started in an
    /// unbound directory cannot write until the user enables write-back there.
    pub async fn resolve_write_context_for_cwd(&self, cwd: &str) -> (Vec<String>, KnowledgeBinding, String) {
        use crate::workpath::{DEFAULT_WORKPATH_KEY, WORKPATH_BINDING_KIND, session_workpath_key};

        let key = if cwd.trim().is_empty() {
            DEFAULT_WORKPATH_KEY.to_owned()
        } else {
            session_workpath_key(std::path::Path::new(cwd), &self.data_dir)
        };
        if key == DEFAULT_WORKPATH_KEY {
            return (self.all_base_ids().await, KnowledgeBinding::default(), key);
        }
        let binding = self.get_binding(WORKPATH_BINDING_KIND, &key).await.unwrap_or_default();
        let bound = if binding.enabled && !binding.kb_ids.is_empty() {
            binding.kb_ids.clone()
        } else {
            self.all_base_ids().await
        };
        (bound, binding, key)
    }

    /// Backend data directory — used by `export` to place import temp dirs
    /// next to the managed bases (same volume, cheap renames).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// True when the managed knowledge root (`{data_dir}/knowledge`) and the
    /// workspace overlap in either direction: the workspace IS the data root
    /// or an ancestor of it (its sweep would run over the tree containing
    /// every managed base), or the workspace lives INSIDE the managed root
    /// (its sweep would run inside a knowledge base's own files).
    /// Canonicalized on both sides so junction/symlink/8.3 spellings cannot
    /// dodge the check; a path that fails to canonicalize (not yet existing)
    /// is compared as-is.
    fn workspace_overlaps_managed_root(&self, workspace: &Path) -> bool {
        let data_root = std::fs::canonicalize(&self.data_dir).unwrap_or_else(|_| self.data_dir.clone());
        let managed = data_root.join(KB_MANAGED_REL_DIR);
        let ws = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        managed.starts_with(&ws) || ws.starts_with(&managed)
    }

    async fn require_base(&self, id: &str) -> Result<KnowledgeBaseRow, AppError> {
        self.repo
            .get_base(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("knowledge base {id} not found")))
    }
}

/// Conversation-delete hook: drop the conversation's knowledge binding so
/// rows don't accumulate as orphans. Failures are logged, never propagated
/// (hook contract).
#[async_trait::async_trait]
impl nomifun_common::OnConversationDelete for KnowledgeService {
    async fn on_conversation_deleted(&self, conversation_id: i64) {
        // Knowledge bindings are keyed by a string `(kind, target_id)`; the
        // integer conversation id is stringified at this domain boundary.
        let target_id = conversation_id.to_string();
        if let Err(e) = self.repo.delete_binding("conversation", &target_id).await {
            tracing::warn!(conversation_id, error = %e, "failed to delete knowledge binding");
        }
    }
}

impl KnowledgeService {

    async fn row_to_info(&self, row: KnowledgeBaseRow) -> KnowledgeBaseInfo {
        let source = source_from_extra(&row.extra);
        let root = PathBuf::from(&row.root_path);
        let root_for_inbox = root.clone();
        // Bounded so a slow/stale NAS mount degrades (assume present, counts
        // unknown) instead of hanging the list/detail response past the
        // client's request timeout — the reported "加载失败" failure mode.
        let (file_count, total_size, root_exists) =
            bounded_blocking(BASE_WALK_BUDGET, (0u64, 0u64, true), move || {
                if !root.is_dir() {
                    return (0u64, 0u64, false);
                }
                let mut count = 0u64;
                let mut size = 0u64;
                for entry in vault_walker(&root) {
                    if entry.file_type().is_file() && is_md(entry.path()) {
                        count += 1;
                        size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                }
                (count, size, true)
            })
            .await;

        let pending_inbox = bounded_blocking(BASE_WALK_BUDGET, 0u64, move || {
            list_inbox_entries(&root_for_inbox).len() as u64
        })
        .await;

        let tags: Vec<String> = row
            .tags
            .as_deref()
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default();

        let kind = derive_kind(row.managed, source.as_ref()).to_string();

        KnowledgeBaseInfo {
            id: row.id,
            seq: None,
            name: row.name,
            description: row.description,
            root_path: row.root_path,
            managed: row.managed,
            created_at: row.created_at,
            updated_at: row.updated_at,
            file_count,
            total_size,
            root_exists,
            source,
            source_fetch: None,
            tags,
            kind,
            pending_inbox,
        }
    }
}

fn validate_kind(kind: &str) -> Result<(), AppError> {
    if BINDING_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!("unsupported binding kind: {kind}")))
    }
}

/// Canonical storage form of a binding `target_id`. Workpath targets are
/// re-normalized server-side ([`workpath_key`] is idempotent), so every
/// client spelling of the same directory (trailing slash, backslashes)
/// converges on one binding row; other kinds carry opaque ids unchanged.
fn canonical_target_id(kind: &str, target_id: &str) -> String {
    if kind == WORKPATH_BINDING_KIND {
        workpath_key(target_id)
    } else {
        target_id.to_owned()
    }
}

/// Run one completion against either the completer's default model
/// (`override_model = None`) or an explicit `(provider_id, model)`
/// (`Some`). The single dispatch point so the default-vs-override branch is
/// written once for every knowledge LLM call.
async fn complete_dispatch(
    completer: &dyn KnowledgeCompleter,
    system: &str,
    user: &str,
    override_model: Option<&(String, String)>,
) -> Result<String, AppError> {
    match override_model {
        Some((provider_id, model)) => completer.complete_with(system, user, provider_id, model).await,
        None => completer.complete(system, user).await,
    }
}

/// Overview-call wrapper around [`complete_dispatch`] (the overview path
/// inlines its own retry loop, so it only needs the single-shot dispatch).
async fn complete_overview(
    completer: &dyn KnowledgeCompleter,
    user: &str,
    override_model: Option<&(String, String)>,
) -> Result<String, AppError> {
    complete_dispatch(completer, autogen::OVERVIEW_SYSTEM, user, override_model).await
}

/// Run one description-only completion with the overview path's tolerance:
/// one retry on parse failure (the model occasionally wraps in prose),
/// provider failures propagate immediately, and the result is clamped to
/// [`autogen::DESCRIPTION_MAX_CHARS`]. `override_model` pins an explicit
/// `(provider_id, model)` (UI picker); `None` uses the completer's default.
async fn complete_description(
    completer: &dyn KnowledgeCompleter,
    system: &str,
    user: &str,
    override_model: Option<&(String, String)>,
) -> Result<String, AppError> {
    let mut last_err = String::new();
    for attempt in 0..2 {
        let raw = complete_dispatch(completer, system, user, override_model).await?;
        match autogen::parse_description_output(&raw) {
            Ok(description) => return Ok(autogen::clamp_description(&description)),
            Err(e) => {
                last_err = e;
                tracing::debug!(attempt, error = %last_err, "knowledge description output unparseable");
            }
        }
    }
    Err(AppError::BadGateway(format!(
        "knowledge description output unparseable: {last_err}"
    )))
}

fn binding_from_row((row, kb_ids): (KnowledgeBindingRow, Vec<String>)) -> KnowledgeBinding {
    let writeback_mode = if WRITEBACK_MODES.contains(&row.writeback_mode.as_str()) {
        row.writeback_mode
    } else {
        default_writeback_mode()
    };
    let writeback_eagerness = if WRITEBACK_EAGERNESS.contains(&row.writeback_eagerness.as_str()) {
        row.writeback_eagerness
    } else {
        default_writeback_eagerness()
    };
    KnowledgeBinding {
        enabled: row.enabled,
        writeback: row.writeback,
        writeback_mode,
        writeback_eagerness,
        channel_write_enabled: row.channel_write_enabled,
        kb_ids,
    }
}

/// Deserialize `extra.source`, tolerating absent/corrupt extras (`None`).
fn source_from_extra(extra: &str) -> Option<KnowledgeSource> {
    let value: serde_json::Value = serde_json::from_str(extra).ok()?;
    serde_json::from_value(value.get("source")?.clone()).ok()
}

/// Derive the UI type discriminator for a knowledge base:
/// - `"feishu"` when a connector-backed source has `kind == "feishu"`
/// - `"web"` when a URL source is attached (`kind == "url"`)
/// - `"blank"` for managed bases with no source (user creates from scratch)
/// - `"local"` for non-managed (user-referenced directory) bases
fn derive_kind(managed: bool, source: Option<&KnowledgeSource>) -> &'static str {
    match source.map(|s| s.kind.as_str()) {
        Some("feishu") => "feishu",
        Some("url") => "web",
        _ => if managed { "blank" } else { "local" },
    }
}

/// Syntactic validation of a client-supplied source config (full SSRF
/// resolution happens per fetch, not here — live-mode URLs are stored
/// without ever being fetched by us).
///
/// **P3-K3**: the per-entry `rendered` flag is meaningful only for URL sources.
/// It needs no dedicated check here because the `kind != "url"` guard below
/// rejects every non-URL source outright (rendered or not), so a `rendered`
/// entry can only ever reach storage on a `url` source. The flag must also be
/// backed by a valid http(s) URL — already guaranteed by the per-entry URL
/// validation in the loop. `rendered` is best-effort routing (browser backend
/// when wired, HTTP otherwise), so an unsupported value can never make a config
/// invalid; there is intentionally nothing to reject.
/// Validate a source config before it is persisted. Dispatches on `kind`:
/// `"url"` runs the existing URL-entry checks; any other kind is treated as a
/// connector-backed source and gets structural validation only (credential
/// reachability is probed at credential-creation and at sync time). Connectors
/// are snapshot-only in v1.
fn validate_source(source: &KnowledgeSource) -> Result<(), AppError> {
    if source.kind == "url" {
        return validate_url_source(source);
    }
    if source.mode != KnowledgeSourceMode::Snapshot {
        return Err(AppError::BadRequest(format!(
            "connector source \"{}\" must use snapshot mode",
            source.kind
        )));
    }
    if source.credential_ref.as_deref().map(str::trim).filter(|s| !s.is_empty()).is_none() {
        return Err(AppError::BadRequest(format!(
            "connector source \"{}\" requires a credential_ref",
            source.kind
        )));
    }
    match &source.scope {
        Some(v) if !v.is_null() => {}
        _ => {
            return Err(AppError::BadRequest(format!(
                "connector source \"{}\" requires a scope",
                source.kind
            )));
        }
    }
    Ok(())
}

/// A filesystem-safe, deterministic slug for a connector document, derived
/// from its stable `remote_id`. Used as the snapshot filename so re-syncs
/// overwrite in place and deletions can target `{slug}.md` directly.
fn connector_doc_slug(remote_id: &str) -> String {
    let mut slug = String::with_capacity(remote_id.len());
    let mut last_dash = false;
    for ch in remote_id.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() { "doc".to_owned() } else { trimmed.to_owned() }
}

fn validate_url_source(source: &KnowledgeSource) -> Result<(), AppError> {
    if source.entries.is_empty() {
        return Err(AppError::BadRequest("source.entries must not be empty".into()));
    }
    if source.entries.len() > MAX_SOURCE_ENTRIES {
        return Err(AppError::BadRequest(format!(
            "source.entries exceeds the limit of {MAX_SOURCE_ENTRIES} (got {})",
            source.entries.len()
        )));
    }
    for entry in &source.entries {
        let url = Url::parse(entry.url.trim())
            .map_err(|e| AppError::BadRequest(format!("invalid source URL {}: {e}", entry.url)))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(AppError::BadRequest(format!(
                "only http(s) source URLs are supported: {}",
                entry.url
            )));
        }
    }
    Ok(())
}

/// Delete `{root}/snapshots/*.md` files whose frontmatter `source_url` no
/// longer appears in the configured entries (orphans left behind after the
/// entry list shrank). Files WITHOUT a `source_url` frontmatter line are
/// user-authored and never touched. Best-effort: IO failures are logged,
/// never propagated.
async fn prune_orphan_snapshots(root: &Path, entries: &[KnowledgeSourceEntry]) {
    let urls: HashSet<&str> = entries.iter().map(|e| e.url.trim()).collect();
    let snap_dir = root.join(source_url::SNAPSHOT_REL_DIR);
    let Ok(mut dir) = tokio::fs::read_dir(&snap_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = dir.next_entry().await {
        let path = entry.path();
        if !entry.file_type().await.is_ok_and(|t| t.is_file()) || !is_md(&path) {
            continue;
        }
        let Ok(content) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        // No source_url frontmatter ⇒ not ours ⇒ keep.
        let Some(src_url) = source_url::snapshot_source_url(&content) else {
            continue;
        };
        if !urls.contains(src_url.trim()) {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    tracing::info!(path = %path.display(), source_url = src_url, "removed orphan knowledge snapshot");
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "failed to remove orphan knowledge snapshot");
                }
            }
        }
    }
}

/// Atomic text write (temp sibling + rename), creating parent dirs.
/// Condense an oversized snapshot body for storage and flag lossy results.
/// Above the compress threshold (and with a completer) the body is
/// LLM-summarized; the summarizer's input is byte-capped, so a body larger than
/// that cap is summarized from its HEAD only. A final hard cap bounds storage.
/// When either path drops content, a visible marker is appended so a partial
/// snapshot is never mistaken — by the user OR the agent — for the full doc.
async fn condense_snapshot_body(body: String, completer: Option<&dyn KnowledgeCompleter>) -> String {
    let original_len = body.len();
    let mut out = body;
    let mut summarized_from_head = false;
    if out.len() > autogen::SNAPSHOT_COMPRESS_THRESHOLD
        && let Some(completer) = completer
    {
        let input_truncated = out.len() > autogen::SNAPSHOT_LLM_INPUT_MAX;
        let input = source_url::truncate_to_bytes(&out, autogen::SNAPSHOT_LLM_INPUT_MAX);
        match completer.complete(autogen::SNAPSHOT_COMPRESS_SYSTEM, input).await {
            Ok(condensed) if !condensed.trim().is_empty() => {
                out = condensed.trim().to_owned();
                summarized_from_head = input_truncated;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "snapshot compression failed; storing raw"),
        }
    }
    let mut hard_truncated = false;
    if out.len() > source_url::SNAPSHOT_MAX_BYTES {
        out = source_url::truncate_to_bytes(&out, source_url::SNAPSHOT_MAX_BYTES).to_owned();
        hard_truncated = true;
    }
    if summarized_from_head {
        out.push_str(&format!(
            "\n\n> ⚠️ 注:本快照由原文前 {} KB 摘要生成(原文约 {} KB),后半内容未纳入。\n",
            autogen::SNAPSHOT_LLM_INPUT_MAX / 1024,
            original_len / 1024
        ));
    } else if hard_truncated {
        out.push_str(&format!(
            "\n\n> ⚠️ 注:本快照内容已截断至 {} KB 上限。\n",
            source_url::SNAPSHOT_MAX_BYTES / 1024
        ));
    }
    out
}

async fn write_text_atomic(path: &Path, content: &str) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AppError::Internal(format!("failed to create parent dirs: {e}")))?;
    }
    let tmp = atomic_tmp_path(path);
    tokio::fs::write(&tmp, content)
        .await
        .map_err(|e| AppError::Internal(format!("failed to write file: {e}")))?;
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(AppError::Internal(format!("failed to finalize file: {e}")));
    }
    Ok(())
}

/// A collision-free sibling temp path for atomic write+rename. Two concurrent
/// writers targeting the same file MUST NOT share a temp name (they would
/// truncate each other); the pid+counter suffix makes each unique. The `.tmp`
/// tail keeps the temp out of `is_md` listings/search, and an orphan left by a
/// crash is harmless (never listed).
fn atomic_tmp_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.{}.tmp", std::process::id(), n));
    PathBuf::from(name)
}

/// Max chars of the mount-time `summary` extracted from a base's README.
const SUMMARY_MAX_CHARS: usize = 400;

/// Locate the base's root README case-insensitively (`README.md`,
/// `readme.md`, … — Linux filesystems distinguish them). Prefers the exact
/// `README.md` spelling when several casings coexist; otherwise the first
/// case-insensitive match wins. Returns the file's ACTUAL path so reads and
/// overwrites hit the existing file instead of creating a parallel one.
async fn find_readme_path(root: &Path) -> Option<PathBuf> {
    let mut fallback: Option<PathBuf> = None;
    let mut dir = tokio::fs::read_dir(root).await.ok()?;
    while let Ok(Some(entry)) = dir.next_entry().await {
        if !entry.file_type().await.is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "README.md" {
            return Some(entry.path());
        }
        if name.eq_ignore_ascii_case("README.md") && fallback.is_none() {
            fallback = Some(entry.path());
        }
    }
    fallback
}

/// First non-heading paragraph of the base's root README (matched
/// case-insensitively), truncated to [`SUMMARY_MAX_CHARS`]. `None` when the
/// base has no README (yet) — the AI-autogen README task fills these in over
/// time.
async fn read_base_summary(root: &Path) -> Option<String> {
    let path = find_readme_path(root).await?;
    let text = tokio::fs::read_to_string(path).await.ok()?;
    extract_readme_summary(&text)
}

/// Extract the first non-heading paragraph from markdown text: headings and
/// blank lines before it are skipped, badge (`[![…`) and raw-HTML (`<…`)
/// lines cannot START the paragraph (README boilerplate noise), its
/// consecutive non-blank non-heading lines are joined with spaces, and the
/// result is truncated to [`SUMMARY_MAX_CHARS`] chars (with a trailing `…`
/// marker when truncation actually happened).
fn extract_readme_summary(text: &str) -> Option<String> {
    let mut para: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            if !para.is_empty() {
                break;
            }
            continue;
        }
        // Badge rows and HTML blocks are layout noise, not prose — skip them
        // while still hunting for the first real paragraph.
        if para.is_empty() && (trimmed.starts_with("[![") || trimmed.starts_with('<')) {
            continue;
        }
        para.push(trimmed);
    }
    if para.is_empty() {
        return None;
    }
    let joined = para.join(" ");
    let mut summary: String = joined.chars().take(SUMMARY_MAX_CHARS).collect();
    if joined.chars().count() > SUMMARY_MAX_CHARS {
        summary.push('…');
    }
    Some(summary)
}

/// Ordering rank for TOC entries so the highest-signal files survive the
/// per-base budget (`context::apply_toc_budgets` keeps the first N): root
/// index/README/overview first, then any index-like file, then shallower
/// paths before deeper. Pure path-based — no file reads. Ties fall through to
/// lexicographic in the caller.
fn toc_rank(rel: &str) -> (u8, usize) {
    let lower = rel.to_lowercase();
    let depth = rel.matches('/').count();
    let stem = lower.rsplit('/').next().unwrap_or(lower.as_str());
    let is_index = matches!(stem, "readme.md" | "index.md" | "overview.md" | "_index.md")
        || stem.starts_with("readme.");
    let tier = if is_index && depth == 0 {
        0
    } else if is_index {
        1
    } else {
        2
    };
    (tier, depth)
}

/// Build the full per-base table of contents: one `rel/path.md — first
/// heading` line per document, `_inbox/` excluded (unreviewed staged
/// write-backs are not authoritative navigation). Budgeting/aggregation is
/// applied afterwards across all mounted bases via
/// [`crate::context::apply_toc_budgets`].
async fn build_toc(root: &Path) -> Vec<String> {
    let root = root.to_path_buf();
    // Bounded + machinery-pruned: at session mount this opens every note for its
    // first heading, so a slow/large NAS vault must degrade to an empty toc
    // rather than block session start.
    bounded_blocking(BASE_WALK_BUDGET, Vec::new(), move || {
        if !root.is_dir() {
            return Vec::new();
        }
        let mut rels: Vec<String> = vault_walker(&root)
            .filter(|e| e.file_type().is_file() && is_md(e.path()))
            .filter_map(|e| {
                let rel = e.path().strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/");
                (!rel.starts_with(&format!("{KB_INBOX_REL_DIR}/"))).then_some(rel)
            })
            .collect();
        rels.sort_by(|a, b| toc_rank(a).cmp(&toc_rank(b)).then_with(|| a.cmp(b)));
        rels.into_iter()
            .map(|rel| match first_heading(&root.join(&rel)) {
                Some(title) => format!("{rel} — {title}"),
                None => rel,
            })
            .collect()
    })
    .await
}

/// First `# `-style heading of a markdown file, read from the first KB only
/// (bounds IO for large files); single line, truncated to keep the prompt
/// row compact.
/// First ATX markdown heading (`# …` … `###### …`) in `text`, trimmed. Requires
/// whitespace (or end-of-line) after the `#` run, so a shebang `#!/bin/sh`, a
/// `#hashtag`, or a `# comment` inside code is NOT mistaken for a title; skips
/// fenced code blocks (``` / ~~~) and a leading YAML front-matter block (whose
/// `# comment` lines and `key: #x` are not headings).
fn first_atx_heading(text: &str) -> Option<String> {
    let mut in_fence: Option<char> = None;
    let mut in_frontmatter = false;
    let mut started = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        // A leading `---` (first non-empty line) opens a YAML front-matter block.
        if !started && !trimmed.is_empty() {
            started = true;
            if trimmed == "---" {
                in_frontmatter = true;
                continue;
            }
        }
        if in_frontmatter {
            if trimmed == "---" || trimmed == "..." {
                in_frontmatter = false;
            }
            continue;
        }
        // Toggle fenced code-block state on ``` / ~~~ (a fence only closes on the
        // same marker char it opened with).
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let fence_char = trimmed.chars().next().unwrap_or('`');
            match in_fence {
                Some(open) if open == fence_char => in_fence = None,
                Some(_) => {}
                None => in_fence = Some(fence_char),
            }
            continue;
        }
        if in_fence.is_some() {
            continue;
        }
        let hashes = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) {
            let rest = &trimmed[hashes..];
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                let title = rest.trim();
                if !title.is_empty() {
                    return Some(title.to_owned());
                }
            }
        }
    }
    None
}

fn first_heading(path: &Path) -> Option<String> {
    use std::io::Read;
    // 4 KiB is enough to clear a typical YAML front-matter block before the
    // first heading (the old 1 KiB could be entirely front-matter).
    let mut buf = vec![0u8; 4096];
    let mut file = std::fs::File::open(path).ok()?;
    let n = file.read(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let title = first_atx_heading(&text)?;
    Some(title.chars().take(60).collect())
}

pub(crate) fn is_md(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

/// True for a directory that is knowledge-base *machinery*, never a source of
/// knowledge documents, and must be pruned from every base walk BEFORE its
/// contents are stat'd: any hidden (dot-prefixed) directory — `.obsidian/`
/// (Obsidian plugins + workspace + cache), `.git/`, `.trash/` — plus a couple
/// of well-known heavy non-note dirs. The root itself (depth 0) is never
/// pruned, so a base may still be rooted at a dotted path.
///
/// This is both a correctness fix (those files are not notes) and the dominant
/// performance fix: on a real Obsidian vault the `.obsidian/` tree alone can
/// dwarf the note count, and descending into it issues one `readdir`/`stat`
/// network round-trip per entry — on a slow NAS mount that is what pushes the
/// per-base walk past the client request timeout and surfaces as "加载失败".
fn is_machinery_dir(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    name.starts_with('.') || name == "node_modules"
}

/// The canonical markdown walker for a knowledge-base `root`: a [`walkdir`]
/// iterator that prunes [`is_machinery_dir`] directories before descending, so
/// no `stat` is ever issued inside `.obsidian/`, `.git/`, etc. `follow_links`
/// stays at walkdir's default (`false`) — that is correct and must be kept, as
/// it rules out symlink-cycle / root-escape traversal. Every KB file walk goes
/// through here so the traversal policy is defined once.
fn vault_walker(root: &Path) -> impl Iterator<Item = walkdir::DirEntry> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_machinery_dir(e))
        .flatten()
}

/// Run a blocking filesystem walk on the blocking pool but never let it hold up
/// the caller past `budget`; on expiry (a slow or stale NAS mount) or a panic
/// in the walk, return `on_timeout` and leave the walk to finish detached
/// (`spawn_blocking` closures cannot be cancelled). This bounds every KB
/// directory walk so a wedged mount degrades the response instead of hanging
/// the request past the client's ~60s deadline (the reported failure mode).
async fn bounded_blocking<T: Send + 'static>(
    budget: std::time::Duration,
    on_timeout: T,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let handle = tokio::task::spawn_blocking(f);
    match tokio::time::timeout(budget, handle).await {
        Ok(Ok(v)) => v,
        // Walk panicked → degrade rather than propagate.
        Ok(Err(_join_err)) => on_timeout,
        // Slow/stale mount exceeded the budget → degrade; the detached walk
        // finishes on its own and its result is discarded.
        Err(_elapsed) => on_timeout,
    }
}

/// Per-base walk budget. Pruning ([`is_machinery_dir`]) makes a healthy vault
/// walk finish well within this; the budget is the safety net for a large or
/// stale NAS mount so the list/detail response never blocks past it.
const BASE_WALK_BUDGET: std::time::Duration = std::time::Duration::from_secs(6);

/// Max concurrent per-base walks in [`KnowledgeService::list_bases`]. Small and
/// fixed: enough to overlap a few slow (NAS) bases with the fast local ones
/// without fanning out an unbounded number of blocking-pool tasks.
const LIST_BASES_CONCURRENCY: usize = 8;

/// Budget for a full `search_bases` sweep (walk + cold-cache reads across every
/// scoped base). More generous than a single-base stat walk because search
/// legitimately reads file bodies; on expiry the search degrades to the hits
/// gathered so far being dropped (empty) rather than hanging the agent tool.
const SEARCH_WALK_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

/// Split a lowercased query into non-empty, deduped whitespace terms. For CJK
/// (no spaces) this yields the whole query as one term, which still
/// substring-matches — adequate for the keyword tier.
fn query_terms(query_lc: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    query_lc
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .filter(|t| seen.insert(t.to_string()))
        .map(str::to_owned)
        .collect()
}

/// First markdown heading (`# ...`) text, trimmed of leading `#`/space, or "".
/// The content-string counterpart of [`first_heading`] (which reads from disk).
fn first_heading_text(content: &str) -> String {
    first_atx_heading(content).unwrap_or_default()
}

/// Score a markdown doc against the query. Returns `(score, snippet)` or `None`
/// when nothing matches. Path/heading hits weigh more than body frequency.
fn score_md(
    rel_path: &str,
    heading: &str,
    content: &str,
    query_lc: &str,
    terms: &[String],
) -> Option<(u32, String)> {
    let path_lc = rel_path.to_lowercase();
    let heading_lc = heading.to_lowercase();
    let content_lc = content.to_lowercase();
    let mut score: u32 = 0;
    if path_lc.contains(query_lc) || heading_lc.contains(query_lc) {
        score += 8;
    }
    if content_lc.contains(query_lc) {
        score += 5;
    }
    for t in terms {
        if path_lc.contains(t.as_str()) {
            score += 4;
        }
        if heading_lc.contains(t.as_str()) {
            score += 3;
        }
        let tf = content_lc.matches(t.as_str()).count() as u32;
        score += tf.min(5);
    }
    if score == 0 {
        return None;
    }
    Some((score, best_snippet(content, query_lc, terms)))
}

/// First content line containing the query or any term, capped to ~200 chars.
fn best_snippet(content: &str, query_lc: &str, terms: &[String]) -> String {
    let pick = content.lines().find(|line| {
        let l = line.to_lowercase();
        l.contains(query_lc) || terms.iter().any(|t| l.contains(t.as_str()))
    });
    let line = pick.unwrap_or_else(|| content.lines().next().unwrap_or("")).trim();
    let mut s: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        s.push('…');
    }
    s
}

/// Strip a workspace-mount prefix the model may have prepended by mistake,
/// returning a path relative to the base root. The model sees the mount at
/// `.nomi/knowledge/{link}/…` but `knowledge_write` expects a base-relative
/// path; without this, `.nomi/knowledge/Finance/terms.md` would create a new
/// nested file instead of updating `terms.md`. Only the unambiguous mount
/// prefix is stripped — a bare `Finance/x.md` is left to the resolver's
/// existence/collision logic.
fn deconfuse_rel_path(rel_path: &str) -> String {
    let normalized = rel_path.trim().replace('\\', "/");
    let p = normalized.strip_prefix("./").unwrap_or(&normalized);
    if let Some(rest) = p.strip_prefix(&format!("{KB_MOUNT_REL_DIR}/")) {
        // rest = "{link_name}/…"; drop the mount link segment.
        return match rest.split_once('/') {
            Some((_link, tail)) => tail.to_owned(),
            None => rest.to_owned(),
        };
    }
    p.to_owned()
}

/// Join `rel_path` onto `root`, rejecting traversal (absolute paths, `..`,
/// drive prefixes) and non-markdown extensions.
fn safe_md_path(root: &Path, rel_path: &str) -> Result<PathBuf, AppError> {
    let rel = Path::new(rel_path);
    if rel.as_os_str().is_empty() {
        return Err(AppError::BadRequest("path must not be empty".into()));
    }
    for comp in rel.components() {
        match comp {
            Component::Normal(_) => {}
            _ => return Err(AppError::BadRequest(format!("invalid path: {rel_path}"))),
        }
    }
    if !is_md(rel) {
        return Err(AppError::BadRequest("only .md files are supported".into()));
    }
    Ok(root.join(rel))
}

fn is_excluded_tree_dir_name(name: &str) -> bool {
    name.starts_with('.') || name == "node_modules" || name == KB_INBOX_REL_DIR
}

fn looks_like_windows_drive_prefix(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

fn normalize_tree_rel_path(rel_path: &str) -> Result<String, AppError> {
    let normalized = rel_path.trim().replace('\\', "/");
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }

    let mut segments = Vec::new();
    for segment in trimmed.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || looks_like_windows_drive_prefix(segment)
        {
            return Err(AppError::BadRequest(format!("invalid path: {rel_path}")));
        }
        if is_excluded_tree_dir_name(segment) {
            return Err(AppError::BadRequest(format!(
                "directory is excluded: {segment}"
            )));
        }
        segments.push(segment);
    }
    Ok(segments.join("/"))
}

fn join_tree_rel_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn list_tree_level(root: &Path, rel_path: &str) -> Result<Vec<KbTreeEntry>, AppError> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let dir = if rel_path.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel_path)
    };
    let meta = match std::fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(_) => return Ok(Vec::new()),
    };
    if !meta.file_type().is_dir() {
        return Err(AppError::NotFound(format!(
            "directory not found: {rel_path}"
        )));
    }

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => return Err(AppError::Internal(format!("failed to read directory: {e}"))),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_rel = join_tree_rel_path(rel_path, &name);
        if file_type.is_dir() {
            if is_excluded_tree_dir_name(&name) {
                continue;
            }
            out.push(KbTreeEntry {
                name,
                rel_path: child_rel,
                is_dir: true,
                is_file: false,
                size: None,
                modified_at: None,
            });
            continue;
        }
        if file_type.is_file() && is_md(&entry.path()) {
            let meta = entry.metadata().ok();
            out.push(KbTreeEntry {
                name,
                rel_path: child_rel,
                is_dir: false,
                is_file: true,
                size: meta.as_ref().map(|m| m.len()),
                modified_at: meta.as_ref().and_then(modified_ms),
            });
        }
    }

    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

fn create_tree_folder(root: &Path, rel_path: &str) -> Result<KbTreeEntry, AppError> {
    if !root.is_dir() {
        return Err(AppError::NotFound("knowledge base directory not found".into()));
    }

    let segments: Vec<&str> = rel_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        return Err(AppError::BadRequest("folder path must not be empty".into()));
    }

    let mut cursor = root.to_path_buf();
    for (idx, segment) in segments.iter().enumerate() {
        cursor.push(segment);
        let is_final = idx + 1 == segments.len();
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(AppError::BadRequest(format!("path crosses a symlink: {rel_path}")));
                }
                if !meta.file_type().is_dir() {
                    return Err(AppError::BadRequest(format!(
                        "path is not a directory: {}",
                        segments[..=idx].join("/")
                    )));
                }
                if is_final {
                    return Err(AppError::Conflict(format!("folder already exists: {rel_path}")));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cursor)
                    .map_err(|e| AppError::Internal(format!("failed to create folder: {e}")))?;
            }
            Err(e) => return Err(AppError::Internal(format!("failed to inspect folder path: {e}"))),
        }
    }

    let meta = std::fs::metadata(&cursor).ok();
    Ok(KbTreeEntry {
        name: segments.last().unwrap_or(&rel_path).to_string(),
        rel_path: rel_path.to_owned(),
        is_dir: true,
        is_file: false,
        size: None,
        modified_at: meta.as_ref().and_then(modified_ms),
    })
}

fn validate_tree_entry_name(name: &str) -> Result<String, AppError> {
    let normalized = name.trim().replace('\\', "/");
    if normalized.is_empty() || normalized.contains('/') || normalized == "." || normalized == ".." || looks_like_windows_drive_prefix(&normalized) {
        return Err(AppError::BadRequest(format!("invalid name: {name}")));
    }
    if is_excluded_tree_dir_name(&normalized) {
        return Err(AppError::BadRequest(format!(
            "directory is excluded: {normalized}"
        )));
    }
    Ok(normalized)
}

fn resolve_tree_existing_path(root: &Path, rel_path: &str) -> Result<(PathBuf, std::fs::Metadata), AppError> {
    if !root.is_dir() {
        return Err(AppError::NotFound("knowledge base directory not found".into()));
    }
    let segments: Vec<&str> = rel_path.split('/').filter(|segment| !segment.is_empty()).collect();
    if segments.is_empty() {
        return Err(AppError::BadRequest("path must not be empty".into()));
    }

    let mut cursor = root.to_path_buf();
    for (idx, segment) in segments.iter().enumerate() {
        cursor.push(segment);
        let meta = std::fs::symlink_metadata(&cursor)
            .map_err(|_| AppError::NotFound(format!("path not found: {rel_path}")))?;
        if meta.file_type().is_symlink() {
            return Err(AppError::BadRequest(format!("path crosses a symlink: {rel_path}")));
        }
        if idx + 1 < segments.len() && !meta.file_type().is_dir() {
            return Err(AppError::BadRequest(format!(
                "path is not a directory: {}",
                segments[..=idx].join("/")
            )));
        }
        if idx + 1 == segments.len() {
            return Ok((cursor, meta));
        }
    }
    Err(AppError::BadRequest("path must not be empty".into()))
}

fn remove_tree_dir_no_follow(path: &Path) -> Result<(), AppError> {
    let entries = std::fs::read_dir(path)
        .map_err(|e| AppError::Internal(format!("failed to read folder before delete: {e}")))?;
    for entry in entries {
        let entry = entry.map_err(|e| AppError::Internal(format!("failed to read folder entry before delete: {e}")))?;
        let child = entry.path();
        let meta = std::fs::symlink_metadata(&child)
            .map_err(|e| AppError::Internal(format!("failed to inspect folder entry before delete: {e}")))?;
        if meta.file_type().is_dir() {
            remove_tree_dir_no_follow(&child)?;
        } else {
            std::fs::remove_file(&child)
                .map_err(|e| AppError::Internal(format!("failed to delete folder entry: {e}")))?;
        }
    }
    std::fs::remove_dir(path)
        .map_err(|e| AppError::Internal(format!("failed to delete folder: {e}")))?;
    Ok(())
}

fn delete_tree_folder(root: &Path, rel_path: &str) -> Result<(), AppError> {
    let (path, meta) = resolve_tree_existing_path(root, rel_path)?;
    if !meta.file_type().is_dir() {
        return Err(AppError::BadRequest(format!("path is not a directory: {rel_path}")));
    }
    remove_tree_dir_no_follow(&path)
}

fn rename_tree_entry(root: &Path, rel_path: &str, new_name: &str) -> Result<KbTreeEntry, AppError> {
    let (from, meta) = resolve_tree_existing_path(root, rel_path)?;
    let file_type = meta.file_type();
    let is_file = file_type.is_file();
    let is_dir = file_type.is_dir();
    if !is_file && !is_dir {
        return Err(AppError::BadRequest(format!("unsupported path type: {rel_path}")));
    }
    if is_file && !is_md(Path::new(new_name)) {
        return Err(AppError::BadRequest("markdown files must keep a .md extension".into()));
    }

    let segments: Vec<&str> = rel_path.split('/').filter(|segment| !segment.is_empty()).collect();
    let parent_rel = if segments.len() <= 1 {
        String::new()
    } else {
        segments[..segments.len() - 1].join("/")
    };
    let parent = from
        .parent()
        .ok_or_else(|| AppError::BadRequest(format!("invalid path: {rel_path}")))?;
    let to = parent.join(new_name);
    match std::fs::symlink_metadata(&to) {
        Ok(_) => return Err(AppError::Conflict(format!("path already exists: {}", join_tree_rel_path(&parent_rel, new_name)))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(AppError::Internal(format!("failed to inspect target path: {e}"))),
    }

    std::fs::rename(&from, &to)
        .map_err(|e| AppError::Internal(format!("failed to rename tree entry: {e}")))?;

    let target_meta = std::fs::metadata(&to).ok();
    Ok(KbTreeEntry {
        name: new_name.to_owned(),
        rel_path: join_tree_rel_path(&parent_rel, new_name),
        is_dir,
        is_file,
        size: if is_file { target_meta.as_ref().map(|m| m.len()) } else { None },
        modified_at: target_meta.as_ref().and_then(modified_ms),
    })
}

/// Sanitize a base name into a directory-safe mount link name, deduplicating
/// collisions (with other mounts AND with the platform-managed companion
/// files inside the mount root, e.g. a base literally named `README.md`)
/// via a short id suffix.
fn unique_link_name(row: &KnowledgeBaseRow, used: &mut HashSet<String>) -> String {
    let mut name: String = row
        .name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .trim_end_matches('.')
        .to_owned();
    if name.is_empty() {
        name = row.id.clone();
    }
    // `MANAGED_KEEP` entries (`.gitignore`, `README.md`) are exempt from the
    // mount sweep and owned by the platform — a link with such a name would
    // collide with them. Windows file names are case-insensitive, so compare
    // accordingly.
    if mount::MANAGED_KEEP.iter().any(|kept| kept.eq_ignore_ascii_case(&name)) || used.contains(&name) {
        name = format!("{name}-{}", short_id_suffix(&row.id));
    }
    used.insert(name.clone());
    name
}

/// Last 6 chars of an id — the disambiguation suffix for link names.
fn short_id_suffix(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    chars[chars.len().saturating_sub(6)..].iter().collect()
}

fn modified_ms(meta: &std::fs::Metadata) -> Option<TimestampMs> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as TimestampMs)
}

fn list_md_files(root: &Path) -> Vec<KbFileEntry> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut entries: Vec<KbFileEntry> = vault_walker(root)
        .filter(|e| e.file_type().is_file() && is_md(e.path()))
        .filter_map(|e| {
            let rel = e.path().strip_prefix(root).ok()?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            // Staged write-back proposals live under `_inbox/` and are shown in
            // the dedicated review panel, not the main document list.
            if rel_str == KB_INBOX_REL_DIR || rel_str.starts_with(&format!("{KB_INBOX_REL_DIR}/")) {
                return None;
            }
            let meta = e.metadata().ok();
            Some(KbFileEntry {
                rel_path: rel_str,
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified_at: meta.as_ref().and_then(modified_ms),
            })
        })
        .collect();
    entries.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    entries
}

/// Walk `{root}/_inbox/{scope}/**/*.md`, deriving `scope` (first path segment)
/// and `rel_path` (the remainder, mirroring the original base path).
fn list_inbox_entries(root: &Path) -> Vec<InboxEntry> {
    let inbox_root = root.join(KB_INBOX_REL_DIR);
    if !inbox_root.is_dir() {
        return Vec::new();
    }
    let mut entries: Vec<InboxEntry> = vault_walker(&inbox_root)
        .filter(|e| e.file_type().is_file() && is_md(e.path()))
        .filter_map(|e| {
            let rel = e.path().strip_prefix(&inbox_root).ok()?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let (scope, rel_path) = rel_str.split_once('/')?;
            if scope.is_empty() || rel_path.is_empty() {
                return None;
            }
            let meta = e.metadata().ok();
            Some(InboxEntry {
                scope: scope.to_owned(),
                rel_path: rel_path.to_owned(),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified_at: meta.as_ref().and_then(modified_ms),
            })
        })
        .collect();
    entries.sort_by(|a, b| a.scope.cmp(&b.scope).then(a.rel_path.cmp(&b.rel_path)));
    entries
}

/// A staged-proposal `scope` must be a single, non-traversing path segment.
fn validate_inbox_scope(scope: &str) -> Result<(), AppError> {
    if scope.is_empty() || scope.contains('/') || scope.contains('\\') || scope == ".." || scope == "." {
        return Err(AppError::BadRequest(format!("invalid inbox scope: {scope}")));
    }
    Ok(())
}

/// Server-side unified diff for the review panel. `old` empty ⇒ a clean "new
/// document" diff (all additions).
fn unified_md_diff(old: &str, new: &str, path: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

/// Remove now-empty inbox directories, walking up from `dir` (the merged/
/// discarded file's parent) until a non-empty dir or the inbox root's parent.
/// `remove_dir` only succeeds on empty dirs, so a non-empty ancestor halts it.
async fn prune_empty_inbox_dirs(inbox_root: &Path, mut dir: PathBuf) {
    while dir.starts_with(inbox_root) {
        if tokio::fs::remove_dir(&dir).await.is_err() {
            break; // non-empty or already gone
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
}

// ── Tag CRUD ─────────────────────────────────────────────────────────────

impl KnowledgeService {
    /// List all tag definitions (sorted by `sort_order` then `key`).
    pub async fn list_tags(&self) -> Result<Vec<KnowledgeTag>, AppError> {
        let rows = self.repo.list_knowledge_tags().await?;
        Ok(rows.into_iter().map(|r| KnowledgeTag {
            key: r.key,
            label: r.label,
            color: r.color,
            sort_order: r.sort_order,
        }).collect())
    }

    /// Create a new tag. The `key` is slug-ified from `label` (lowercase,
    /// non-alphanumeric→`-`, collapsed, trimmed). Chinese-only labels produce a
    /// pinyin-like short hash fallback (`tag-<8hex>`). Conflicts are
    /// disambiguated by appending `-2`, `-3`, etc.
    pub async fn create_tag(
        &self,
        label: &str,
        color: Option<String>,
    ) -> Result<KnowledgeTag, AppError> {
        let label = label.trim();
        if label.is_empty() {
            return Err(AppError::BadRequest("tag label must not be empty".into()));
        }

        let base_slug = slugify(label);
        let existing: HashSet<String> = self
            .repo
            .list_knowledge_tags()
            .await?
            .into_iter()
            .map(|r| r.key)
            .collect();

        let key = deduplicate_slug(&base_slug, &existing);

        let sort_order = existing.len() as i64;
        let created_at = now_ms();

        self.repo
            .create_knowledge_tag(CreateKnowledgeTagParams {
                key: key.clone(),
                label: label.to_owned(),
                color: color.clone(),
                sort_order,
                created_at,
            })
            .await?;

        self.emitter.emit_tag_changed();
        Ok(KnowledgeTag {
            key,
            label: label.to_owned(),
            color,
            sort_order,
        })
    }

    /// Update mutable fields of an existing tag.
    pub async fn update_tag(
        &self,
        key: &str,
        req: UpdateKnowledgeTagRequest,
    ) -> Result<KnowledgeTag, AppError> {
        use nomifun_db::models::UpdateKnowledgeTagParams;

        let params = UpdateKnowledgeTagParams {
            label: req.label.clone(),
            // API sends `Option<String>`:
            //   absent/null (None) = don't change
            //   Some("") = clear → mapped to Some(None) in the DB layer
            //   Some("blue") = set → mapped to Some(Some("blue"))
            color: req.color.as_ref().map(|c| {
                let c = c.trim();
                if c.is_empty() { None } else { Some(c.to_owned()) }
            }),
            sort_order: req.sort_order,
        };
        self.repo.update_knowledge_tag(key, params).await?;

        // Re-read and return the updated tag.
        let rows = self.repo.list_knowledge_tags().await?;
        let row = rows
            .into_iter()
            .find(|r| r.key == key)
            .ok_or_else(|| AppError::NotFound(format!("knowledge tag {key}")))?;
        self.emitter.emit_tag_changed();
        Ok(KnowledgeTag {
            key: row.key,
            label: row.label,
            color: row.color,
            sort_order: row.sort_order,
        })
    }

    /// Delete a tag by key. **Transactionally** strips the key from the `tags`
    /// JSON array of every base that references it, then removes the tag row.
    pub async fn delete_tag(&self, key: &str) -> Result<(), AppError> {
        // 1. Strip the key from every base that references it.
        let bases = self.repo.list_bases().await?;
        for mut base in bases {
            let tags: Vec<String> = base
                .tags
                .as_deref()
                .and_then(|t| serde_json::from_str(t).ok())
                .unwrap_or_default();
            if tags.contains(&key.to_owned()) {
                let filtered: Vec<&String> = tags.iter().filter(|k| k.as_str() != key).collect();
                base.tags = if filtered.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&filtered).unwrap())
                };
                base.updated_at = now_ms();
                self.repo.update_base(&base).await?;
                // The base's tag chips changed — refresh any base list/detail
                // view (the tag-changed signal below only refreshes tag maps).
                let info = self.row_to_info(base).await;
                self.emitter.emit_base_updated(&info);
            }
        }
        // 2. Delete the tag row itself.
        self.repo.delete_knowledge_tag(key).await?;
        self.emitter.emit_tag_changed();
        Ok(())
    }
}

/// Convert a label to a URL-safe slug. Keeps ASCII alphanumeric characters;
/// replaces everything else with `-`; collapses consecutive dashes; trims
/// leading/trailing dashes. If the result is empty (e.g. purely CJK label),
/// falls back to `tag-<8 hex chars from a content hash>`.
fn slugify(label: &str) -> String {
    let lower = label.to_lowercase();
    let mut slug = String::with_capacity(lower.len());
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else {
            // Replace non-ASCII-alphanumeric with dash (will be collapsed).
            if !slug.ends_with('-') {
                slug.push('-');
            }
        }
    }
    // Trim leading/trailing dashes.
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        // Fallback: deterministic short hash so the same label always gets
        // the same key (before dedup).
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        label.hash(&mut hasher);
        format!("tag-{:08x}", hasher.finish() as u32)
    } else {
        slug
    }
}

/// Given a base slug and a set of existing keys, return a unique key by
/// appending `-2`, `-3`, … if needed.
fn deduplicate_slug(base: &str, existing: &HashSet<String>) -> String {
    if !existing.contains(base) {
        return base.to_owned();
    }
    for n in 2..=999 {
        let candidate = format!("{base}-{n}");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    // Extremely unlikely: fall back to a unique suffix.
    format!("{base}-{}", now_ms())
}


#[cfg(test)]
mod tests {
    use super::*;

    /// **P3-K2 seam**: the render fetcher is an OPTIONAL, late-wired backend. By
    /// default it is absent (every source uses the HTTP `fetcher` — zero
    /// regression); the app layer registers a `BrowserFetcher` via
    /// [`KnowledgeService::set_render_fetcher`] when `browser-use` is on. K3 reads
    /// it to route `rendered` sources; K2 only proves the seam wires.
    #[tokio::test]
    async fn render_fetcher_seam_is_optional_and_late_wired() {
        use crate::source_url::FetchedPage;

        struct CannedRenderFetcher;
        #[async_trait::async_trait]
        impl PageFetcher for CannedRenderFetcher {
            async fn fetch_page(&self, _raw_url: &str) -> Result<FetchedPage, AppError> {
                Ok(FetchedPage {
                    final_url: "https://spa.example.com/app".into(),
                    title: Some("Rendered".into()),
                    markdown: "# Rendered\n\nonly a browser sees this".into(),
                    truncated: false,
                })
            }
        }

        let dir = tempfile::TempDir::new().unwrap();
        let repo = Arc::new(MemRepo::default());
        let events = Arc::new(RecordingBroadcaster::default());
        let service = Arc::new(KnowledgeService::new(
            repo,
            &dir.path().join("data"),
            KnowledgeEventEmitter::new(events),
        ));

        // Default: no render backend → HTTP fetcher is the only path (zero regression).
        assert!(service.render_fetcher().is_none(), "render fetcher must default to None");

        // Late-wire on the shared Arc (interior mutability, like set_completer).
        service.set_render_fetcher(Arc::new(CannedRenderFetcher));
        let rf = service.render_fetcher().expect("render fetcher wired");
        let page = rf.fetch_page("https://spa.example.com/app").await.unwrap();
        assert_eq!(page.title.as_deref(), Some("Rendered"));
        assert!(page.markdown.contains("only a browser sees this"));
    }

    /// **P3-K3 backend selection** (pure logic over `fetcher_for`): each fetcher
    /// reports a distinctive marker so we can prove *which* backend a given
    /// `(rendered, render-wired?)` combination selects.
    ///   • `rendered == false`            → HTTP (default), even with a browser wired
    ///   • `rendered == true`, browser ✓  → render backend
    ///   • `rendered == true`, browser ✗  → graceful HTTP fallback (no error)
    #[tokio::test]
    async fn fetcher_for_selects_backend_by_rendered_flag() {
        use crate::source_url::FetchedPage;

        fn marked(marker: &str) -> FetchedPage {
            FetchedPage {
                final_url: "https://x".into(),
                title: Some(marker.into()),
                markdown: format!("via:{marker}"),
                truncated: false,
            }
        }

        struct Canned(&'static str);
        #[async_trait::async_trait]
        impl PageFetcher for Canned {
            async fn fetch_page(&self, _url: &str) -> Result<FetchedPage, AppError> {
                Ok(marked(self.0))
            }
        }

        async fn which(service: &KnowledgeService, rendered: bool) -> String {
            service
                .fetcher_for(rendered)
                .fetch_page("https://x")
                .await
                .unwrap()
                .title
                .unwrap()
        }

        let dir = tempfile::TempDir::new().unwrap();
        let service = KnowledgeService::new(
            Arc::new(MemRepo::default()),
            &dir.path().join("data"),
            KnowledgeEventEmitter::new(Arc::new(NoopBroadcaster)),
        )
        .with_url_fetcher(Canned("http"));

        // No render backend wired: every flag value resolves to HTTP (graceful
        // fallback for rendered=true — the flag is best-effort, never fails).
        assert_eq!(which(&service, false).await, "http", "rendered=false → HTTP");
        assert_eq!(
            which(&service, true).await,
            "http",
            "rendered=true but no browser backend → graceful HTTP fallback"
        );

        // Wire a browser backend.
        service.set_render_fetcher(Arc::new(Canned("browser")));
        assert_eq!(which(&service, false).await, "http", "rendered=false → HTTP even with browser wired");
        assert_eq!(which(&service, true).await, "browser", "rendered=true + browser wired → render backend");
    }

    #[test]
    fn safe_md_path_rejects_traversal() {
        let root = Path::new("/kb");
        assert!(safe_md_path(root, "ok.md").is_ok());
        assert!(safe_md_path(root, "sub/dir/ok.md").is_ok());
        assert!(safe_md_path(root, "../escape.md").is_err());
        assert!(safe_md_path(root, "/abs.md").is_err());
        assert!(safe_md_path(root, "no_extension").is_err());
        assert!(safe_md_path(root, "script.exe").is_err());
        assert!(safe_md_path(root, "").is_err());
        #[cfg(windows)]
        assert!(safe_md_path(root, "C:\\evil.md").is_err());
    }

    #[test]
    fn first_atx_heading_is_strict_and_skips_noise() {
        // Plain ATX heading.
        assert_eq!(first_atx_heading("# 标题\n正文").as_deref(), Some("标题"));
        assert_eq!(first_atx_heading("### Third level\n").as_deref(), Some("Third level"));
        // Requires whitespace after the run: a shebang / hashtag is NOT a heading.
        assert_eq!(first_atx_heading("#!/bin/sh\n# 真标题").as_deref(), Some("真标题"));
        assert_eq!(first_atx_heading("#hashtag\nplain").as_deref(), None);
        // 7+ hashes is not a heading.
        assert_eq!(first_atx_heading("####### too deep\n").as_deref(), None);
        // Fenced code blocks are skipped (the `# rm` inside is a comment).
        assert_eq!(
            first_atx_heading("```sh\n# rm -rf /\n```\n## 真标题\n").as_deref(),
            Some("真标题")
        );
        // Leading YAML front-matter (incl. its `# comment`) is skipped.
        assert_eq!(
            first_atx_heading("---\ntitle: x\n# not a heading\n---\n# 文档标题\n").as_deref(),
            Some("文档标题")
        );
        // No heading anywhere.
        assert_eq!(first_atx_heading("just text\nmore text\n"), None);
        // Closing-hash run trimmed via the leading-run + trim only when spaced.
        assert_eq!(first_atx_heading("#  spaced  \n").as_deref(), Some("spaced"));
    }

    #[tokio::test]
    async fn toc_lists_all_and_skips_inbox() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("guide.md"), "# 使用指南\n正文").unwrap();
        std::fs::create_dir_all(root.join("_inbox/conv_x")).unwrap();
        std::fs::write(root.join("_inbox/conv_x/draft.md"), "# 草稿").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/notes.md"), "no heading here").unwrap();

        let toc = build_toc(root).await;
        assert_eq!(toc.len(), 2, "{toc:?}");
        assert!(toc.contains(&"guide.md — 使用指南".to_string()));
        assert!(toc.contains(&"sub/notes.md".to_string()));
        assert!(!toc.iter().any(|l| l.contains("_inbox")));
    }

    /// `build_toc` returns the FULL listing — budgeting/aggregation happens
    /// later in `context::apply_toc_budgets` across all mounted bases, so a
    /// per-base cap here would double-truncate.
    #[tokio::test]
    async fn build_toc_returns_full_listing_without_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        for i in 0..35 {
            std::fs::write(root.join(format!("f{i:02}.md")), "x").unwrap();
        }
        let toc = build_toc(root).await;
        assert_eq!(toc.len(), 35, "{toc:?}");
        assert!(!toc.iter().any(|l| l.contains("more files")), "{toc:?}");
    }

    #[tokio::test]
    async fn build_toc_orders_index_and_shallow_first() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("aaa")).unwrap();
        std::fs::write(root.join("aaa/early.md"), "# Early\n").unwrap();
        std::fs::write(root.join("zzz.md"), "# Zzz\n").unwrap();
        std::fs::write(root.join("overview.md"), "# Overview\n").unwrap();
        let toc = build_toc(root).await;
        assert!(toc[0].starts_with("overview.md"), "index file first: {toc:?}");
        let shallow = toc.iter().position(|l| l.starts_with("zzz.md")).unwrap();
        let deep = toc.iter().position(|l| l.starts_with("aaa/early.md")).unwrap();
        assert!(shallow < deep, "shallow zzz.md before deep aaa/early.md: {toc:?}");
    }

    /// build_toc (run at session mount) must prune machinery dirs (`.obsidian/`,
    /// `.git/`): those files are not notes and, on a NAS vault, dominate the
    /// mount-time walk that opens every file for its first heading.
    #[tokio::test]
    async fn build_toc_skips_machinery_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".obsidian")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("note.md"), "# Note\n").unwrap();
        std::fs::write(root.join(".obsidian/workspace.md"), "# machinery\n").unwrap();
        std::fs::write(root.join(".git/COMMIT_EDITMSG.md"), "# machinery\n").unwrap();

        let toc = build_toc(root).await;
        assert_eq!(toc.len(), 1, "only the real note belongs in the toc: {toc:?}");
        assert!(toc[0].starts_with("note.md"), "{toc:?}");
    }

    /// search_bases must not index files under machinery dirs (`.obsidian/`,
    /// `.git/`) — vault plumbing is never a knowledge document.
    #[tokio::test]
    async fn search_bases_skips_machinery_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        std::fs::write(vault.join("real.md"), "# Real\n关于部署的说明").unwrap();
        std::fs::write(vault.join(".obsidian/plugin.md"), "# 机器\n关于部署的说明").unwrap();
        let kb = service
            .create_base("v", "", Some(vault.to_str().unwrap()), None)
            .await
            .unwrap();

        let hits = service.search_bases(&[kb.id], "部署", 10).await.unwrap();
        let rels: Vec<&str> = hits.iter().map(|h| h.rel_path.as_str()).collect();
        assert_eq!(rels, vec!["real.md"], "machinery-dir files must not be searchable: {rels:?}");
    }

    #[test]
    fn toc_rank_prioritizes_index_then_depth() {
        assert!(toc_rank("README.md") < toc_rank("alpha.md"));
        assert!(toc_rank("overview.md") < toc_rank("aaa/early.md"));
        assert!(toc_rank("shallow.md") < toc_rank("a/deep.md"));
        assert!(toc_rank("docs/readme.md") < toc_rank("docs/other.md"));
    }

    #[test]
    fn readme_summary_takes_first_paragraph_and_truncates() {
        // Headings (and blank lines) before the first paragraph are skipped;
        // the paragraph's lines are joined; a following heading/paragraph is
        // not included.
        let text = "# 领域知识\n\nCovers deployment flows\nand on-call runbooks.\n\n## Layout\nmore text";
        assert_eq!(
            extract_readme_summary(text).as_deref(),
            Some("Covers deployment flows and on-call runbooks.")
        );

        // Heading directly after the paragraph also terminates it.
        let text = "Intro paragraph.\n# Heading\nbody";
        assert_eq!(extract_readme_summary(text).as_deref(), Some("Intro paragraph."));

        // No paragraph at all → None.
        assert_eq!(extract_readme_summary("# Only a title\n\n## And a section\n"), None);
        assert_eq!(extract_readme_summary(""), None);

        // Truncated to SUMMARY_MAX_CHARS on a char boundary, with an explicit
        // truncation marker appended.
        let long = "知".repeat(500);
        let summary = extract_readme_summary(&long).unwrap();
        assert_eq!(summary.chars().count(), SUMMARY_MAX_CHARS + 1, "400 chars + ellipsis");
        assert!(summary.ends_with('…'), "truncation must be marked: …{}", &summary[summary.len() - 9..]);

        // Exactly at the cap → kept whole, no marker.
        let exact = "k".repeat(SUMMARY_MAX_CHARS);
        assert_eq!(extract_readme_summary(&exact).as_deref(), Some(exact.as_str()));
    }

    /// Badge rows (`[![…`) and raw HTML lines (`<…`) are README boilerplate,
    /// not prose — they must not become the summary; the first REAL paragraph
    /// after them wins.
    #[test]
    fn readme_summary_skips_badge_and_html_noise() {
        let text = "# Repo\n\n\
                    [![CI](https://img.shields.io/badge/ci-pass-green)](https://ci.example.com)\n\
                    <p align=\"center\"><img src=\"logo.png\" /></p>\n\
                    \n\
                    The real first paragraph.\n\n## Next\n";
        assert_eq!(extract_readme_summary(text).as_deref(), Some("The real first paragraph."));

        // Noise directly glued to the paragraph (no blank line in between)
        // still yields the prose only.
        let glued = "[![badge](x)](y)\n<div>\n实际描述在这里。\n";
        assert_eq!(extract_readme_summary(glued).as_deref(), Some("实际描述在这里。"));

        // A README of nothing but badges/HTML has no summary.
        assert_eq!(extract_readme_summary("[![CI](x)](y)\n<hr/>\n"), None);
    }

    #[tokio::test]
    async fn base_summary_read_from_readme_first_paragraph() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        // No README yet (autogen lands later) → None.
        assert_eq!(read_base_summary(root).await, None);

        std::fs::write(root.join("README.md"), "# 库\n\n这套库覆盖部署与运维流程。\n\n## 结构\n…").unwrap();
        assert_eq!(read_base_summary(root).await.as_deref(), Some("这套库覆盖部署与运维流程。"));
    }

    /// README detection is case-insensitive: on case-sensitive filesystems a
    /// `readme.md` (or any other casing) must be found and read.
    #[tokio::test]
    async fn base_summary_reads_lowercase_readme_variant() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("readme.md"), "# 库\n\n小写文件名也必须能读到。\n").unwrap();
        assert_eq!(read_base_summary(root).await.as_deref(), Some("小写文件名也必须能读到。"));

        let dir2 = tempfile::TempDir::new().unwrap();
        std::fs::write(dir2.path().join("ReadMe.MD"), "混合大小写同样命中。\n").unwrap();
        assert_eq!(read_base_summary(dir2.path()).await.as_deref(), Some("混合大小写同样命中。"));
    }

    /// Autogen's "README already exists" check must also be case-insensitive:
    /// an existing `readme.md` blocks the non-overwrite path, and the
    /// overwrite path rewrites THAT file instead of creating a parallel
    /// `README.md` next to it.
    #[tokio::test]
    async fn autogen_readme_detection_is_case_insensitive() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let kb = service.create_base("库", "", None, None).await.unwrap();
        service.write_file(&kb.id, "a.md", "# A").await.unwrap();
        let root = PathBuf::from(&kb.root_path);
        std::fs::write(root.join("readme.md"), "# 手写 readme\n保留我").unwrap();
        service.set_completer(FakeCompleter::new(OVERVIEW_JSON, ""));

        let readmes = |root: &Path| -> Vec<PathBuf> {
            std::fs::read_dir(root)
                .unwrap()
                .flatten()
                .filter(|e| {
                    e.file_name().to_str().is_some_and(|n| n.eq_ignore_ascii_case("README.md"))
                })
                .map(|e| e.path())
                .collect()
        };

        // overwrite=false: the lowercase README counts as existing → untouched.
        let outcome = service.generate_overview(&kb.id, false, None).await.unwrap();
        assert!(!outcome.readme_written, "existing readme.md must block the non-overwrite write");
        let found = readmes(&root);
        assert_eq!(found.len(), 1, "no parallel README.md may appear: {found:?}");
        assert_eq!(std::fs::read_to_string(&found[0]).unwrap(), "# 手写 readme\n保留我");

        // overwrite=true: rewrites in place — still exactly one readme file.
        let outcome = service.generate_overview(&kb.id, true, None).await.unwrap();
        assert!(outcome.readme_written);
        let found = readmes(&root);
        assert_eq!(found.len(), 1, "overwrite must hit the existing file: {found:?}");
        assert!(std::fs::read_to_string(&found[0]).unwrap().starts_with("# 接口库"));
    }

    #[test]
    fn link_names_sanitize_and_dedupe() {
        let mut used = HashSet::new();
        let row_a = KnowledgeBaseRow {
            id: "kb_aaaaaa".into(),
            name: "领域/知识:v1".into(),
            description: String::new(),
            root_path: String::new(),
            managed: true,
            extra: "{}".into(),
            created_at: 0,
            updated_at: 0,
            tags: None,
        };
        let name_a = unique_link_name(&row_a, &mut used);
        assert_eq!(name_a, "领域_知识_v1");

        let row_b = KnowledgeBaseRow {
            id: "kb_bbbbbb".into(),
            ..row_a.clone()
        };
        let name_b = unique_link_name(&row_b, &mut used);
        assert_ne!(name_a, name_b);
        assert!(name_b.starts_with("领域_知识_v1-"));
    }

    /// A base named like a platform-managed companion file (`README.md`,
    /// `.gitignore` — any casing on Windows) must not mount under that name:
    /// the sweep exempts those names, so the link would collide with the
    /// managed file.
    #[test]
    fn link_names_avoid_managed_companion_files() {
        for name in ["README.md", "readme.MD", ".gitignore"] {
            let mut used = HashSet::new();
            let row = KnowledgeBaseRow {
                id: "kb_cccccc".into(),
                name: name.into(),
                description: String::new(),
                root_path: String::new(),
                managed: true,
                extra: "{}".into(),
                created_at: 0,
                updated_at: 0,
                tags: None,
            };
            let link = unique_link_name(&row, &mut used);
            assert!(
                !mount::MANAGED_KEEP.iter().any(|k| k.eq_ignore_ascii_case(&link)),
                "{name} → {link}"
            );
            assert!(link.starts_with(name) && link.ends_with("cccccc"), "{name} → {link}");
        }
    }

    // ── AI autogen ───────────────────────────────────────────────────

    use crate::testutil::{MemRepo, NoopBroadcaster, make_service};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Branches on the system prompt: overview calls get strict JSON,
    /// snapshot-compression calls get plain markdown.
    struct FakeCompleter {
        overview_reply: String,
        compress_reply: String,
        calls: AtomicUsize,
    }

    impl FakeCompleter {
        fn new(overview_reply: &str, compress_reply: &str) -> Arc<Self> {
            Arc::new(Self {
                overview_reply: overview_reply.to_owned(),
                compress_reply: compress_reply.to_owned(),
                calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait::async_trait]
    impl KnowledgeCompleter for FakeCompleter {
        async fn complete(&self, system: &str, _user: &str) -> Result<String, AppError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if system == autogen::OVERVIEW_SYSTEM {
                Ok(self.overview_reply.clone())
            } else {
                Ok(self.compress_reply.clone())
            }
        }
    }

    const OVERVIEW_JSON: &str =
        r##"{"description":"AI 生成的描述","readme_markdown":"# 接口库\n\n这套库覆盖外部接口文档。"}"##;

    #[tokio::test]
    async fn generate_overview_writes_description_and_readme() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let kb = service.create_base("接口库", "", None, None).await.unwrap();
        service.write_file(&kb.id, "api.md", "# API\n说明").await.unwrap();
        service.set_completer(FakeCompleter::new(OVERVIEW_JSON, ""));

        let outcome = service.generate_overview(&kb.id, false, None).await.unwrap();
        assert!(outcome.readme_written);
        assert!(outcome.description_updated);
        assert_eq!(outcome.description, "AI 生成的描述");
        assert_eq!(outcome.base.description, "AI 生成的描述");
        let readme = std::fs::read_to_string(PathBuf::from(&kb.root_path).join("README.md")).unwrap();
        assert!(readme.starts_with("# 接口库"), "got: {readme}");

        // Long descriptions are clamped to DESCRIPTION_MAX_CHARS.
        let long = format!(r##"{{"description":"{}","readme_markdown":"# X"}}"##, "知".repeat(300));
        service.set_completer(FakeCompleter::new(&long, ""));
        let outcome = service.generate_overview(&kb.id, true, None).await.unwrap();
        assert_eq!(outcome.description.chars().count(), autogen::DESCRIPTION_MAX_CHARS);
    }

    /// Regression (NAS load-failure root cause): a base rooted on a real
    /// Obsidian-style vault carries a `.obsidian/` (plugins/cache) tree and
    /// often a `.git/` tree whose `.md` files are machinery, not knowledge
    /// documents. row_to_info's file_count/total_size walk and list_files must
    /// PRUNE dot-directories before stat'ing — otherwise a large `.obsidian/`
    /// inflates the per-base directory walk (which, on a slow NAS mount, blows
    /// past the client request timeout and surfaces as "加载失败") and pollutes
    /// the counts/listing with non-notes.
    #[tokio::test]
    async fn dotdir_files_are_pruned_from_counts_and_listing() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));

        let vault = dir.path().join("vault");
        std::fs::create_dir_all(vault.join("sub")).unwrap();
        std::fs::create_dir_all(vault.join(".obsidian/plugins")).unwrap();
        std::fs::create_dir_all(vault.join(".git")).unwrap();
        std::fs::write(vault.join("note.md"), "# Note").unwrap();
        std::fs::write(vault.join("sub/real.md"), "# Real").unwrap();
        std::fs::write(vault.join(".obsidian/plugins/data.md"), "x").unwrap();
        std::fs::write(vault.join(".git/COMMIT.md"), "x").unwrap();

        let info = service
            .create_base("vault", "", Some(vault.to_str().unwrap()), None)
            .await
            .unwrap();

        // Only the two real notes are knowledge documents; the .obsidian/.git
        // markdown must never be counted or walked.
        assert_eq!(info.file_count, 2, "dot-dir markdown must not inflate file_count");

        let files = service.list_files(&info.id).await.unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(
            rels,
            vec!["note.md", "sub/real.md"],
            "dot-dir files must not appear in the document listing"
        );
    }

    #[tokio::test]
    async fn tree_listing_shows_real_dirs_and_markdown_files_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));

        let vault = dir.path().join("vault");
        std::fs::create_dir_all(vault.join("raw")).unwrap();
        std::fs::create_dir_all(vault.join("empty")).unwrap();
        std::fs::create_dir_all(vault.join("assets")).unwrap();
        std::fs::create_dir_all(vault.join("_inbox/conv_x")).unwrap();
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        std::fs::create_dir_all(vault.join("node_modules/pkg")).unwrap();
        std::fs::write(vault.join("README.md"), "# Root").unwrap();
        std::fs::write(vault.join("raw/python3-type-conversion.md"), "# Types").unwrap();
        std::fs::write(vault.join("assets/logo.png"), "png").unwrap();
        std::fs::write(vault.join("_inbox/conv_x/draft.md"), "# Draft").unwrap();
        std::fs::write(vault.join(".obsidian/workspace.md"), "# Tooling").unwrap();
        std::fs::write(vault.join("node_modules/pkg/readme.md"), "# Package").unwrap();

        let info = service
            .create_base("vault", "", Some(vault.to_str().unwrap()), None)
            .await
            .unwrap();

        let root = service.list_tree(&info.id, "").await.unwrap();
        let root_names: Vec<(&str, bool, bool)> = root
            .iter()
            .map(|entry| (entry.name.as_str(), entry.is_dir, entry.is_file))
            .collect();
        assert_eq!(
            root_names,
            vec![
                ("assets", true, false),
                ("empty", true, false),
                ("raw", true, false),
                ("README.md", false, true),
            ]
        );

        let raw = service.list_tree(&info.id, "raw").await.unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].rel_path, "raw/python3-type-conversion.md");
        assert!(raw[0].is_file);

        let empty = service.list_tree(&info.id, "empty").await.unwrap();
        assert!(
            empty.is_empty(),
            "empty folders should be expandable but list no children"
        );
        assert!(service.list_tree(&info.id, "../escape").await.is_err());
    }

    #[tokio::test]
    async fn create_folder_creates_real_empty_folder_visible_in_tree() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));

        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        std::fs::write(vault.join("README.md"), "# Root").unwrap();

        let info = service
            .create_base("vault", "", Some(vault.to_str().unwrap()), None)
            .await
            .unwrap();

        service.create_folder(&info.id, "raw/tutorials").await.unwrap();
        assert!(vault.join("raw/tutorials").is_dir());

        let raw = service.list_tree(&info.id, "raw").await.unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].rel_path, "raw/tutorials");
        assert!(raw[0].is_dir);

        assert!(service.create_folder(&info.id, "").await.is_err());
        assert!(service.create_folder(&info.id, "../escape").await.is_err());
        assert!(service.create_folder(&info.id, "_inbox/draft").await.is_err());
        assert!(service.create_folder(&info.id, "node_modules/pkg").await.is_err());
        assert!(service.create_folder(&info.id, "README.md/child").await.is_err());
    }

    #[tokio::test]
    async fn delete_folder_removes_visible_markdown_tree_and_rejects_unsafe_targets() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));

        let vault = dir.path().join("vault");
        std::fs::create_dir_all(vault.join("docs/nested")).unwrap();
        std::fs::write(vault.join("docs/README.md"), "# Docs").unwrap();
        std::fs::write(vault.join("docs/nested/topic.md"), "# Topic").unwrap();
        std::fs::write(vault.join("root.md"), "# Root").unwrap();

        let info = service
            .create_base("vault", "", Some(vault.to_str().unwrap()), None)
            .await
            .unwrap();

        service.delete_folder(&info.id, "docs").await.unwrap();
        assert!(!vault.join("docs").exists());
        assert!(vault.join("root.md").exists());

        assert!(service.delete_folder(&info.id, "").await.is_err());
        assert!(service.delete_folder(&info.id, "../escape").await.is_err());
        assert!(service.delete_folder(&info.id, "root.md").await.is_err());
        assert!(service.delete_folder(&info.id, "_inbox").await.is_err());
        assert!(service.delete_folder(&info.id, "node_modules").await.is_err());
    }

    #[tokio::test]
    async fn rename_tree_entry_renames_files_and_folders_within_the_same_parent() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));

        let vault = dir.path().join("vault");
        std::fs::create_dir_all(vault.join("docs/nested")).unwrap();
        std::fs::write(vault.join("docs/old.md"), "# Old").unwrap();
        std::fs::write(vault.join("docs/nested/topic.md"), "# Topic").unwrap();
        std::fs::write(vault.join("taken.md"), "# Taken").unwrap();
        std::fs::write(vault.join("existing.md"), "# Existing").unwrap();

        let info = service
            .create_base("vault", "", Some(vault.to_str().unwrap()), None)
            .await
            .unwrap();

        let file = service
            .rename_tree_entry(&info.id, "docs/old.md", "new.md")
            .await
            .unwrap();
        assert_eq!(file.rel_path, "docs/new.md");
        assert!(file.is_file);
        assert!(vault.join("docs/new.md").is_file());
        assert!(!vault.join("docs/old.md").exists());

        let folder = service
            .rename_tree_entry(&info.id, "docs/nested", "renamed")
            .await
            .unwrap();
        assert_eq!(folder.rel_path, "docs/renamed");
        assert!(folder.is_dir);
        assert!(vault.join("docs/renamed/topic.md").is_file());

        assert!(service.rename_tree_entry(&info.id, "", "root").await.is_err());
        assert!(service.rename_tree_entry(&info.id, "docs/new.md", "bad.txt").await.is_err());
        assert!(service.rename_tree_entry(&info.id, "docs/new.md", "../escape.md").await.is_err());
        assert!(service.rename_tree_entry(&info.id, "docs/new.md", "renamed").await.is_err());
        assert!(service.rename_tree_entry(&info.id, "taken.md", "existing.md").await.is_err());
    }

    /// The per-base walk must be bounded: a walk that finishes within budget
    /// returns its real value, but one that exceeds it degrades to the fallback
    /// (a slow/stale NAS mount must never hang the response past the client
    /// timeout). The detached closure keeps running; its result is discarded.
    #[tokio::test]
    async fn bounded_blocking_returns_value_then_degrades_on_timeout() {
        use std::time::Duration;

        let v = bounded_blocking(Duration::from_secs(5), 999u32, || 7u32).await;
        assert_eq!(v, 7, "value must be returned when the walk finishes within budget");

        let v = bounded_blocking(Duration::from_millis(1), 999u32, || {
            std::thread::sleep(Duration::from_millis(60));
            7u32
        })
        .await;
        assert_eq!(v, 999, "a walk that exceeds the budget must degrade to the fallback");
    }

    /// `list_base_ids` returns every registered base id straight from the DB,
    /// with no directory walk — the disk-free path binding/ensure-known callers
    /// must use instead of the walking `list_bases`.
    #[tokio::test]
    async fn list_base_ids_returns_all_ids_from_registry() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let a = service.create_base("a", "", None, None).await.unwrap();
        let b = service.create_base("b", "", None, None).await.unwrap();

        let mut got = service.list_base_ids().await.unwrap();
        got.sort();
        let mut expected = vec![a.id, b.id];
        expected.sort();
        assert_eq!(got, expected);
    }

    /// The concurrent `list_bases` must still return EVERY base and preserve
    /// registry order (guards the `.buffered` parallelization against dropping
    /// or reordering rows).
    #[tokio::test]
    async fn list_bases_returns_every_base_in_registry_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let a = service.create_base("a", "", None, None).await.unwrap();
        let b = service.create_base("b", "", None, None).await.unwrap();
        let c = service.create_base("c", "", None, None).await.unwrap();

        let infos = service.list_bases().await.unwrap();
        let ids: Vec<&str> = infos.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec![a.id.as_str(), b.id.as_str(), c.id.as_str()]);
    }

    #[tokio::test]
    async fn generate_overview_preserves_readme_unless_overwrite() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let kb = service.create_base("库", "", None, None).await.unwrap();
        service.write_file(&kb.id, "a.md", "# A").await.unwrap();
        let readme_path = PathBuf::from(&kb.root_path).join("README.md");
        std::fs::write(&readme_path, "# 手写 README\n保留我").unwrap();
        service.set_completer(FakeCompleter::new(OVERVIEW_JSON, ""));

        // overwrite_readme=false: README untouched, description refreshed.
        let outcome = service.generate_overview(&kb.id, false, None).await.unwrap();
        assert!(!outcome.readme_written);
        assert!(outcome.description_updated);
        assert_eq!(std::fs::read_to_string(&readme_path).unwrap(), "# 手写 README\n保留我");

        // overwrite_readme=true replaces it.
        let outcome = service.generate_overview(&kb.id, true, None).await.unwrap();
        assert!(outcome.readme_written);
        assert!(std::fs::read_to_string(&readme_path).unwrap().starts_with("# 接口库"));

        // preserve_existing_description (post-import mode) keeps a non-empty
        // description but can still write a (missing) README.
        std::fs::remove_file(&readme_path).unwrap();
        service.update_base(&kb.id, None, Some("人工描述"), None).await.unwrap();
        let outcome = service.generate_overview_opts(&kb.id, false, true, None).await.unwrap();
        assert!(outcome.readme_written);
        assert!(!outcome.description_updated);
        assert_eq!(outcome.base.description, "人工描述");
    }

    #[tokio::test]
    async fn generate_overview_requires_completer_and_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let kb = service.create_base("空库", "", None, None).await.unwrap();

        // No completer wired → explicit 409.
        let err = service.generate_overview(&kb.id, false, None).await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "{err:?}");
        assert!(err.to_string().contains("no AI completer"), "{err}");

        // Completer wired but the base has no documents → 400.
        service.set_completer(FakeCompleter::new(OVERVIEW_JSON, ""));
        let err = service.generate_overview(&kb.id, false, None).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");

        // Garbage model output (twice) → BadGateway, nothing persisted.
        service.write_file(&kb.id, "a.md", "# A").await.unwrap();
        service.set_completer(FakeCompleter::new("我不会输出 JSON", ""));
        let err = service.generate_overview(&kb.id, false, None).await.unwrap_err();
        assert!(matches!(err, AppError::BadGateway(_)), "{err:?}");
        assert!(!PathBuf::from(&kb.root_path).join("README.md").exists());
        assert_eq!(service.get_base_info(&kb.id).await.unwrap().description, "");
    }

    // ── Stateless description endpoints ──────────────────────────────

    /// Replays scripted replies in order (last one repeats) and records the
    /// prompts; for the stateless description generate/polish paths. Also
    /// records the explicit `(provider_id, model)` of the most recent call:
    /// `Some` when reached via `complete_with`, `None` via `complete`.
    struct ScriptedCompleter {
        replies: std::sync::Mutex<Vec<String>>,
        calls: AtomicUsize,
        last_system: std::sync::Mutex<String>,
        last_user: std::sync::Mutex<String>,
        last_override: std::sync::Mutex<Option<(String, String)>>,
    }

    impl ScriptedCompleter {
        fn new(replies: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                replies: std::sync::Mutex::new(replies.iter().rev().map(|r| (*r).to_owned()).collect()),
                calls: AtomicUsize::new(0),
                last_system: std::sync::Mutex::new(String::new()),
                last_user: std::sync::Mutex::new(String::new()),
                last_override: std::sync::Mutex::new(None),
            })
        }

        fn next_reply(&self) -> String {
            let mut replies = self.replies.lock().unwrap();
            if replies.len() > 1 { replies.pop().unwrap() } else { replies.last().cloned().unwrap_or_default() }
        }
    }

    #[async_trait::async_trait]
    impl KnowledgeCompleter for ScriptedCompleter {
        async fn complete(&self, system: &str, user: &str) -> Result<String, AppError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_system.lock().unwrap() = system.to_owned();
            *self.last_user.lock().unwrap() = user.to_owned();
            *self.last_override.lock().unwrap() = None;
            Ok(self.next_reply())
        }

        async fn complete_with(
            &self,
            system: &str,
            user: &str,
            provider_id: &str,
            model: &str,
        ) -> Result<String, AppError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_system.lock().unwrap() = system.to_owned();
            *self.last_user.lock().unwrap() = user.to_owned();
            *self.last_override.lock().unwrap() = Some((provider_id.to_owned(), model.to_owned()));
            Ok(self.next_reply())
        }
    }

    /// Explicit `Some((provider_id, model))` must reach the completer via
    /// `complete_with` carrying exactly that pick — proving the UI's model
    /// selection is threaded through the description/polish service methods
    /// (it is NOT silently dropped or replaced by the default).
    #[tokio::test]
    async fn explicit_model_override_reaches_completer() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.md"), "# A\n正文").unwrap();

        let completer = ScriptedCompleter::new(&[r#"{"description":"覆盖A，查阅时用。"}"#]);
        service.set_completer(completer.clone());

        let pick = Some(("prov-2".to_owned(), "model-x".to_owned()));
        let description = service
            .generate_description_for_path("库", &docs.to_string_lossy(), pick.clone())
            .await
            .unwrap();
        assert_eq!(description, "覆盖A，查阅时用。");
        assert_eq!(
            *completer.last_override.lock().unwrap(),
            Some(("prov-2".to_owned(), "model-x".to_owned())),
            "the explicit (provider_id, model) must reach complete_with verbatim"
        );

        // polish_description threads it the same way.
        let completer = ScriptedCompleter::new(&[r#"{"description":"润色结果。"}"#]);
        service.set_completer(completer.clone());
        service
            .polish_description("库", "草稿", Some(("prov-9".to_owned(), "m-9".to_owned())))
            .await
            .unwrap();
        assert_eq!(
            *completer.last_override.lock().unwrap(),
            Some(("prov-9".to_owned(), "m-9".to_owned()))
        );
    }

    /// `None` must fall back to the completer's default model: the call
    /// arrives via `complete` (no recorded override) — confirming existing
    /// behavior is byte-for-byte unchanged when no model is picked.
    #[tokio::test]
    async fn none_override_falls_back_to_default_model() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.md"), "# A\n正文").unwrap();

        let completer = ScriptedCompleter::new(&[r#"{"description":"默认模型生成。"}"#]);
        service.set_completer(completer.clone());

        let description = service
            .generate_description_for_path("库", &docs.to_string_lossy(), None)
            .await
            .unwrap();
        assert_eq!(description, "默认模型生成。");
        assert_eq!(
            *completer.last_override.lock().unwrap(),
            None,
            "None must route through complete() — the default-model path"
        );
    }

    #[tokio::test]
    async fn generate_description_for_path_is_stateless() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("deploy.md"), "# 部署\n发布流程说明").unwrap();

        let completer = ScriptedCompleter::new(&[r#"{"description":"覆盖部署与发布流程，上线/排障时查阅。"}"#]);
        service.set_completer(completer.clone());

        let description = service
            .generate_description_for_path("发布手册", &docs.to_string_lossy(), None)
            .await
            .unwrap();
        assert_eq!(description, "覆盖部署与发布流程，上线/排障时查阅。");
        // Description-only contract: the dedicated system prompt is used and
        // the user prompt carries the name and the sampled file.
        assert_eq!(*completer.last_system.lock().unwrap(), autogen::DESCRIPTION_SYSTEM);
        let user = completer.last_user.lock().unwrap().clone();
        assert!(user.contains("发布手册") && user.contains("--- FILE: deploy.md ---"), "{user}");
        // Stateless: no base row was created, nothing written to the dir.
        assert!(service.list_bases().await.unwrap().is_empty());
        assert!(!docs.join("README.md").exists());

        // Clamp: an over-long model description is cut to the shared cap.
        service.set_completer(ScriptedCompleter::new(&[&format!(
            r#"{{"description":"{}"}}"#,
            "知".repeat(300)
        )]));
        let description = service
            .generate_description_for_path("", &docs.to_string_lossy(), None)
            .await
            .unwrap();
        assert_eq!(description.chars().count(), autogen::DESCRIPTION_MAX_CHARS);
    }

    #[tokio::test]
    async fn generate_description_for_path_validates_input() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();

        // No completer wired → same 409 as the kb-bound autogen.
        let err = service.generate_description_for_path("x", &docs.to_string_lossy(), None).await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "{err:?}");
        assert!(err.to_string().contains("no AI completer"), "{err}");

        service.set_completer(ScriptedCompleter::new(&[r#"{"description":"d"}"#]));
        // Empty / relative / missing root paths → 400.
        for bad in ["", "  ", "relative/path"] {
            let err = service.generate_description_for_path("x", bad, None)
            .await.unwrap_err();
            assert!(matches!(err, AppError::BadRequest(_)), "{bad:?} → {err:?}");
        }
        let missing = dir.path().join("nope");
        let err = service.generate_description_for_path("x", &missing.to_string_lossy(), None).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
        // Existing dir without markdown → 400.
        let err = service.generate_description_for_path("x", &docs.to_string_lossy(), None).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
    }

    #[tokio::test]
    async fn generate_description_retries_once_then_bad_gateway() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.md"), "# A").unwrap();

        // First reply unparseable, second good → succeeds with two calls.
        let completer = ScriptedCompleter::new(&["我不会输出 JSON", r#"{"description":"第二次成功"}"#]);
        service.set_completer(completer.clone());
        let description = service.generate_description_for_path("", &docs.to_string_lossy(), None)
            .await.unwrap();
        assert_eq!(description, "第二次成功");
        assert_eq!(completer.calls.load(Ordering::SeqCst), 2);

        // Garbage twice → 502, exactly two attempts.
        let completer = ScriptedCompleter::new(&["nope"]);
        service.set_completer(completer.clone());
        let err = service.generate_description_for_path("", &docs.to_string_lossy(), None)
            .await.unwrap_err();
        assert!(matches!(err, AppError::BadGateway(_)), "{err:?}");
        assert_eq!(completer.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn polish_description_rewrites_draft_statelessly() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));

        // No completer wired → 409.
        let err = service.polish_description("库", "草稿", None).await.unwrap_err();
        assert!(matches!(err, AppError::Conflict(_)), "{err:?}");

        let completer = ScriptedCompleter::new(&[r#"{"description":"覆盖部署流程与排障要点，发布前后查阅。"}"#]);
        service.set_completer(completer.clone());

        // Empty draft → 400 (completer never called).
        let err = service.polish_description("库", "   ", None).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
        assert_eq!(completer.calls.load(Ordering::SeqCst), 0);

        let description = service.polish_description("运维库", "  记录部署和排障  ", None).await.unwrap();
        assert_eq!(description, "覆盖部署流程与排障要点，发布前后查阅。");
        assert_eq!(*completer.last_system.lock().unwrap(), autogen::POLISH_SYSTEM);
        let user = completer.last_user.lock().unwrap().clone();
        assert!(user.contains("运维库") && user.contains("记录部署和排障"), "{user}");
        // Stateless: nothing registered.
        assert!(service.list_bases().await.unwrap().is_empty());
    }

    // ── URL sources ──────────────────────────────────────────────────

    fn url_source(mode: KnowledgeSourceMode, urls: &[&str]) -> KnowledgeSource {
        KnowledgeSource {
            kind: "url".into(),
            mode,
            entries: urls
                .iter()
                .map(|u| KnowledgeSourceEntry {
                    url: (*u).to_owned(),
                    title: None,
                    rendered: false,
                })
                .collect(),
            // A client-sent value must be discarded by create.
            last_fetched_at: Some(42),
            credential_ref: None,
            scope: None,
            sync: None,
        }
    }

    fn service_with_repo(dir: &Path) -> (KnowledgeService, Arc<MemRepo>) {
        let repo = Arc::new(MemRepo::default());
        let service = KnowledgeService::new(
            repo.clone(),
            dir,
            KnowledgeEventEmitter::new(Arc::new(NoopBroadcaster)),
        )
        .with_url_fetcher(HttpFetcher::new().allow_private_for_tests());
        (service, repo)
    }

    fn extra_source(repo: &MemRepo, kb_id: &str) -> Option<KnowledgeSource> {
        let row = repo.bases.lock().unwrap().iter().find(|r| r.id == kb_id).cloned().unwrap();
        source_from_extra(&row.extra)
    }

    #[tokio::test]
    async fn create_validates_source_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let (service, _repo) = service_with_repo(&dir.path().join("data"));

        let mut bad_kind = url_source(KnowledgeSourceMode::Live, &["https://e.com"]);
        bad_kind.kind = "carrier-pigeon".into();
        assert!(service.create_base("x", "", None, Some(bad_kind)).await.is_err());

        let empty = url_source(KnowledgeSourceMode::Live, &[]);
        assert!(service.create_base("x", "", None, Some(empty)).await.is_err());

        let ftp = url_source(KnowledgeSourceMode::Live, &["ftp://e.com/x"]);
        assert!(service.create_base("x", "", None, Some(ftp)).await.is_err());

        assert!(service.list_bases().await.unwrap().is_empty(), "rejected creates must not register");
    }

    #[tokio::test]
    async fn create_live_source_stores_extra_and_fills_mounts() {
        let dir = tempfile::TempDir::new().unwrap();
        let (service, repo) = service_with_repo(&dir.path().join("data"));

        let mut source = url_source(KnowledgeSourceMode::Live, &["https://example.com/api-docs"]);
        source.entries[0].title = Some("API docs".into());
        let kb = service.create_base("接口库", "", None, Some(source)).await.unwrap();

        // Live mode never fetches: no snapshots, no fetch stamp, no
        // create-time fetch summary.
        assert!(!PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR).exists());
        assert!(kb.source_fetch.is_none(), "live create must not report a fetch");
        let stored = extra_source(&repo, &kb.id).expect("source stored in extra");
        assert_eq!(stored.mode, KnowledgeSourceMode::Live);
        assert_eq!(stored.last_fetched_at, None, "client-sent stamp must be discarded");

        // extra.source(live) → mounts.live_sources.
        service
            .set_binding(
                "conversation",
                "1",
                KnowledgeBinding {
                    enabled: true,
                    kb_ids: vec![kb.id.clone()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let outcome = service.ensure_mounts_for_target("conversation", "1", &ws).await;
        assert_eq!(outcome.mounts.len(), 1);
        let live = &outcome.mounts[0].live_sources;
        assert_eq!(live.len(), 1, "{live:?}");
        assert_eq!(live[0].url, "https://example.com/api-docs");
        assert_eq!(live[0].title.as_deref(), Some("API docs"));
    }

    /// `KnowledgeBaseInfo` must carry `extra.source` on get/list — the
    /// frontend detail page renders mode / URL count / lastFetchedAt from
    /// it (it probes `base.source`, so the key must be present).
    #[tokio::test]
    async fn base_info_carries_source_on_get_and_list() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("snapshot body"))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, repo) = service_with_repo(&dir.path().join("data"));
        let url = format!("{}/doc", server.uri());
        let kb = service
            .create_base("有源库", "", None, Some(url_source(KnowledgeSourceMode::Snapshot, &[&url])))
            .await
            .unwrap();
        let stamp = extra_source(&repo, &kb.id)
            .unwrap()
            .last_fetched_at
            .expect("snapshot create stamps last_fetched_at");

        // The create response is built from the re-read row → already stamped.
        assert_eq!(kb.source.as_ref().expect("create carries source").last_fetched_at, Some(stamp));

        let got = service.get_base_info(&kb.id).await.unwrap();
        let src = got.source.as_ref().expect("get carries source");
        assert_eq!(src.mode, KnowledgeSourceMode::Snapshot);
        assert_eq!(src.entries.len(), 1);
        assert_eq!(src.entries[0].url, url);
        assert_eq!(src.last_fetched_at, Some(stamp));

        let listed = service.list_bases().await.unwrap();
        let src = listed
            .iter()
            .find(|b| b.id == kb.id)
            .and_then(|b| b.source.as_ref())
            .expect("list carries source");
        assert_eq!(src.mode, KnowledgeSourceMode::Snapshot);
        assert_eq!(src.entries.len(), 1);
        assert_eq!(src.last_fetched_at, Some(stamp));

        // Wire shape: nested source keeps its camelCase contract.
        let v = serde_json::to_value(&got).unwrap();
        assert_eq!(v["source"]["mode"], "snapshot");
        assert_eq!(v["source"]["entries"].as_array().unwrap().len(), 1);
        assert_eq!(v["source"]["lastFetchedAt"], stamp);
    }

    /// A plain directory base has no URL source — the `source` key must
    /// stay off the wire entirely, not serialize as `null`.
    #[tokio::test]
    async fn base_info_without_source_keeps_key_off_the_wire() {
        let dir = tempfile::TempDir::new().unwrap();
        let (service, _repo) = service_with_repo(&dir.path().join("data"));
        let kb = service.create_base("无源库", "", None, None).await.unwrap();

        let got = service.get_base_info(&kb.id).await.unwrap();
        assert!(got.source.is_none());
        let v = serde_json::to_value(&got).unwrap();
        assert!(v.get("source").is_none(), "no-source base must not serialize the key: {v}");
    }

    #[tokio::test]
    async fn create_snapshot_source_fetches_and_chains_autogen() {
        use wiremock::matchers::{method, path as urlpath};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(urlpath("/docs/guide"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "<html><head><title>接口文档</title></head><body><h1>API</h1><p>说明文字</p></body></html>",
                "text/html; charset=utf-8",
            ))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, repo) = service_with_repo(&dir.path().join("data"));
        service.set_completer(FakeCompleter::new(OVERVIEW_JSON, ""));

        let url = format!("{}/docs/guide", server.uri());
        let kb = service
            .create_base("接口库", "", None, Some(url_source(KnowledgeSourceMode::Snapshot, &[&url])))
            .await
            .unwrap();

        // Snapshot landed with frontmatter.
        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);
        let snaps: Vec<_> = std::fs::read_dir(&snap_dir).unwrap().flatten().collect();
        assert_eq!(snaps.len(), 1, "{snaps:?}");
        let content = std::fs::read_to_string(snaps[0].path()).unwrap();
        assert!(content.starts_with(&format!("---\nsource_url: {url}\nfetched_at: ")), "got: {content}");
        assert!(content.contains("# API"), "got: {content}");

        // Source stamped + entry title backfilled from <title>.
        let stored = extra_source(&repo, &kb.id).unwrap();
        assert!(stored.last_fetched_at.is_some());
        assert_eq!(stored.entries[0].title.as_deref(), Some("接口文档"));

        // Chained autogen: description + README, returned info is final.
        assert_eq!(kb.description, "AI 生成的描述");
        let readme = std::fs::read_to_string(PathBuf::from(&kb.root_path).join("README.md")).unwrap();
        assert!(readme.starts_with("# 接口库"), "got: {readme}");
    }

    #[tokio::test]
    async fn create_snapshot_without_completer_is_silent_and_dedupes_slugs() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("plain body"))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, repo) = service_with_repo(&dir.path().join("data"));
        // No completer wired: snapshots still land, autogen silently skipped.
        let url = format!("{}/page", server.uri());
        let kb = service
            .create_base(
                "双份库",
                "",
                None,
                Some(url_source(KnowledgeSourceMode::Snapshot, &[&url, &url])),
            )
            .await
            .unwrap();

        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);
        let mut names: Vec<String> = std::fs::read_dir(&snap_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 2, "same-slug entries must suffix, not overwrite: {names:?}");
        assert!(names.iter().any(|n| n.ends_with("-2.md")), "{names:?}");
        assert!(!PathBuf::from(&kb.root_path).join("README.md").exists(), "no completer → no README");
        assert_eq!(kb.description, "");
        assert!(extra_source(&repo, &kb.id).unwrap().last_fetched_at.is_some());
    }

    #[tokio::test]
    async fn oversized_snapshot_compressed_via_completer() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/plain")
                    .set_body_string("x".repeat(autogen::SNAPSHOT_COMPRESS_THRESHOLD + 1024)),
            )
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, _repo) = service_with_repo(&dir.path().join("data"));
        // Overview reply is garbage on purpose: the chained autogen failing
        // must not fail the create.
        service.set_completer(FakeCompleter::new("not json", "## 要点\n- 已压缩"));

        let url = format!("{}/big", server.uri());
        let kb = service
            .create_base("大页库", "", None, Some(url_source(KnowledgeSourceMode::Snapshot, &[&url])))
            .await
            .unwrap();

        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);
        let snap = std::fs::read_dir(&snap_dir).unwrap().flatten().next().unwrap();
        let content = std::fs::read_to_string(snap.path()).unwrap();
        assert!(content.contains("## 要点"), "oversized page must be condensed: {}", &content[..200.min(content.len())]);
        assert!(!content.contains("xxxxxxxxxx"), "raw body must be replaced");
    }

    /// **P3-K3 end-to-end routing**: a snapshot source with one `rendered`
    /// entry and one plain entry must write the browser-backed body for the
    /// rendered URL and the HTTP body for the plain one — proving the
    /// `entry.rendered → fetcher_for → snapshot` chain selects per-entry.
    /// Deterministic (canned render fetcher, mock HTTP server) — no real Chrome.
    #[tokio::test]
    async fn rendered_entry_uses_render_backend_per_source() {
        use crate::source_url::FetchedPage;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Render backend stamps a marker no HTTP fetch could produce.
        struct MarkerRenderFetcher;
        #[async_trait::async_trait]
        impl PageFetcher for MarkerRenderFetcher {
            async fn fetch_page(&self, raw_url: &str) -> Result<FetchedPage, AppError> {
                Ok(FetchedPage {
                    final_url: raw_url.to_owned(),
                    title: Some("Rendered".into()),
                    markdown: "RENDERED-BY-BROWSER only a headless browser sees this".into(),
                    truncated: false,
                })
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/plain").set_body_string("PLAIN-HTTP-BODY"))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, _repo) = service_with_repo(&dir.path().join("data"));
        service.set_render_fetcher(Arc::new(MarkerRenderFetcher));

        let plain_url = format!("{}/plain", server.uri());
        let rendered_url = format!("{}/spa", server.uri());
        let source = KnowledgeSource {
            kind: "url".into(),
            mode: KnowledgeSourceMode::Snapshot,
            entries: vec![
                KnowledgeSourceEntry { url: plain_url.clone(), title: None, rendered: false },
                KnowledgeSourceEntry { url: rendered_url.clone(), title: None, rendered: true },
            ],
            last_fetched_at: None,
            credential_ref: None,
            scope: None,
            sync: None,
        };
        let kb = service.create_base("混合库", "", None, Some(source)).await.unwrap();

        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);
        let mut bodies: Vec<String> = std::fs::read_dir(&snap_dir)
            .unwrap()
            .flatten()
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .collect();
        bodies.sort();
        assert_eq!(bodies.len(), 2, "both entries snapshot");
        let plain_snap = bodies.iter().find(|b| b.contains(&plain_url)).expect("plain snapshot present");
        let rendered_snap = bodies.iter().find(|b| b.contains(&rendered_url)).expect("rendered snapshot present");
        assert!(plain_snap.contains("PLAIN-HTTP-BODY"), "rendered=false entry must use HTTP: {plain_snap}");
        assert!(!plain_snap.contains("RENDERED-BY-BROWSER"), "rendered=false must NOT use browser backend");
        assert!(
            rendered_snap.contains("RENDERED-BY-BROWSER"),
            "rendered=true entry must use the wired browser backend: {rendered_snap}"
        );
    }

    /// **P3-K3 graceful fallback**: a `rendered` entry with NO render backend
    /// wired must silently fall back to HTTP and still snapshot — the flag is
    /// best-effort, never a hard failure that blocks the fetch.
    #[tokio::test]
    async fn rendered_entry_without_render_backend_falls_back_to_http() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).insert_header("content-type", "text/plain").set_body_string("HTTP-FALLBACK-BODY"))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, _repo) = service_with_repo(&dir.path().join("data"));
        // No set_render_fetcher → render backend absent.

        let url = format!("{}/spa", server.uri());
        let source = KnowledgeSource {
            kind: "url".into(),
            mode: KnowledgeSourceMode::Snapshot,
            entries: vec![KnowledgeSourceEntry { url: url.clone(), title: None, rendered: true }],
            last_fetched_at: None,
            credential_ref: None,
            scope: None,
            sync: None,
        };
        let kb = service.create_base("回退库", "", None, Some(source)).await.unwrap();

        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);
        let snap = std::fs::read_dir(&snap_dir).unwrap().flatten().next().expect("snapshot written despite no browser backend");
        let content = std::fs::read_to_string(snap.path()).unwrap();
        assert!(content.contains("HTTP-FALLBACK-BODY"), "rendered=true with no browser backend must degrade to HTTP: {content}");
    }

    #[tokio::test]
    async fn refresh_source_overwrites_snapshots_and_stamps_time() {
        use wiremock::matchers::{method, path as urlpath};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // First fetch sees v1, all later fetches see v2.
        Mock::given(method("GET"))
            .and(urlpath("/doc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("version-one"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(urlpath("/doc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("version-two"))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, repo) = service_with_repo(&dir.path().join("data"));
        let url = format!("{}/doc", server.uri());
        let kb = service
            .create_base("刷新库", "", None, Some(url_source(KnowledgeSourceMode::Snapshot, &[&url])))
            .await
            .unwrap();
        let first_stamp = extra_source(&repo, &kb.id).unwrap().last_fetched_at.unwrap();
        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);
        let snap_path = std::fs::read_dir(&snap_dir).unwrap().flatten().next().unwrap().path();
        assert!(std::fs::read_to_string(&snap_path).unwrap().contains("version-one"));

        let summary = service.refresh_source(&kb.id).await.unwrap();
        assert_eq!(summary.fetched, 1);
        assert_eq!(summary.failed, 0, "{:?}", summary.errors);
        let stamp = summary.last_fetched_at.expect("successful refresh must stamp");
        assert!(stamp >= first_stamp);
        // Same slug → overwritten in place, not duplicated.
        assert_eq!(std::fs::read_dir(&snap_dir).unwrap().flatten().count(), 1);
        assert!(std::fs::read_to_string(&snap_path).unwrap().contains("version-two"));
        assert_eq!(extra_source(&repo, &kb.id).unwrap().last_fetched_at, Some(stamp));

        // A base without a source refuses to refresh.
        let plain = service.create_base("无源库", "", None, None).await.unwrap();
        let err = service.refresh_source(&plain.id).await.unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
    }

    /// A refresh in which every entry fails must NOT pretend freshness: the
    /// old `extra.source.last_fetched_at` is kept (the snapshots on disk are
    /// still the old ones) and the summary reports that old value.
    #[tokio::test]
    async fn refresh_source_all_failed_keeps_old_stamp() {
        use wiremock::matchers::{method, path as urlpath};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // The create-time fetch succeeds once; every later fetch fails.
        Mock::given(method("GET"))
            .and(urlpath("/doc"))
            .respond_with(ResponseTemplate::new(200).set_body_string("original"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(urlpath("/doc"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, repo) = service_with_repo(&dir.path().join("data"));
        let url = format!("{}/doc", server.uri());
        let kb = service
            .create_base("失败刷新库", "", None, Some(url_source(KnowledgeSourceMode::Snapshot, &[&url])))
            .await
            .unwrap();
        let first_stamp = extra_source(&repo, &kb.id).unwrap().last_fetched_at.expect("create stamped");

        let summary = service.refresh_source(&kb.id).await.unwrap();
        assert_eq!((summary.fetched, summary.failed), (0, 1), "{:?}", summary.errors);
        assert_eq!(summary.last_fetched_at, Some(first_stamp), "summary must report the old stamp");
        assert_eq!(
            extra_source(&repo, &kb.id).unwrap().last_fetched_at,
            Some(first_stamp),
            "extra.source.lastFetchedAt must keep the old value"
        );
        // The old snapshot is untouched.
        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);
        let snap = std::fs::read_dir(&snap_dir).unwrap().flatten().next().unwrap();
        assert!(std::fs::read_to_string(snap.path()).unwrap().contains("original"));
    }

    #[tokio::test]
    async fn create_rejects_source_with_too_many_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let (service, _repo) = service_with_repo(&dir.path().join("data"));

        let urls: Vec<String> = (0..=MAX_SOURCE_ENTRIES).map(|i| format!("https://example.com/{i}")).collect();
        let refs: Vec<&str> = urls.iter().map(String::as_str).collect();
        let err = service
            .create_base("超限库", "", None, Some(url_source(KnowledgeSourceMode::Live, &refs)))
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
        assert!(
            err.to_string().contains(&MAX_SOURCE_ENTRIES.to_string()),
            "message must state the limit: {err}"
        );
        assert!(service.list_bases().await.unwrap().is_empty(), "rejected create must not register");

        // Exactly at the limit passes (live mode: nothing is fetched).
        let at_limit: Vec<&str> = urls.iter().take(MAX_SOURCE_ENTRIES).map(String::as_str).collect();
        service
            .create_base("满额库", "", None, Some(url_source(KnowledgeSourceMode::Live, &at_limit)))
            .await
            .unwrap();
    }

    /// Fetches run concurrently, but slug numbering must follow entry order:
    /// three URLs sharing one slug (same host+path, different query) land as
    /// `{slug}.md` / `{slug}-2.md` / `{slug}-3.md` matching entries 0/1/2 no
    /// matter which fetch completes first.
    #[tokio::test]
    async fn concurrent_fetch_keeps_slug_numbering_deterministic() {
        use wiremock::matchers::{method, path as urlpath};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(urlpath("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_string("body"))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, _repo) = service_with_repo(&dir.path().join("data"));
        let urls: Vec<String> = (1..=3).map(|i| format!("{}/page?v={i}", server.uri())).collect();
        let refs: Vec<&str> = urls.iter().map(String::as_str).collect();
        let kb = service
            .create_base("并发库", "", None, Some(url_source(KnowledgeSourceMode::Snapshot, &refs)))
            .await
            .unwrap();
        let summary = kb.source_fetch.as_ref().expect("snapshot create reports the fetch");
        assert_eq!((summary.fetched, summary.failed), (3, 0), "{:?}", summary.errors);

        let slug = source_url::slug_for_url(&Url::parse(&urls[0]).unwrap());
        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);
        let expected = [format!("{slug}.md"), format!("{slug}-2.md"), format!("{slug}-3.md")];
        for (i, name) in expected.iter().enumerate() {
            let content = std::fs::read_to_string(snap_dir.join(name))
                .unwrap_or_else(|e| panic!("{name} must exist: {e}"));
            assert!(
                content.contains(&format!("source_url: {}", urls[i])),
                "{name} must hold entry #{i}: {content}"
            );
        }
    }

    /// Create-time chained autogen only backfills an EMPTY description — a
    /// user-supplied one survives (the README is still generated).
    #[tokio::test]
    async fn create_autogen_preserves_user_description() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("body"))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, _repo) = service_with_repo(&dir.path().join("data"));
        service.set_completer(FakeCompleter::new(OVERVIEW_JSON, ""));

        let url = format!("{}/page", server.uri());
        let kb = service
            .create_base("手填库", "手填的描述", None, Some(url_source(KnowledgeSourceMode::Snapshot, &[&url])))
            .await
            .unwrap();
        assert_eq!(kb.description, "手填的描述", "user description must survive chained autogen");
        let readme = std::fs::read_to_string(PathBuf::from(&kb.root_path).join("README.md")).unwrap();
        assert!(readme.starts_with("# 接口库"), "README still generated: {readme}");
    }

    /// The create response surfaces the per-entry fetch outcome via the
    /// additive `source_fetch` field; list/get (and source-less creates)
    /// keep it off the wire.
    #[tokio::test]
    async fn create_response_carries_source_fetch_summary() {
        use wiremock::matchers::{method, path as urlpath};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(urlpath("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("body"))
            .mount(&server)
            .await;
        // `/missing` has no mock → wiremock answers 404 → per-entry failure.

        let dir = tempfile::TempDir::new().unwrap();
        let (service, _repo) = service_with_repo(&dir.path().join("data"));
        let good = format!("{}/ok", server.uri());
        let missing = format!("{}/missing", server.uri());
        let kb = service
            .create_base(
                "源响应库",
                "",
                None,
                Some(url_source(KnowledgeSourceMode::Snapshot, &[&good, &missing])),
            )
            .await
            .unwrap();

        let summary = kb.source_fetch.as_ref().expect("snapshot create reports the fetch");
        assert_eq!((summary.fetched, summary.failed), (1, 1), "{:?}", summary.errors);
        assert!(summary.errors[0].contains("/missing"), "{:?}", summary.errors);
        let v = serde_json::to_value(&kb).unwrap();
        assert_eq!(v["source_fetch"]["fetched"], 1);
        assert_eq!(v["source_fetch"]["failed"], 1);
        assert!(v["source_fetch"]["last_fetched_at"].is_i64(), "{v}");

        // get/list re-reads never carry it (and None stays off the wire).
        let info = service.get_base_info(&kb.id).await.unwrap();
        assert!(info.source_fetch.is_none());
        let v = serde_json::to_value(&info).unwrap();
        assert!(v.get("source_fetch").is_none(), "None must stay off the wire: {v}");

        // A source-less create reports nothing either.
        let plain = service.create_base("无源库", "", None, None).await.unwrap();
        assert!(plain.source_fetch.is_none());
    }

    /// After the entry list shrinks, a refresh must sweep snapshots whose
    /// frontmatter `source_url` no longer matches any configured entry —
    /// while user-authored files in `snapshots/` (no source_url frontmatter)
    /// stay untouched.
    #[tokio::test]
    async fn refresh_source_prunes_orphan_snapshots_but_keeps_user_files() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("body"))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let (service, repo) = service_with_repo(&dir.path().join("data"));
        let url_a = format!("{}/keep", server.uri());
        let url_b = format!("{}/drop", server.uri());
        let kb = service
            .create_base("缩减库", "", None, Some(url_source(KnowledgeSourceMode::Snapshot, &[&url_a, &url_b])))
            .await
            .unwrap();
        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);
        assert_eq!(std::fs::read_dir(&snap_dir).unwrap().flatten().count(), 2);

        // The user drops their own notes into snapshots/ (no frontmatter) —
        // plus one with frontmatter but no source_url. Both must survive.
        std::fs::write(snap_dir.join("my-notes.md"), "# 自留笔记\n手写内容").unwrap();
        std::fs::write(snap_dir.join("fm-no-url.md"), "---\ntitle: x\n---\n\n正文").unwrap();

        // Shrink the configured entries to url_a only (out-of-band config
        // change, as the routes/gateway source-update path would do).
        {
            let mut bases = repo.bases.lock().unwrap();
            let row = bases.iter_mut().find(|r| r.id == kb.id).unwrap();
            let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
            extra["source"]["entries"] = serde_json::json!([{ "url": url_a }]);
            row.extra = extra.to_string();
        }

        let summary = service.refresh_source(&kb.id).await.unwrap();
        assert_eq!((summary.fetched, summary.failed), (1, 0), "{:?}", summary.errors);

        let names: Vec<String> = std::fs::read_dir(&snap_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"my-notes.md".to_owned()), "user file must survive: {names:?}");
        assert!(names.contains(&"fm-no-url.md".to_owned()), "no-source_url file must survive: {names:?}");
        let slug_a = source_url::slug_for_url(&Url::parse(&url_a).unwrap());
        assert!(names.contains(&format!("{slug_a}.md")), "kept entry's snapshot must remain: {names:?}");
        let slug_b = source_url::slug_for_url(&Url::parse(&url_b).unwrap());
        assert!(
            !names.contains(&format!("{slug_b}.md")),
            "orphan snapshot for the removed entry must be deleted: {names:?}"
        );
        assert_eq!(names.len(), 3, "{names:?}");
    }

    /// `MemRepo` whose `update_base` always fails — simulates the registry
    /// going unwritable between the snapshot fetch and the source stamping.
    struct FailingUpdateRepo(MemRepo);

    #[async_trait::async_trait]
    impl nomifun_db::IKnowledgeRepository for FailingUpdateRepo {
        async fn insert_base(&self, row: &KnowledgeBaseRow) -> Result<(), nomifun_db::DbError> {
            self.0.insert_base(row).await
        }
        async fn update_base(&self, row: &KnowledgeBaseRow) -> Result<(), nomifun_db::DbError> {
            Err(nomifun_db::DbError::NotFound(format!("simulated persist failure for {}", row.id)))
        }
        async fn delete_base(&self, id: &str) -> Result<(), nomifun_db::DbError> {
            self.0.delete_base(id).await
        }
        async fn get_base(&self, id: &str) -> Result<Option<KnowledgeBaseRow>, nomifun_db::DbError> {
            self.0.get_base(id).await
        }
        async fn list_bases(&self) -> Result<Vec<KnowledgeBaseRow>, nomifun_db::DbError> {
            self.0.list_bases().await
        }
        async fn get_binding(
            &self,
            kind: &str,
            id: &str,
        ) -> Result<Option<(KnowledgeBindingRow, Vec<String>)>, nomifun_db::DbError> {
            self.0.get_binding(kind, id).await
        }
        #[allow(clippy::too_many_arguments)]
        async fn set_binding(
            &self,
            kind: &str,
            id: &str,
            kb_ids: &[String],
            enabled: bool,
            writeback: bool,
            writeback_mode: &str,
            writeback_eagerness: &str,
            channel_write_enabled: bool,
            updated_at: nomifun_common::TimestampMs,
        ) -> Result<i64, nomifun_db::DbError> {
            self.0
                .set_binding(kind, id, kb_ids, enabled, writeback, writeback_mode, writeback_eagerness, channel_write_enabled, updated_at)
                .await
        }
        async fn delete_binding(&self, kind: &str, id: &str) -> Result<(), nomifun_db::DbError> {
            self.0.delete_binding(kind, id).await
        }
        async fn list_bindings_using_kb(&self, kb_id: &str) -> Result<Vec<KnowledgeBindingRow>, nomifun_db::DbError> {
            self.0.list_bindings_using_kb(kb_id).await
        }
        async fn list_knowledge_tags(&self) -> Result<Vec<nomifun_db::models::KnowledgeTagRow>, nomifun_db::DbError> {
            self.0.list_knowledge_tags().await
        }
        async fn create_knowledge_tag(&self, params: nomifun_db::models::CreateKnowledgeTagParams) -> Result<(), nomifun_db::DbError> {
            self.0.create_knowledge_tag(params).await
        }
        async fn update_knowledge_tag(&self, key: &str, params: nomifun_db::models::UpdateKnowledgeTagParams) -> Result<(), nomifun_db::DbError> {
            self.0.update_knowledge_tag(key, params).await
        }
        async fn delete_knowledge_tag(&self, key: &str) -> Result<(), nomifun_db::DbError> {
            self.0.delete_knowledge_tag(key).await
        }
    }

    /// When persisting the fetched source state fails (warn-only path), the
    /// create response must NOT claim the new stamp: the registry still holds
    /// the old value (`None` at create), so `source_fetch.last_fetched_at`
    /// reports that — never the aspirational fresh stamp.
    #[tokio::test]
    async fn create_summary_reports_no_stamp_when_persist_fails() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("body"))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let repo = Arc::new(FailingUpdateRepo(MemRepo::default()));
        let service = KnowledgeService::new(
            repo.clone(),
            &dir.path().join("data"),
            KnowledgeEventEmitter::new(Arc::new(NoopBroadcaster)),
        )
        .with_url_fetcher(HttpFetcher::new().allow_private_for_tests());

        let url = format!("{}/doc", server.uri());
        let kb = service
            .create_base("失忆库", "", None, Some(url_source(KnowledgeSourceMode::Snapshot, &[&url])))
            .await
            .unwrap();

        // The snapshot itself landed (fetch succeeded)…
        let summary = kb.source_fetch.as_ref().expect("snapshot create reports the fetch");
        assert_eq!((summary.fetched, summary.failed), (1, 0), "{:?}", summary.errors);
        // …but the stamp was never persisted, so the summary must not claim it.
        assert_eq!(
            summary.last_fetched_at, None,
            "unpersisted stamp must not be reported as fresh"
        );
        // The registry row agrees: still unstamped.
        assert_eq!(extra_source(&repo.0, &kb.id).unwrap().last_fetched_at, None);
    }

    /// Boot-resume re-runs the fetch pipeline for snapshot-mode sources whose
    /// stamp is missing (registered but never fetched — e.g. the app exited
    /// while a background create-fetch was in flight). Live-mode sources and
    /// already-stamped bases are never touched.
    #[tokio::test]
    async fn boot_resume_fetches_only_unstamped_snapshot_sources() {
        use std::time::{Duration, Instant};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("resumed body"))
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let (service, repo) = service_with_repo(&data_dir);
        let service = Arc::new(service);
        let url = format!("{}/doc", server.uri());

        // Seed rows directly (the persisted-but-unfetched shape a background
        // create leaves behind when the process dies mid-run).
        let seed = |id: &str, mode: KnowledgeSourceMode, stamp: Option<i64>| -> PathBuf {
            let root = data_dir.join(KB_MANAGED_REL_DIR).join(id);
            std::fs::create_dir_all(&root).unwrap();
            let source = KnowledgeSource {
                kind: "url".into(),
                mode,
                entries: vec![KnowledgeSourceEntry {
                    url: url.clone(),
                    title: None,
                    rendered: false,
                }],
                last_fetched_at: stamp,
                credential_ref: None,
                scope: None,
                sync: None,
            };
            repo.bases.lock().unwrap().push(KnowledgeBaseRow {
                id: id.into(),
                name: id.into(),
                description: String::new(),
                root_path: root.to_string_lossy().into_owned(),
                managed: true,
                extra: serde_json::json!({ "source": source }).to_string(),
                created_at: 0,
                updated_at: 0,
                tags: None,
            });
            root
        };
        let pending_root = seed("kb_pending", KnowledgeSourceMode::Snapshot, None);
        let live_root = seed("kb_live", KnowledgeSourceMode::Live, None);
        let stamped_root = seed("kb_stamped", KnowledgeSourceMode::Snapshot, Some(123));

        // Spawned exactly like the production wiring (boot must not block).
        tokio::spawn(Arc::clone(&service).resume_pending_source_fetches());

        // Deadline poll: the pending base gains its snapshot + stamp.
        let snap_dir = pending_root.join(source_url::SNAPSHOT_REL_DIR);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let stamped = extra_source(&repo, "kb_pending").unwrap().last_fetched_at.is_some();
            let snapshots = std::fs::read_dir(&snap_dir).map(|d| d.flatten().count()).unwrap_or(0);
            if stamped && snapshots == 1 {
                let snap = std::fs::read_dir(&snap_dir).unwrap().flatten().next().unwrap();
                assert!(std::fs::read_to_string(snap.path()).unwrap().contains("resumed body"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "boot-resume did not land: stamped={stamped} snapshots={snapshots}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Live-mode and already-stamped bases were never touched.
        assert!(
            !live_root.join(source_url::SNAPSHOT_REL_DIR).exists(),
            "live source must not be fetched by boot-resume"
        );
        assert_eq!(extra_source(&repo, "kb_live").unwrap().last_fetched_at, None);
        assert!(
            !stamped_root.join(source_url::SNAPSHOT_REL_DIR).exists(),
            "stamped source must not be re-fetched by boot-resume"
        );
        assert_eq!(extra_source(&repo, "kb_stamped").unwrap().last_fetched_at, Some(123));
    }

    // ── background-dispatch create (gateway path) ────────────────────

    /// Event-name recorder — lets tests assert `knowledge.base-updated`
    /// is emitted when the background pipeline completes.
    #[derive(Default)]
    struct RecordingBroadcaster {
        names: std::sync::Mutex<Vec<String>>,
    }

    impl nomifun_realtime::EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, event: nomifun_api_types::WebSocketMessage<serde_json::Value>) {
            self.names.lock().unwrap().push(event.name);
        }
    }

    /// Gateway-mode create (`create_base_with_background_fetch`): the call
    /// returns before any URL is fetched — no sync `source_fetch` summary,
    /// `extra.source` persisted unstamped — then the background task lands
    /// the snapshot, stamps `lastFetchedAt`, backfills the description via
    /// the chained autogen, and emits `knowledge.base-updated`.
    #[tokio::test]
    async fn background_create_returns_immediately_then_fetches_and_autogens() {
        use std::time::{Duration, Instant};
        use wiremock::matchers::{method, path as urlpath};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // The response delay guarantees the fetch cannot have finished when
        // create returns, making the immediate-return assertions race-free.
        Mock::given(method("GET"))
            .and(urlpath("/docs/guide"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(400))
                    .set_body_raw(
                        "<html><head><title>接口文档</title></head><body><h1>API</h1><p>说明</p></body></html>",
                        "text/html; charset=utf-8",
                    ),
            )
            .mount(&server)
            .await;

        let dir = tempfile::TempDir::new().unwrap();
        let repo = Arc::new(MemRepo::default());
        let events = Arc::new(RecordingBroadcaster::default());
        let service = Arc::new(
            KnowledgeService::new(
                repo.clone(),
                &dir.path().join("data"),
                KnowledgeEventEmitter::new(events.clone()),
            )
            .with_url_fetcher(HttpFetcher::new().allow_private_for_tests()),
        );
        service.set_completer(FakeCompleter::new(OVERVIEW_JSON, ""));

        let url = format!("{}/docs/guide", server.uri());
        let kb = Arc::clone(&service)
            .create_base_with_background_fetch(
                "后台库",
                "",
                None,
                Some(url_source(KnowledgeSourceMode::Snapshot, &[&url])),
            )
            .await
            .unwrap();

        // Immediate return: source persisted (unstamped), nothing fetched
        // yet, description still the (empty) user-supplied one.
        assert!(kb.source_fetch.is_none(), "background create must not report a sync fetch");
        assert_eq!(kb.description, "");
        let stored = extra_source(&repo, &kb.id).expect("extra.source persisted before the fetch");
        assert_eq!(stored.entries[0].url, url);
        assert_eq!(stored.last_fetched_at, None, "stamp belongs to the background fetch");
        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);
        assert!(!snap_dir.exists(), "create must not wait for the fetch");

        // Deadline poll (no fixed sleep): snapshot on disk, stamp set,
        // description backfilled, completion event emitted.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let stored = extra_source(&repo, &kb.id).unwrap();
            let described = repo
                .bases
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.id == kb.id)
                .is_some_and(|r| r.description == "AI 生成的描述");
            let snapshots = std::fs::read_dir(&snap_dir).map(|d| d.flatten().count()).unwrap_or(0);
            let updated_emitted =
                events.names.lock().unwrap().iter().any(|n| n == "knowledge.base-updated");
            if stored.last_fetched_at.is_some() && described && snapshots == 1 && updated_emitted {
                assert_eq!(stored.entries[0].title.as_deref(), Some("接口文档"), "title backfill persisted");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "background fetch did not complete: stamped={} described={described} snapshots={snapshots} updated_emitted={updated_emitted}",
                stored.last_fetched_at.is_some()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    // ── workspace / data-root overlap guard ──────────────────────────

    #[tokio::test]
    async fn mounts_skipped_when_workspace_overlaps_data_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let service = make_service(&data_dir);
        let kb = service.create_base("库", "", None, None).await.unwrap();
        service.write_file(&kb.id, "a.md", "# A").await.unwrap();
        service
            .set_binding(
                "conversation",
                "1",
                KnowledgeBinding {
                    enabled: true,
                    kb_ids: vec![kb.id.clone()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // Workspace == data root → skipped, no scaffolding created.
        let outcome = service.ensure_mounts_for_target("conversation", "1", &data_dir).await;
        assert!(outcome.mounts.is_empty());
        assert!(!data_dir.join(".nomi").exists());

        // Workspace is an ancestor of the data root → skipped too.
        let outcome = service.ensure_mounts_for_target("conversation", "1", dir.path()).await;
        assert!(outcome.mounts.is_empty());
        assert!(!dir.path().join(".nomi").exists());

        // Reverse direction: a workspace INSIDE the managed knowledge root
        // (here: the base's own directory) must be skipped as well — the
        // mount sweep would otherwise run inside a knowledge base's files.
        let kb_root = PathBuf::from(&kb.root_path);
        let outcome = service.ensure_mounts_for_target("conversation", "1", &kb_root).await;
        assert!(outcome.mounts.is_empty());
        assert!(!kb_root.join(".nomi").exists());

        // A sibling workspace mounts normally (guard must not overfire).
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let outcome = service.ensure_mounts_for_target("conversation", "1", &ws).await;
        assert_eq!(outcome.mounts.len(), 1);
    }

    // ── workpath-level bindings (session-list unification §7) ────────

    use crate::workpath::DEFAULT_WORKPATH_KEY;

    /// Create a base with one document and bind it (enabled) to the given
    /// target. Returns the kb id.
    async fn bind_new_base(service: &KnowledgeService, name: &str, kind: &str, target: &str) -> String {
        let kb = service.create_base(name, "", None, None).await.unwrap();
        service.write_file(&kb.id, "a.md", "# A").await.unwrap();
        service
            .set_binding(
                kind,
                target,
                KnowledgeBinding {
                    enabled: true,
                    kb_ids: vec![kb.id.clone()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        kb.id
    }

    /// When a `workpath` row exists it wins over the legacy per-session
    /// binding — no merge.
    #[tokio::test]
    async fn session_mounts_prefer_workpath_binding() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let key = workpath_key(&ws.to_string_lossy());

        let kb_workpath = bind_new_base(&service, "路径库", WORKPATH_BINDING_KIND, &key).await;
        let _kb_legacy = bind_new_base(&service, "会话库", "conversation", "1").await;

        let outcome = service.ensure_mounts_for_session(&key, "conversation", "1", &ws).await;
        assert_eq!(outcome.mounts.len(), 1, "{:?}", outcome.mounts);
        assert_eq!(outcome.mounts[0].id, kb_workpath);
    }

    /// No `workpath` row at all → the legacy `(conversation, id)` binding
    /// keeps mounting (smooth upgrade for pre-workpath local data).
    #[tokio::test]
    async fn session_mounts_fall_back_to_legacy_binding_on_workpath_miss() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let key = workpath_key(&ws.to_string_lossy());

        let kb_legacy = bind_new_base(&service, "会话库", "conversation", "1").await;

        let outcome = service.ensure_mounts_for_session(&key, "conversation", "1", &ws).await;
        assert_eq!(outcome.mounts.len(), 1, "{:?}", outcome.mounts);
        assert_eq!(outcome.mounts[0].id, kb_legacy);

        // Terminal sessions take the same path with their own legacy kind.
        let kb_term = bind_new_base(&service, "终端库", "terminal", "2").await;
        let outcome = service.ensure_mounts_for_session(&key, "terminal", "2", &ws).await;
        assert_eq!(outcome.mounts.len(), 1, "{:?}", outcome.mounts);
        assert_eq!(outcome.mounts[0].id, kb_term);
    }

    /// An existing-but-disabled workpath row is an explicit choice, not a
    /// miss — it shadows the legacy binding instead of falling back.
    #[tokio::test]
    async fn disabled_workpath_binding_shadows_legacy_binding() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let key = workpath_key(&ws.to_string_lossy());

        let _kb_legacy = bind_new_base(&service, "会话库", "conversation", "1").await;
        service
            .set_binding(WORKPATH_BINDING_KIND, &key, KnowledgeBinding::default())
            .await
            .unwrap();

        let outcome = service.ensure_mounts_for_session(&key, "conversation", "1", &ws).await;
        assert!(outcome.mounts.is_empty(), "{:?}", outcome.mounts);
    }

    /// Temporary (backend-managed) workspaces resolve to the
    /// `__default__` sentinel and share one default-workpath binding.
    #[tokio::test]
    async fn session_mounts_default_workpath_for_temporary_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let service = make_service(&data_dir);

        // Temp workspace under the backend data dir → sentinel key (the
        // same derivation the conversation/terminal services apply).
        let temp_ws = data_dir.join("conversations").join("gemini-temp-c1");
        std::fs::create_dir_all(&temp_ws).unwrap();
        let key = crate::workpath::session_workpath_key(&temp_ws, &data_dir);
        assert_eq!(key, DEFAULT_WORKPATH_KEY);

        let kb = bind_new_base(&service, "默认库", WORKPATH_BINDING_KIND, DEFAULT_WORKPATH_KEY).await;
        let outcome = service.ensure_mounts_for_session(&key, "conversation", "1", &temp_ws).await;
        assert_eq!(outcome.mounts.len(), 1, "{:?}", outcome.mounts);
        assert_eq!(outcome.mounts[0].id, kb);
    }

    /// Workpath target ids are canonicalized server-side: every spelling of
    /// the same directory (trailing slash, backslashes) reads/writes the
    /// same row, and the mount lookup tolerates a raw (un-normalized) key.
    #[tokio::test]
    async fn workpath_binding_target_id_is_canonicalized() {
        let dir = tempfile::TempDir::new().unwrap();
        let service = make_service(&dir.path().join("data"));
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let canonical = workpath_key(&ws.to_string_lossy());

        // Write with a trailing-slash spelling…
        let kb = bind_new_base(&service, "路径库", WORKPATH_BINDING_KIND, &format!("{}/", ws.display())).await;

        // …read back under the canonical key.
        let binding = service.get_binding(WORKPATH_BINDING_KIND, &canonical).await.unwrap();
        assert!(binding.enabled);
        assert_eq!(binding.kb_ids, vec![kb.clone()]);

        // The session lookup normalizes its own input too.
        let outcome = service
            .ensure_mounts_for_session(&format!("{}/", ws.display()), "conversation", "1", &ws)
            .await;
        assert_eq!(outcome.mounts.len(), 1, "{:?}", outcome.mounts);
        assert_eq!(outcome.mounts[0].id, kb);

        // delete_binding canonicalizes as well — the row really goes away.
        service
            .delete_binding(WORKPATH_BINDING_KIND, &format!("{}/", ws.display()))
            .await
            .unwrap();
        let binding = service.get_binding(WORKPATH_BINDING_KIND, &canonical).await.unwrap();
        assert!(!binding.enabled);
        assert!(binding.kb_ids.is_empty());
    }

    // ── search_bases (in-process keyword search over real base root) ──

    /// Build a service whose `data_dir` is a fresh tempdir, mirroring the
    /// crate's `make_service`/managed-base layout. Managed bases provision
    /// under `{data_dir}/knowledge/{id}` eagerly at create time.
    async fn search_test_service() -> (KnowledgeService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let svc = make_service(tmp.path());
        (svc, tmp)
    }

    #[tokio::test]
    async fn search_bases_finds_topic_in_managed_base_ignoring_gitignore() {
        let (svc, _tmp) = search_test_service().await;
        let info = svc.create_base("运维手册", "团队运维约定", None, None).await.unwrap();
        let root = svc.data_dir().join("knowledge").join(&info.id);
        std::fs::write(root.join(".gitignore"), "*\n").unwrap();
        std::fs::create_dir_all(root.join("deploy")).unwrap();
        std::fs::write(root.join("deploy/rollback.md"), "# 回滚流程\n\n生产环境回滚分三步：先停流量……\n").unwrap();
        std::fs::create_dir_all(root.join("_inbox/conv-1")).unwrap();
        std::fs::write(root.join("_inbox/conv-1/draft.md"), "# 回滚草稿\n临时笔记\n").unwrap();

        let hits = svc.search_bases(&[info.id.clone()], "回滚", 8).await.unwrap();
        assert!(!hits.is_empty(), "must find topic despite .gitignore + hidden mount semantics");
        assert!(hits.iter().any(|h| h.rel_path == "deploy/rollback.md"));
        assert!(hits.iter().all(|h| !h.rel_path.starts_with("_inbox/")), "_inbox excluded");
        let top = &hits[0];
        assert_eq!(top.kb_name, "运维手册");
        assert!(top.heading.contains("回滚流程"));
        assert!(!top.snippet.is_empty());
    }

    #[tokio::test]
    async fn search_bases_ranks_filename_and_heading_above_body() {
        let (svc, _tmp) = search_test_service().await;
        let info = svc.create_base("库", "", None, None).await.unwrap();
        let root = svc.data_dir().join("knowledge").join(&info.id);
        std::fs::write(root.join("payments.md"), "# Payments API\n\nrefund flow here\n").unwrap();
        std::fs::write(root.join("misc.md"), "# Misc\n\nthe word payments appears once\n").unwrap();
        let hits = svc.search_bases(&[info.id.clone()], "payments", 8).await.unwrap();
        assert_eq!(hits[0].rel_path, "payments.md", "filename/heading match ranks first");
    }

    #[tokio::test]
    async fn search_bases_unknown_id_is_skipped_not_error() {
        let (svc, _tmp) = search_test_service().await;
        let hits = svc.search_bases(&["does-not-exist".into()], "x", 8).await.unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn score_md_zero_when_no_match() {
        let terms = vec!["zzz".to_string()];
        assert!(score_md("a/b.md", "Heading", "body text", "zzz", &terms).is_none());
    }

    #[test]
    fn score_md_phrase_and_terms() {
        let terms = vec!["回滚".to_string()];
        let scored = score_md("deploy/rollback.md", "回滚流程", "生产环境回滚分三步", "回滚", &terms);
        assert!(scored.is_some());
        let (score, snippet) = scored.unwrap();
        assert!(score > 0);
        assert!(snippet.contains("回滚"));
    }

    // ── resolve_kb_ids_for_cwd ──────────────────────────────────────────

    #[tokio::test]
    async fn resolve_kb_ids_for_cwd_returns_bound_bases_for_known_workpath() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let service = make_service(&data_dir);

        // Create two bases.
        let kb1 = service.create_base("库A", "", None, None).await.unwrap();
        let kb2 = service.create_base("库B", "", None, None).await.unwrap();

        // Bind only kb1 to a workpath.
        let ws = "/Users/dev/project";
        let key = workpath_key(ws);
        service
            .set_binding(
                WORKPATH_BINDING_KIND,
                &key,
                KnowledgeBinding {
                    enabled: true,
                    kb_ids: vec![kb1.id.clone()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // cwd matching the bound workpath → only the bound base.
        let ids = service.resolve_kb_ids_for_cwd(ws).await;
        assert_eq!(ids, vec![kb1.id.clone()]);

        // Trailing slash normalizes to the same key.
        let ids = service.resolve_kb_ids_for_cwd(&format!("{ws}/")).await;
        assert_eq!(ids, vec![kb1.id.clone()]);

        // Unknown cwd → all bases.
        let ids = service.resolve_kb_ids_for_cwd("/unknown/path").await;
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&kb1.id));
        assert!(ids.contains(&kb2.id));

        // Empty cwd → all bases.
        let ids = service.resolve_kb_ids_for_cwd("").await;
        assert_eq!(ids.len(), 2);
    }

    #[tokio::test]
    async fn resolve_kb_ids_for_cwd_disabled_binding_falls_back_to_all() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let service = make_service(&data_dir);

        let kb1 = service.create_base("库A", "", None, None).await.unwrap();
        let kb2 = service.create_base("库B", "", None, None).await.unwrap();

        let ws = "/Users/dev/proj2";
        let key = workpath_key(ws);
        // Bind only kb1 to this workpath, but DISABLED → fallback to all.
        service
            .set_binding(
                WORKPATH_BINDING_KIND,
                &key,
                KnowledgeBinding {
                    enabled: false,
                    kb_ids: vec![kb1.id.clone()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let ids = service.resolve_kb_ids_for_cwd(ws).await;
        // Must return ALL mounted bases (kb1 + kb2), not just the binding's [kb1].
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&kb1.id));
        assert!(ids.contains(&kb2.id));
    }

    #[tokio::test]
    async fn resolve_kb_ids_for_cwd_managed_workspace_returns_all() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        let service = make_service(&data_dir);

        let kb1 = service.create_base("库", "", None, None).await.unwrap();

        // A cwd under the data_dir (managed workspace) maps to DEFAULT_WORKPATH_KEY → all.
        let managed_cwd = data_dir.join("conversations").join("temp-1");
        let ids = service
            .resolve_kb_ids_for_cwd(&managed_cwd.to_string_lossy())
            .await;
        assert_eq!(ids, vec![kb1.id]);
    }

    // ── document handle codec (P1 unified write stack) ────────────────

    #[test]
    fn handle_roundtrips_kb_id_and_rel_path() {
        let h = encode_doc_handle("kb_0193", "deploy/rollback.md");
        assert!(h.starts_with("kdoc_"), "{h}");
        assert_eq!(decode_doc_handle(&h), Some(("kb_0193".to_owned(), "deploy/rollback.md".to_owned())));
    }

    #[test]
    fn handle_roundtrips_unicode_and_spaces() {
        let h = encode_doc_handle("kb_x", "运维/回滚 流程.md");
        assert_eq!(decode_doc_handle(&h), Some(("kb_x".to_owned(), "运维/回滚 流程.md".to_owned())));
    }

    #[test]
    fn handle_decode_rejects_malformed() {
        assert_eq!(decode_doc_handle("not-a-handle"), None);
        assert_eq!(decode_doc_handle("kdoc_!!!notbase64"), None);
        assert_eq!(decode_doc_handle("kdoc_"), None);
    }

    // ── write target resolver + path de-confusion (P1) ────────────────

    /// Build a service with one managed base seeded with `{rel}` = `content`.
    /// The returned `TempDir` must be kept in scope by the caller (bind it as
    /// `_dir`) so the managed directory survives for the test.
    async fn test_service_with_file(rel: &str, content: &str) -> (KnowledgeService, String, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let service = crate::testutil::make_service(&dir.path().join("data"));
        let kb = service.create_base("库", "", None, None).await.unwrap();
        service.write_file(&kb.id, rel, content).await.unwrap();
        (service, kb.id, dir)
    }

    #[test]
    fn deconfuse_strips_mount_prefix() {
        assert_eq!(deconfuse_rel_path(".nomi/knowledge/Finance/terms.md"), "terms.md");
        assert_eq!(deconfuse_rel_path("./.nomi/knowledge/运维手册/deploy/rollback.md"), "deploy/rollback.md");
        assert_eq!(deconfuse_rel_path("deploy/rollback.md"), "deploy/rollback.md");
        assert_eq!(deconfuse_rel_path("terms.md"), "terms.md");
        assert_eq!(deconfuse_rel_path("a\\b.md"), "a/b.md");
    }

    #[tokio::test]
    async fn resolve_handle_to_existing_is_update() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "# 术语").await;
        let h = encode_doc_handle(&kb_id, "terms.md");
        let res = svc.resolve_write_target(&[kb_id.clone()], &WriteTargetSpec::Handle(h)).await.unwrap();
        assert_eq!(res.canonical_rel_path, "terms.md");
        assert_eq!(res.op, WriteOp::Update);
    }

    #[tokio::test]
    async fn resolve_mount_prefixed_path_updates_original() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "# 术语").await;
        let spec = WriteTargetSpec::Path { kb_id: kb_id.clone(), rel_path: ".nomi/knowledge/X/terms.md".into() };
        let res = svc.resolve_write_target(&[kb_id.clone()], &spec).await.unwrap();
        assert_eq!(res.canonical_rel_path, "terms.md", "mount prefix stripped → updates original");
        assert_eq!(res.op, WriteOp::Update);
    }

    #[tokio::test]
    async fn resolve_novel_path_is_create() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "x").await;
        let spec = WriteTargetSpec::Path { kb_id: kb_id.clone(), rel_path: "brand-new.md".into() };
        let res = svc.resolve_write_target(&[kb_id.clone()], &spec).await.unwrap();
        assert_eq!(res.op, WriteOp::Create);
        assert_eq!(res.canonical_rel_path, "brand-new.md");
    }

    #[tokio::test]
    async fn resolve_basename_collision_elsewhere_errors_with_handle() {
        let (svc, kb_id, _dir) = test_service_with_file("deep/terms.md", "x").await;
        let spec = WriteTargetSpec::Path { kb_id: kb_id.clone(), rel_path: "terms.md".into() };
        let err = svc.resolve_write_target(&[kb_id.clone()], &spec).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("deep/terms.md") && msg.contains("kdoc_"), "{msg}");
    }

    #[tokio::test]
    async fn resolve_out_of_scope_kb_is_forbidden() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "x").await;
        let h = encode_doc_handle(&kb_id, "terms.md");
        let err = svc.resolve_write_target(&[], &WriteTargetSpec::Handle(h)).await.unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)));
    }

    // ── write_document + per-surface WritePolicy (P1) ─────────────────

    fn wb_binding(writeback: bool, mode: &str) -> KnowledgeBinding {
        KnowledgeBinding { enabled: true, writeback, writeback_mode: mode.to_owned(), ..Default::default() }
    }

    #[tokio::test]
    async fn search_cache_serves_unchanged_and_invalidates_on_edit() {
        let (svc, kb_id, _dir) = test_service_with_file("doc.md", "# 标题\n市盈率 PER 内容").await;
        // First search populates the cache.
        let hits = svc.search_bases(&[kb_id.clone()], "市盈率", 8).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(svc.search_cache_len() >= 1, "cache should be populated after a search");
        // Second search → same result (served from cache).
        let hits2 = svc.search_bases(&[kb_id.clone()], "市盈率", 8).await.unwrap();
        assert_eq!(hits2.len(), 1);
        assert_eq!(hits2[0].rel_path, hits[0].rel_path);
        // Overwrite (size + mtime change) → cache invalidates, new content searchable.
        svc.write_file(&kb_id, "doc.md", "# 标题\n净资产收益率 ROE 新内容补充").await.unwrap();
        let hits3 = svc.search_bases(&[kb_id.clone()], "ROE", 8).await.unwrap();
        assert_eq!(hits3.len(), 1, "edited content must be searchable");
        let stale = svc.search_bases(&[kb_id.clone()], "市盈率", 8).await.unwrap();
        assert!(stale.is_empty(), "old content must not survive in cache");
        // Clear empties the cache.
        svc.clear_search_cache();
        assert_eq!(svc.search_cache_len(), 0);
    }

    #[test]
    fn policy_regular_chat_defaults_staged_and_respects_direct() {
        let p = resolve_write_policy(WriteSurface::RegularChat, &wb_binding(true, "staged"), "conv-1");
        assert!(matches!(p.mode, WriteMode::Staged { ref scope } if scope == "conv-1"));
        let p = resolve_write_policy(WriteSurface::RegularChat, &wb_binding(true, "direct"), "conv-1");
        assert!(matches!(p.mode, WriteMode::Direct));
    }

    #[test]
    fn policy_companion_direct_channel_disabled_unwritten_disabled() {
        assert!(matches!(resolve_write_policy(WriteSurface::Companion, &wb_binding(true, "staged"), "c").mode, WriteMode::Direct));
        assert!(matches!(resolve_write_policy(WriteSurface::ExternalChannel, &wb_binding(true, "direct"), "c").mode, WriteMode::Disabled));
        assert!(matches!(resolve_write_policy(WriteSurface::RegularChat, &wb_binding(false, "direct"), "c").mode, WriteMode::Disabled));
    }

    #[test]
    fn policy_external_channel_respects_write_toggle_and_forces_staged() {
        // Off (default) → Disabled even with writeback on.
        let off = KnowledgeBinding { enabled: true, writeback: true, channel_write_enabled: false, ..Default::default() };
        assert!(matches!(resolve_write_policy(WriteSurface::ExternalChannel, &off, "ch-1").mode, WriteMode::Disabled));
        // On → Staged (never Direct), regardless of writeback_mode.
        let on = KnowledgeBinding {
            enabled: true,
            writeback: true,
            writeback_mode: "direct".into(),
            channel_write_enabled: true,
            ..Default::default()
        };
        assert!(
            matches!(resolve_write_policy(WriteSurface::ExternalChannel, &on, "ch-1").mode, WriteMode::Staged { ref scope } if scope == "ch-1"),
            "channel writes must be forced to staged even when mode=direct"
        );
        // Toggle is irrelevant when write-back itself is off.
        let no_wb = KnowledgeBinding { enabled: true, writeback: false, channel_write_enabled: true, ..Default::default() };
        assert!(matches!(resolve_write_policy(WriteSurface::ExternalChannel, &no_wb, "ch-1").mode, WriteMode::Disabled));
    }

    #[tokio::test]
    async fn staged_update_writes_to_inbox_and_preserves_original() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "ORIGINAL").await;
        let req = WriteRequest {
            spec: WriteTargetSpec::Path { kb_id: kb_id.clone(), rel_path: "terms.md".into() },
            content: "PROPOSED".into(),
            policy: WritePolicy { mode: WriteMode::Staged { scope: "conv-7".into() }, allow_create: true, surface: WriteSurface::RegularChat },
            bound_kb_ids: vec![kb_id.clone()],
        };
        let out = svc.write_document(req).await.unwrap();
        assert_eq!(out.final_rel_path, "_inbox/conv-7/terms.md");
        assert!(out.staged && out.op == WriteOp::Update);
        assert_eq!(svc.read_file(&kb_id, "terms.md").await.unwrap().content, "ORIGINAL");
        assert_eq!(svc.read_file(&kb_id, "_inbox/conv-7/terms.md").await.unwrap().content, "PROPOSED");
    }

    #[tokio::test]
    async fn direct_update_overwrites_original() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "OLD").await;
        let req = WriteRequest {
            spec: WriteTargetSpec::Handle(encode_doc_handle(&kb_id, "terms.md")),
            content: "NEW".into(),
            policy: WritePolicy { mode: WriteMode::Direct, allow_create: true, surface: WriteSurface::Companion },
            bound_kb_ids: vec![kb_id.clone()],
        };
        let out = svc.write_document(req).await.unwrap();
        assert_eq!(out.final_rel_path, "terms.md");
        assert!(!out.staged);
        assert_eq!(svc.read_file(&kb_id, "terms.md").await.unwrap().content, "NEW");
    }

    #[tokio::test]
    async fn disabled_mode_refuses() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "x").await;
        let req = WriteRequest {
            spec: WriteTargetSpec::Path { kb_id: kb_id.clone(), rel_path: "terms.md".into() },
            content: "y".into(),
            policy: WritePolicy { mode: WriteMode::Disabled, allow_create: true, surface: WriteSurface::ExternalChannel },
            bound_kb_ids: vec![kb_id.clone()],
        };
        assert!(matches!(svc.write_document(req).await.unwrap_err(), AppError::Forbidden(_)));
    }

    /// The exact reported scenario: STAGED regular-chat write-back where the
    /// model passes the workspace-mount path. It must land in the review inbox
    /// mirroring the ORIGINAL doc — NOT a new nested file — and the original
    /// must be untouched.
    #[tokio::test]
    async fn mothers_bug_staged_mount_prefixed_path_lands_in_inbox_not_nested() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "ORIGINAL").await;
        let req = WriteRequest {
            spec: WriteTargetSpec::Path { kb_id: kb_id.clone(), rel_path: ".nomi/knowledge/Finance/terms.md".into() },
            content: "PROPOSED EDIT".into(),
            policy: WritePolicy { mode: WriteMode::Staged { scope: "conv-9".into() }, allow_create: true, surface: WriteSurface::RegularChat },
            bound_kb_ids: vec![kb_id.clone()],
        };
        let out = svc.write_document(req).await.unwrap();
        assert_eq!(out.final_rel_path, "_inbox/conv-9/terms.md", "mirror original, not nest under .nomi/...");
        assert_eq!(out.op, WriteOp::Update);
        assert_eq!(svc.read_file(&kb_id, "terms.md").await.unwrap().content, "ORIGINAL");
        let files = svc.list_files(&kb_id).await.unwrap();
        assert!(!files.iter().any(|f| f.rel_path.contains(".nomi/knowledge")), "no nested mount-path file: {files:?}");
    }

    /// **P3 connector sync e2e**: register a mock connector + credential store,
    /// create a connector-backed base, and prove `sync_connector_source` writes
    /// one snapshot per remote doc (slug = sanitized remote_id, frontmatter
    /// carries the web source_url), persists the cursor, and on a second sync
    /// moves a vanished doc into `snapshots/_trash/` (never hard-deletes).
    #[tokio::test]
    async fn connector_sync_writes_snapshots_persists_cursor_and_trashes_deletions() {
        use crate::connector::FetchedConnectorDoc;
        use nomifun_db::DbError;
        use nomifun_db::models::ConnectorCredentialRow;
        use std::sync::Mutex as StdMutex;

        #[derive(Default)]
        struct MemCredRepo {
            rows: StdMutex<Vec<ConnectorCredentialRow>>,
        }
        #[async_trait::async_trait]
        impl IConnectorCredentialRepository for MemCredRepo {
            async fn list(&self) -> Result<Vec<ConnectorCredentialRow>, DbError> {
                Ok(self.rows.lock().unwrap().clone())
            }
            async fn get(&self, id: &str) -> Result<Option<ConnectorCredentialRow>, DbError> {
                Ok(self.rows.lock().unwrap().iter().find(|r| r.id == id).cloned())
            }
            async fn create(
                &self,
                kind: &str,
                name: &str,
                payload_encrypted: &str,
            ) -> Result<ConnectorCredentialRow, DbError> {
                let mut rows = self.rows.lock().unwrap();
                let row = ConnectorCredentialRow {
                    id: format!("cred_{}", rows.len() + 1),
                    kind: kind.to_owned(),
                    name: name.to_owned(),
                    payload_encrypted: payload_encrypted.to_owned(),
                    created_at: 1,
                    updated_at: 1,
                };
                rows.push(row.clone());
                Ok(row)
            }
            async fn delete(&self, id: &str) -> Result<(), DbError> {
                self.rows.lock().unwrap().retain(|r| r.id != id);
                Ok(())
            }
        }

        struct MockConn {
            docs: StdMutex<Vec<RemoteDocRef>>,
            deleted: StdMutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl KnowledgeConnector for MockConn {
            fn kind(&self) -> &'static str {
                "mock"
            }
            async fn validate_credentials(&self, cred: &ConnectorCredential) -> Result<ConnectorIdentity, AppError> {
                // Reject an obviously-bad payload so create_credential's fail-fast is exercised elsewhere.
                if cred.payload.get("token").is_none() {
                    return Err(AppError::Unauthorized("missing token".into()));
                }
                Ok(ConnectorIdentity { tenant_name: Some("T".into()), scopes_available: vec!["wiki".into()] })
            }
            async fn list_documents(
                &self,
                _cred: &ConnectorCredential,
                _scope: &ConnectorScope,
                _cursor: &SyncCursor,
                _page_token: Option<&str>,
            ) -> Result<SyncPage, AppError> {
                Ok(SyncPage {
                    docs: self.docs.lock().unwrap().clone(),
                    deleted_ids: self.deleted.lock().unwrap().clone(),
                    next_page_token: None,
                    updated_cursor: SyncCursor { last_sync_at: Some(123), opaque: serde_json::json!({ "p": 1 }) },
                })
            }
            async fn fetch_document(
                &self,
                _cred: &ConnectorCredential,
                doc: &RemoteDocRef,
            ) -> Result<FetchedConnectorDoc, AppError> {
                Ok(FetchedConnectorDoc {
                    remote_id: doc.remote_id.clone(),
                    title: doc.title.clone(),
                    markdown: format!("# {}\n\nbody of {}", doc.title, doc.remote_id),
                    edit_time: doc.edit_time,
                    source_url: Some(format!("https://mock/{}", doc.remote_id)),
                })
            }
        }

        let dir = tempfile::TempDir::new().unwrap();
        let (service, repo) = service_with_repo(&dir.path().join("data"));
        let conn = Arc::new(MockConn {
            docs: StdMutex::new(vec![
                RemoteDocRef { remote_id: "DOCAAA".into(), title: "Alpha".into(), edit_time: 10, doc_type: "docx".into() },
                RemoteDocRef { remote_id: "DOCBBB".into(), title: "Beta".into(), edit_time: 20, doc_type: "docx".into() },
            ]),
            deleted: StdMutex::new(vec![]),
        });
        service.register_connector(conn.clone());
        service.set_connector_credentials(Arc::new(MemCredRepo::default()), [7u8; 32]);

        // A bad payload is rejected and never stored.
        assert!(service.create_credential("mock", "bad", serde_json::json!({})).await.is_err());
        assert!(service.list_credentials().await.unwrap().is_empty(), "rejected credential must not persist");

        // A good payload validates and is stored as a secret-free summary.
        let cred = service
            .create_credential("mock", "My Mock", serde_json::json!({ "token": "x" }))
            .await
            .unwrap();
        assert_eq!(cred.kind, "mock");
        assert_eq!(service.list_credentials().await.unwrap().len(), 1);

        let source = KnowledgeSource {
            kind: "mock".into(),
            mode: KnowledgeSourceMode::Snapshot,
            entries: vec![],
            last_fetched_at: None,
            credential_ref: Some(cred.id.clone()),
            scope: Some(serde_json::json!({ "space_id": "s1" })),
            sync: None,
        };
        let kb = service.create_base("镜像库", "", None, Some(source)).await.unwrap();
        let snap_dir = PathBuf::from(&kb.root_path).join(source_url::SNAPSHOT_REL_DIR);

        // First sync: 2 docs → 2 snapshots.
        let summary = service.sync_connector_source(&kb.id).await.unwrap();
        assert_eq!(summary.fetched, 2);
        assert_eq!(summary.failed, 0);
        let a = snap_dir.join("docaaa.md");
        let b = snap_dir.join("docbbb.md");
        assert!(a.exists() && b.exists(), "both snapshots written");
        let a_body = std::fs::read_to_string(&a).unwrap();
        assert!(a_body.contains("# Alpha"), "body rendered: {a_body}");
        assert!(a_body.contains("body of DOCAAA"));
        assert!(a_body.contains("https://mock/DOCAAA"), "frontmatter carries web source_url");

        // Cursor + stamp persisted; no error.
        let stored = extra_source(&repo, &kb.id).unwrap();
        let sync = stored.sync.expect("sync state persisted");
        assert_eq!(sync.last_error, None);
        assert!(sync.last_sync_at.is_some());
        assert_eq!(sync.cursor, serde_json::json!({ "p": 1 }), "terminal page cursor persisted");
        // Watermark is the REMOTE max edit_time (20), not local now_ms — this is
        // what the incremental filter resumes from, avoiding clock-skew misses.
        assert_eq!(sync.watermark, Some(20), "watermark = max remote edit_time");

        // Second sync: DOCBBB deleted → moved to _trash, DOCAAA survives.
        *conn.docs.lock().unwrap() =
            vec![RemoteDocRef { remote_id: "DOCAAA".into(), title: "Alpha".into(), edit_time: 10, doc_type: "docx".into() }];
        *conn.deleted.lock().unwrap() = vec!["DOCBBB".into()];
        let summary2 = service.sync_connector_source(&kb.id).await.unwrap();
        assert_eq!(summary2.fetched, 1);
        assert!(!b.exists(), "deleted doc removed from live snapshots");
        assert!(snap_dir.join("_trash").join("docbbb.md").exists(), "deleted doc moved to _trash");
        assert!(a.exists(), "surviving doc remains");
    }

    /// **P4 inbox e2e**: stage proposals via the P1 staged-write path, then
    /// list / diff / merge / discard them, and prove the main document list
    /// hides `_inbox/`.
    #[tokio::test]
    async fn inbox_review_lists_diffs_merges_and_discards() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "ORIGINAL\n").await;
        let stage = |rel: &str, content: &str, scope: &str| WriteRequest {
            spec: WriteTargetSpec::Path { kb_id: kb_id.clone(), rel_path: rel.into() },
            content: content.into(),
            policy: WritePolicy {
                mode: WriteMode::Staged { scope: scope.into() },
                allow_create: true,
                surface: WriteSurface::RegularChat,
            },
            bound_kb_ids: vec![kb_id.clone()],
        };
        svc.write_document(stage("terms.md", "UPDATED\n", "conv-1")).await.unwrap();
        svc.write_document(stage("new-note.md", "BRAND NEW\n", "conv-2")).await.unwrap();

        // Main document list excludes `_inbox/`.
        let files = svc.list_files(&kb_id).await.unwrap();
        assert!(files.iter().all(|f| !f.rel_path.starts_with("_inbox")), "main list hides inbox: {files:?}");
        assert!(files.iter().any(|f| f.rel_path == "terms.md"));

        // Inbox lists both proposals (grouped by scope client-side).
        let inbox = svc.list_inbox(&kb_id).await.unwrap();
        assert_eq!(inbox.len(), 2, "two staged proposals: {inbox:?}");
        assert_eq!(inbox.iter().find(|e| e.rel_path == "terms.md").unwrap().scope, "conv-1");
        assert_eq!(svc.count_pending_inbox().await.unwrap(), 2, "global pending count reflects staged proposals");

        // Update diff carries old+new; new-doc diff flags is_new.
        let d = svc.inbox_diff(&kb_id, "conv-1", "terms.md").await.unwrap();
        assert!(!d.is_new);
        assert_eq!(d.base_content.as_deref(), Some("ORIGINAL\n"));
        assert!(d.unified_diff.contains("-ORIGINAL") && d.unified_diff.contains("+UPDATED"), "diff: {}", d.unified_diff);
        let dn = svc.inbox_diff(&kb_id, "conv-2", "new-note.md").await.unwrap();
        assert!(dn.is_new && dn.base_content.is_none());

        // Merge → base overwritten, inbox copy gone, scope dir pruned.
        assert_eq!(svc.merge_inbox(&kb_id, "conv-1", "terms.md").await.unwrap().merged_path, "terms.md");
        assert_eq!(svc.read_file(&kb_id, "terms.md").await.unwrap().content, "UPDATED\n");
        let after = svc.list_inbox(&kb_id).await.unwrap();
        assert_eq!(after.len(), 1, "merged proposal removed");
        assert_eq!(after[0].rel_path, "new-note.md");

        // Discard → gone, base never created.
        svc.discard_inbox(&kb_id, "conv-2", "new-note.md").await.unwrap();
        assert!(svc.list_inbox(&kb_id).await.unwrap().is_empty());
        assert!(svc.read_file(&kb_id, "new-note.md").await.is_err(), "discarded proposal never merged");

        // Vanished proposal → NotFound; traversal scope → BadRequest.
        assert!(svc.inbox_diff(&kb_id, "conv-1", "terms.md").await.is_err());
        assert!(svc.inbox_diff(&kb_id, "..", "terms.md").await.is_err());
    }

    /// **Batch merge**: merge_all_inbox accepts all staged proposals and count
    /// drops to zero.
    #[tokio::test]
    async fn merge_all_clears_pending() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "ORIGINAL\n").await;
        let stage = |rel: &str, content: &str, scope: &str| WriteRequest {
            spec: WriteTargetSpec::Path { kb_id: kb_id.clone(), rel_path: rel.into() },
            content: content.into(),
            policy: WritePolicy {
                mode: WriteMode::Staged { scope: scope.into() },
                allow_create: true,
                surface: WriteSurface::RegularChat,
            },
            bound_kb_ids: vec![kb_id.clone()],
        };
        svc.write_document(stage("terms.md", "UPDATED\n", "conv-1")).await.unwrap();
        svc.write_document(stage("new-note.md", "BRAND NEW\n", "conv-2")).await.unwrap();
        assert_eq!(svc.count_pending_inbox().await.unwrap(), 2);

        let merged = svc.merge_all_inbox(&kb_id, None).await.unwrap();
        assert_eq!(merged, 2);
        assert_eq!(svc.count_pending_inbox().await.unwrap(), 0);
        // Verify content was actually merged into base.
        assert_eq!(svc.read_file(&kb_id, "terms.md").await.unwrap().content, "UPDATED\n");
        assert_eq!(svc.read_file(&kb_id, "new-note.md").await.unwrap().content, "BRAND NEW\n");
    }

    /// **Batch discard**: discard_all_inbox removes all proposals without
    /// affecting base content.
    #[tokio::test]
    async fn discard_all_clears_pending() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "ORIGINAL\n").await;
        let stage = |rel: &str, content: &str, scope: &str| WriteRequest {
            spec: WriteTargetSpec::Path { kb_id: kb_id.clone(), rel_path: rel.into() },
            content: content.into(),
            policy: WritePolicy {
                mode: WriteMode::Staged { scope: scope.into() },
                allow_create: true,
                surface: WriteSurface::RegularChat,
            },
            bound_kb_ids: vec![kb_id.clone()],
        };
        svc.write_document(stage("terms.md", "UPDATED\n", "conv-1")).await.unwrap();
        svc.write_document(stage("new-note.md", "BRAND NEW\n", "conv-2")).await.unwrap();
        assert_eq!(svc.count_pending_inbox().await.unwrap(), 2);

        let discarded = svc.discard_all_inbox(&kb_id, None).await.unwrap();
        assert_eq!(discarded, 2);
        assert_eq!(svc.count_pending_inbox().await.unwrap(), 0);
        // Base content unchanged.
        assert_eq!(svc.read_file(&kb_id, "terms.md").await.unwrap().content, "ORIGINAL\n");
        // New doc was never created.
        assert!(svc.read_file(&kb_id, "new-note.md").await.is_err());
    }

    /// **Batch with scope filter**: only proposals in matching scope are processed.
    #[tokio::test]
    async fn merge_all_with_scope_filter() {
        let (svc, kb_id, _dir) = test_service_with_file("terms.md", "ORIGINAL\n").await;
        let stage = |rel: &str, content: &str, scope: &str| WriteRequest {
            spec: WriteTargetSpec::Path { kb_id: kb_id.clone(), rel_path: rel.into() },
            content: content.into(),
            policy: WritePolicy {
                mode: WriteMode::Staged { scope: scope.into() },
                allow_create: true,
                surface: WriteSurface::RegularChat,
            },
            bound_kb_ids: vec![kb_id.clone()],
        };
        svc.write_document(stage("terms.md", "UPDATED\n", "conv-1")).await.unwrap();
        svc.write_document(stage("new-note.md", "BRAND NEW\n", "conv-2")).await.unwrap();
        assert_eq!(svc.count_pending_inbox().await.unwrap(), 2);

        // Only merge scope conv-1.
        let merged = svc.merge_all_inbox(&kb_id, Some("conv-1")).await.unwrap();
        assert_eq!(merged, 1);
        assert_eq!(svc.count_pending_inbox().await.unwrap(), 1);
        // conv-1 was merged.
        assert_eq!(svc.read_file(&kb_id, "terms.md").await.unwrap().content, "UPDATED\n");
        // conv-2 still pending.
        let remaining = svc.list_inbox(&kb_id).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].scope, "conv-2");
    }

    /// **P4 consumers**: `list_consumers` returns every binding that mounts the
    /// base — enabled and disabled — and excludes bindings of other bases.
    #[tokio::test]
    async fn list_consumers_returns_enabled_and_disabled_bindings() {
        let (svc, kb_id, _dir) = test_service_with_file("a.md", "x").await;
        svc.set_binding(
            "conversation",
            "1",
            KnowledgeBinding { enabled: true, kb_ids: vec![kb_id.clone()], ..Default::default() },
        )
        .await
        .unwrap();
        svc.set_binding(
            "workpath",
            "/Users/me/proj",
            KnowledgeBinding { enabled: false, kb_ids: vec![kb_id.clone()], ..Default::default() },
        )
        .await
        .unwrap();
        // A binding for a DIFFERENT base must not appear.
        let other = svc.create_base("其他", "", None, None).await.unwrap();
        svc.set_binding(
            "terminal",
            "9",
            KnowledgeBinding { enabled: true, kb_ids: vec![other.id.clone()], ..Default::default() },
        )
        .await
        .unwrap();

        let consumers = svc.list_consumers(&kb_id).await.unwrap();
        assert_eq!(consumers.len(), 2, "only bindings using this kb: {consumers:?}");
        let conv = consumers.iter().find(|c| c.target_kind == "conversation").unwrap();
        assert_eq!(conv.target_id.as_deref(), Some("1"));
        assert!(conv.enabled);
        assert!(!consumers.iter().find(|c| c.target_kind == "workpath").unwrap().enabled, "disabled included");
    }

    /// `derive_kind` covers all four UI type categories.
    #[test]
    fn derive_kind_covers_all_ui_types() {
        // managed + no source = blank (user created from scratch)
        assert_eq!(derive_kind(true, None), "blank");
        // non-managed + no source = local (user-referenced directory)
        assert_eq!(derive_kind(false, None), "local");
        // URL source = web
        let url = KnowledgeSource {
            kind: "url".into(),
            mode: KnowledgeSourceMode::Live,
            entries: vec![],
            last_fetched_at: None,
            credential_ref: None,
            scope: None,
            sync: None,
        };
        assert_eq!(derive_kind(true, Some(&url)), "web");
        assert_eq!(derive_kind(false, Some(&url)), "web");
        // Feishu connector = feishu
        let fs = KnowledgeSource {
            kind: "feishu".into(),
            mode: KnowledgeSourceMode::Snapshot,
            entries: vec![],
            last_fetched_at: None,
            credential_ref: None,
            scope: None,
            sync: None,
        };
        assert_eq!(derive_kind(true, Some(&fs)), "feishu");
        assert_eq!(derive_kind(false, Some(&fs)), "feishu");
    }

    // ── Tag CRUD tests ───────────────────────────────────────────────────

    fn test_service() -> KnowledgeService {
        let dir = tempfile::TempDir::new().unwrap();
        // Leak the tempdir so it lives for the test's duration.
        let path = dir.keep();
        crate::testutil::make_service(&path)
    }

    #[tokio::test]
    async fn delete_tag_strips_it_from_bases() {
        let svc = test_service();
        svc.create_tag("研发", None).await.unwrap();
        let key = svc.list_tags().await.unwrap()[0].key.clone();
        let base = svc.create_base("库A", "", None, None).await.unwrap();
        svc.update_base(&base.id, None, None, Some(vec![key.clone()])).await.unwrap();
        // Verify the tag was written.
        let before = svc.list_bases().await.unwrap().into_iter().find(|b| b.id == base.id).unwrap();
        assert_eq!(before.tags, vec![key.clone()]);
        // Delete the tag — must strip from the base.
        svc.delete_tag(&key).await.unwrap();
        let after = svc.list_bases().await.unwrap().into_iter().find(|b| b.id == base.id).unwrap();
        assert!(after.tags.is_empty(), "删除标签须从库上剔除");
    }

    #[tokio::test]
    async fn create_tag_slugifies_label() {
        let svc = test_service();
        let tag = svc.create_tag("Hello World", None).await.unwrap();
        assert_eq!(tag.key, "hello-world");
        assert_eq!(tag.label, "Hello World");
    }

    #[tokio::test]
    async fn create_tag_dedup_on_conflict() {
        let svc = test_service();
        svc.create_tag("ops", None).await.unwrap();
        let second = svc.create_tag("ops", None).await.unwrap();
        assert_eq!(second.key, "ops-2");
    }

    #[tokio::test]
    async fn create_tag_chinese_label_fallback() {
        let svc = test_service();
        let tag = svc.create_tag("研发", None).await.unwrap();
        // Should start with "tag-" since the label is all CJK.
        assert!(tag.key.starts_with("tag-"), "CJK label should get hash-based key, got: {}", tag.key);
    }

    #[tokio::test]
    async fn update_tag_changes_label() {
        let svc = test_service();
        let tag = svc.create_tag("alpha", Some("red".into())).await.unwrap();
        let updated = svc.update_tag(&tag.key, UpdateKnowledgeTagRequest {
            label: Some("beta".into()),
            color: None,
            sort_order: None,
        }).await.unwrap();
        assert_eq!(updated.label, "beta");
        assert_eq!(updated.color, Some("red".into())); // unchanged
    }

    /// A5: creating a base with a feishu (connector-backed) source persists
    /// `extra.source` with credential_ref and scope, and `kind` derives to
    /// "feishu".
    #[tokio::test]
    async fn create_base_with_feishu_source_persists_kind_and_credential() {
        let dir = tempfile::TempDir::new().unwrap();
        let (service, repo) = service_with_repo(&dir.path().join("data"));
        let source = KnowledgeSource {
            kind: "feishu".into(),
            mode: KnowledgeSourceMode::Snapshot,
            entries: vec![],
            last_fetched_at: None,
            credential_ref: Some("cred_abc".into()),
            scope: Some(serde_json::json!({ "space_id": "sp1" })),
            sync: Some(Default::default()),
        };
        let info = service.create_base("飞书库", "", None, Some(source)).await.unwrap();
        assert_eq!(info.kind, "feishu", "kind must derive to feishu");
        // Verify credential_ref persisted in extra.source
        let stored = extra_source(&repo, &info.id).expect("source stored in extra");
        assert_eq!(stored.credential_ref.as_deref(), Some("cred_abc"));
        assert_eq!(stored.scope.unwrap()["space_id"], "sp1");
        assert_eq!(stored.kind, "feishu");
    }

    /// A5: feishu source must be rejected when credential_ref or scope is missing.
    #[tokio::test]
    async fn create_base_feishu_source_rejects_missing_credential_or_scope() {
        let dir = tempfile::TempDir::new().unwrap();
        let (service, _repo) = service_with_repo(&dir.path().join("data"));
        // Missing credential_ref
        let no_cred = KnowledgeSource {
            kind: "feishu".into(),
            mode: KnowledgeSourceMode::Snapshot,
            entries: vec![],
            last_fetched_at: None,
            credential_ref: None,
            scope: Some(serde_json::json!({ "space_id": "sp1" })),
            sync: None,
        };
        assert!(service.create_base("x", "", None, Some(no_cred)).await.is_err());
        // Missing scope
        let no_scope = KnowledgeSource {
            kind: "feishu".into(),
            mode: KnowledgeSourceMode::Snapshot,
            entries: vec![],
            last_fetched_at: None,
            credential_ref: Some("cred1".into()),
            scope: None,
            sync: None,
        };
        assert!(service.create_base("x", "", None, Some(no_scope)).await.is_err());
        // Wrong mode (must be snapshot)
        let live_mode = KnowledgeSource {
            kind: "feishu".into(),
            mode: KnowledgeSourceMode::Live,
            entries: vec![],
            last_fetched_at: None,
            credential_ref: Some("cred1".into()),
            scope: Some(serde_json::json!({ "space_id": "sp1" })),
            sync: None,
        };
        assert!(service.create_base("x", "", None, Some(live_mode)).await.is_err());
        assert!(service.list_bases().await.unwrap().is_empty(), "all rejected");
    }

    /// A5: creating a base with tags persists them in the returned info.
    #[tokio::test]
    async fn create_base_with_tags_via_route_persists_tags() {
        let svc = test_service();
        // First create a tag so the key exists.
        svc.create_tag("研发", None).await.unwrap();
        let key = svc.list_tags().await.unwrap()[0].key.clone();
        // Create a base, then immediately assign tags (mimicking the route handler
        // pattern: create → update_base with tags).
        let info = svc.create_base("带标签库", "", None, None).await.unwrap();
        let info = svc.update_base(&info.id, None, None, Some(vec![key.clone()])).await.unwrap();
        assert_eq!(info.tags, vec![key.clone()], "tags must be persisted at create-time");
        // Verify via a fresh load
        let reloaded = svc.get_base_info(&info.id).await.unwrap();
        assert_eq!(reloaded.tags, vec![key]);
    }
}
