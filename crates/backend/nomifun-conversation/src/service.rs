use std::path::PathBuf;
use std::sync::Arc;

use nomifun_ai_agent::types::{BuildTaskOptions, SendMessageData};
use nomifun_ai_agent::{AgentInstance, IWorkerTaskManager};

use crate::response_middleware::ICronService;
use crate::runtime_state::ConversationRuntimeStateService;
use nomifun_api_types::{
    ApprovalCheckResponse, CloneConversationRequest, ConfirmRequest, ConfirmationListResponse,
    ConversationArtifactKind, ConversationArtifactListResponse, ConversationArtifactResponse,
    ConversationArtifactStatus, ConversationListResponse, ConversationMcpStatus, ConversationMcpStatusKind,
    ConversationResponse, ConversationRuntimeSummary, CreateConversationRequest, ListConversationsQuery,
    ListMessagesQuery, MessageListResponse, MessageResponse, MessageSearchResponse, SearchMessagesQuery,
    SendMessageRequest, SessionMcpServer, SessionMcpTransport, UpdateConversationArtifactRequest,
    UpdateConversationRequest, WebSocketMessage,
};
use nomifun_common::{
    AgentKillReason, AgentType, AppError, ConversationSource, ConversationStatus, ErrorChain, MessageType,
    OnConversationDelete, PaginatedResult, generate_prefixed_id, now_ms, workspace_path_has_edge_whitespace_segment,
};
use nomifun_db::models::{ConversationRow, MessageRow};
use nomifun_db::{
    ConversationFilters, ConversationRowUpdate, CreateAcpSessionParams, IAcpSessionRepository,
    IAgentMetadataRepository, IConversationRepository, IMcpServerRepository, SaveRuntimeStateParams, SortOrder,
};
use nomifun_mcp::{AcpMcpCapabilities, parse_acp_mcp_capabilities};
use nomifun_realtime::EventBroadcaster;
use nomifun_runtime::resolve_command_path;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::convert::{
    TOOL_CONTENT_COMPACT_THRESHOLD_BYTES, row_to_artifact_response, row_to_message_response,
    row_to_message_response_compact, row_to_response, row_to_response_with_extra, search_row_to_item, string_to_enum,
};
use crate::skill_resolver::SkillResolver;
use crate::skill_snapshot::{backfill_skills_if_missing, compute_initial_skills};
use crate::stream_relay::StreamRelay;
use std::sync::RwLock;

const MAX_CRON_CONTINUATIONS_PER_TURN: usize = 4;

/// Parse a string conversation id (the service's public-API / in-memory key
/// form) into the integer key the repo now uses. A non-numeric id yields an
/// explicit NotFound rather than silently matching another row (spec §2.5/§7.4).
pub(crate) fn parse_conv_id(id: &str) -> Result<i64, nomifun_common::AppError> {
    id.parse::<i64>()
        .map_err(|_| nomifun_common::AppError::NotFound(format!("conversation {id}")))
}

#[derive(Debug, Clone, Copy)]
struct McpSupportPolicy {
    stdio: bool,
    http: bool,
    sse: bool,
    streamable_http: bool,
}

impl McpSupportPolicy {
    const NOMI: Self = Self {
        stdio: true,
        http: true,
        sse: true,
        streamable_http: true,
    };

    fn from_acp_capabilities(capabilities: AcpMcpCapabilities) -> Self {
        Self {
            stdio: capabilities.stdio,
            http: capabilities.http,
            sse: capabilities.sse,
            streamable_http: capabilities.http,
        }
    }

    fn supports_row_transport(self, transport_type: &str) -> bool {
        match transport_type {
            "stdio" => self.stdio,
            "http" => self.http,
            "sse" => self.sse,
            "streamable_http" => self.streamable_http,
            _ => false,
        }
    }

    fn supports_session_transport(self, transport: &SessionMcpTransport) -> bool {
        match transport {
            SessionMcpTransport::Stdio { .. } => self.stdio,
            SessionMcpTransport::Http { .. } => self.http,
            SessionMcpTransport::Sse { .. } => self.sse,
            SessionMcpTransport::StreamableHttp { .. } => self.streamable_http,
        }
    }
}

/// One-directional seam letting IDMM (the `nomifun-idmm` crate) arm supervision
/// for a desktop conversation at turn start — WITHOUT this crate depending on
/// `nomifun-idmm` (which sits above it). `nomifun-idmm::IdmmManager` implements
/// it; `nomifun-app` injects the implementation at assembly time via
/// [`ConversationService::with_supervision_hook`]. Called fire-and-forget once
/// per turn after the agent task exists; the implementation resolves config
/// internally and is a cheap no-op when IDMM is disabled or already supervising.
///
/// Mirrors AutoWork's `IdmmHandle::ensure_supervising` (which arms per
/// orchestrator loop iteration) for the plain, user-driven desktop chat path —
/// the only path that otherwise never armed IDMM (no AutoWork loop, no
/// boot-resume), so an enabled 智能决策 silently never observed the turn.
pub trait ConversationSupervisionHook: Send + Sync {
    /// Ensure IDMM supervision is (idempotently) running for this conversation.
    fn on_turn_start(&self, conversation_id: &str);
}

#[derive(Clone)]
pub struct ConversationService {
    workspace_root: PathBuf,
    broadcaster: Arc<dyn EventBroadcaster>,
    skill_resolver: Arc<dyn SkillResolver>,
    task_manager: Arc<dyn IWorkerTaskManager>,
    /// Hooks invoked at the end of `delete()` so other services
    /// (`WorkerTaskManagerImpl`, `CronService`, …) can clean up their
    /// per-conversation state. Wrapped in `Arc<RwLock<…>>` so registration
    /// can happen post-construction without breaking the `Clone` impl —
    /// mirrors the `cron_service` slot pattern below.
    delete_hooks: Arc<RwLock<Vec<Arc<dyn OnConversationDelete>>>>,
    cron_service: Arc<RwLock<Option<Arc<dyn ICronService>>>>,
    mcp_server_repo: Arc<RwLock<Option<Arc<dyn IMcpServerRepository>>>>,
    /// Knowledge base service slot (same post-construction registration
    /// pattern as `cron_service`). When wired, bound knowledge bases are
    /// mounted into the workspace at task start and surfaced to the agent
    /// via `extra.knowledge_mounts` / `extra.knowledge_writeback`.
    knowledge_service: Arc<RwLock<Option<Arc<nomifun_knowledge::KnowledgeService>>>>,
    runtime_state: Arc<ConversationRuntimeStateService>,
    /// Per-conversation timestamp (ms) of the most recent USER-initiated
    /// cancel (`POST /api/conversations/{id}/cancel`). AutoWork's orchestrator
    /// consults this after a turn ends (`user_cancelled_since`) to tell "the
    /// user deliberately stopped this work" apart from a turn failure —
    /// engine stream events alone can't carry that intent reliably across
    /// every agent type. In-memory only; bounded by the number of
    /// conversations a user ever cancels in one process lifetime.
    user_cancel_stamps: Arc<std::sync::Mutex<std::collections::HashMap<String, i64>>>,

    // Repos for conversation, acp_session and agent_metadata access.
    conversation_repo: Arc<dyn IConversationRepository>,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    acp_session_repo: Arc<dyn IAcpSessionRepository>,
    /// Optional IDMM arm hook (post-construction registration, same slot pattern
    /// as `cron_service`). Wired by `nomifun-app` so a desktop turn arms 智能决策
    /// supervision; `None` in contexts that don't run IDMM (tests, webui-only).
    supervision_hook: Arc<RwLock<Option<Arc<dyn ConversationSupervisionHook>>>>,
    /// Phase 3 模型故障转移(plan D5)。挑选器要读 `providers` 表、配置要读
    /// `client_preferences`,而 `ConversationService::new` 不带这两个仓库。沿用
    /// `cron_service` / `supervision_hook` 的「构造后注册」槽位模式而非改 `new()`
    /// 签名:`nomifun-app` 在装配处对 send-loop 实例调用
    /// [`Self::with_failover_deps`]。未注册(两槽为 `None`)即视为故障转移关闭
    /// —— fail-safe,所以不跑故障转移的上下文(测试、纯 webui)无需任何改动。
    failover_provider_repo: Arc<RwLock<Option<Arc<dyn nomifun_db::IProviderRepository>>>>,
    failover_client_prefs: Arc<RwLock<Option<Arc<dyn nomifun_db::IClientPreferenceRepository>>>>,
}

// ── Construction & Dependency Injection ──────────────────────────────

impl ConversationService {
    pub fn new(
        workspace_root: PathBuf,
        broadcaster: Arc<dyn EventBroadcaster>,
        skill_resolver: Arc<dyn SkillResolver>,
        task_manager: Arc<dyn IWorkerTaskManager>,

        conversation_repo: Arc<dyn IConversationRepository>,
        agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
        acp_session_repo: Arc<dyn IAcpSessionRepository>,
    ) -> Self {
        Self {
            workspace_root,
            broadcaster,
            skill_resolver,
            task_manager,
            delete_hooks: Arc::new(RwLock::new(Vec::new())),
            cron_service: Arc::new(RwLock::new(None)),
            mcp_server_repo: Arc::new(RwLock::new(None)),
            knowledge_service: Arc::new(RwLock::new(None)),
            runtime_state: Arc::new(ConversationRuntimeStateService::default()),
            user_cancel_stamps: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),

            conversation_repo,
            agent_metadata_repo,
            acp_session_repo,
            supervision_hook: Arc::new(RwLock::new(None)),
            failover_provider_repo: Arc::new(RwLock::new(None)),
            failover_client_prefs: Arc::new(RwLock::new(None)),
        }
    }

    pub fn with_runtime_state(mut self, runtime_state: Arc<ConversationRuntimeStateService>) -> Self {
        self.runtime_state = runtime_state;
        self
    }

    pub fn with_cron_service(&self, cron_service: Option<Arc<dyn ICronService>>) {
        if let Ok(mut guard) = self.cron_service.write() {
            *guard = cron_service;
        }
    }

    pub fn with_mcp_server_repo(&self, repo: Arc<dyn IMcpServerRepository>) {
        if let Ok(mut guard) = self.mcp_server_repo.write() {
            *guard = Some(repo);
        }
    }

    pub fn with_knowledge_service(&self, service: Arc<nomifun_knowledge::KnowledgeService>) {
        if let Ok(mut guard) = self.knowledge_service.write() {
            *guard = Some(service);
        }
    }

    /// Register the IDMM supervision hook (post-construction, same pattern as
    /// `with_cron_service`). Called by `nomifun-app` so each desktop turn arms
    /// 智能决策 supervision for the conversation.
    pub fn with_supervision_hook(&self, hook: Arc<dyn ConversationSupervisionHook>) {
        if let Ok(mut guard) = self.supervision_hook.write() {
            *guard = Some(hook);
        }
    }

    /// Register the repositories the Phase 3 model-failover seam needs
    /// (post-construction, same slot pattern as `with_cron_service`): the
    /// provider repo backs the candidate picker, the client-preference repo
    /// backs the global failover config. Wired by `nomifun-app` on the
    /// send-loop instance. When either is left unset, failover is treated as
    /// disabled (fail-safe), so contexts that never run failover need not call
    /// this.
    pub fn with_failover_deps(
        &self,
        provider_repo: Arc<dyn nomifun_db::IProviderRepository>,
        client_prefs: Arc<dyn nomifun_db::IClientPreferenceRepository>,
    ) {
        if let Ok(mut guard) = self.failover_provider_repo.write() {
            *guard = Some(provider_repo);
        }
        if let Ok(mut guard) = self.failover_client_prefs.write() {
            *guard = Some(client_prefs);
        }
    }

    /// Register a hook to be notified when a conversation is deleted.
    ///
    /// Hooks are dispatched sequentially in registration order from
    /// `delete()`. Used by `nomifun-app` to wire up `WorkerTaskManagerImpl`
    /// (kill the agent process) and `CronService` (cascade-delete cron jobs).
    pub fn with_delete_hook(&self, hook: Arc<dyn OnConversationDelete>) {
        if let Ok(mut guard) = self.delete_hooks.write() {
            guard.push(hook);
        }
    }

    /// The single source of truth for `msg_id` values across the backend.
    ///
    /// Every `msg_id` — user message id, assistant message id, cron/tips WS
    /// event id, agent correlation id (`SendMessageData.msg_id`), etc. — must
    /// be produced here. This keeps the ID space uniform and prevents
    /// downstream modules from accidentally forking their own format.
    ///
    /// The value is purely functional (no state), exposed as an associated
    /// function so callers that hold only `ConversationService::mint_msg_id`
    /// (or none of the service at all, via re-export) can use it.
    pub fn mint_msg_id() -> String {
        generate_prefixed_id("msg")
    }

    pub fn conversation_repo(&self) -> &Arc<dyn IConversationRepository> {
        &self.conversation_repo
    }

    pub(crate) fn acp_session_repo(&self) -> &Arc<dyn IAcpSessionRepository> {
        &self.acp_session_repo
    }

    /// Snapshot of the registered failover deps (`None` until
    /// [`Self::with_failover_deps`] is called). Both must be present for the
    /// seam to run; either missing → failover disabled (fail-safe).
    pub(crate) fn failover_deps(
        &self,
    ) -> Option<(
        Arc<dyn nomifun_db::IProviderRepository>,
        Arc<dyn nomifun_db::IClientPreferenceRepository>,
    )> {
        let provider_repo = self.failover_provider_repo.read().ok()?.clone()?;
        let client_prefs = self.failover_client_prefs.read().ok()?.clone()?;
        Some((provider_repo, client_prefs))
    }

    pub fn runtime_state(&self) -> Arc<ConversationRuntimeStateService> {
        self.runtime_state.clone()
    }

    /// Read AND remove the conversation's accumulated token total (`input +
    /// output` summed across the turns the stream relay saw complete). Returns
    /// `None` when nothing was recorded — e.g. a turn that errored before
    /// completing, a non-nomi engine that emits no `TurnCompleted`, or a relay
    /// not wired with the runtime state (every non-orchestrator conversation).
    /// The orchestrator worker calls this exactly once, after its turn settles,
    /// to fill `orch_run_tasks.tokens`. Removing the entry keeps the map bounded.
    pub fn take_turn_tokens(&self, conversation_id: &str) -> Option<i64> {
        self.runtime_state.take_turn_tokens(conversation_id)
    }

    pub(crate) fn task(&self, conversation_id: &str) -> Result<AgentInstance, AppError> {
        self.task_manager
            .get_task(conversation_id)
            .ok_or_else(|| AppError::NotFound(format!("No active agent for conversation '{conversation_id}'")))
    }

    pub async fn runtime_summary_for(&self, conversation_id: &str) -> ConversationRuntimeSummary {
        let agent = self.task_manager.get_task(conversation_id);
        let has_task = agent.is_some();
        let task_status = agent.as_ref().and_then(|agent| agent.status());
        let pending_confirmations = agent.as_ref().map(|agent| agent.get_confirmations().len()).unwrap_or(0);

        self.runtime_state
            .summary_from_parts(conversation_id, task_status, has_task, pending_confirmations)
    }

    pub async fn complete_turn_with_companion_context(
        &self,
        conversation_id: &str,
        companion: bool,
        companion_id: Option<String>,
        origin: Option<String>,
        channel_platform: Option<String>,
    ) {
        let runtime = self.runtime_summary_for(conversation_id).await;
        StreamRelay::complete_conversation_with_context(
            &self.conversation_repo,
            &self.broadcaster,
            conversation_id,
            Some(runtime),
            companion,
            companion_id,
            origin,
            channel_platform,
        )
        .await;
    }

    async fn broadcast_turn_started_with_context(
        &self,
        conversation_id: &str,
        turn_id: &str,
        companion: bool,
        companion_id: Option<String>,
        origin: Option<String>,
        channel_platform: Option<String>,
    ) {
        let runtime = self.runtime_summary_for(conversation_id).await;
        let conv_id: i64 = conversation_id.parse().unwrap_or_default();
        let payload = serde_json::json!({
            "conversation_id": conv_id,
            "session_id": conv_id,
            "turn_id": turn_id,
            "status": "running",
            "phase": "starting",
            "state": "initializing",
            "canSendMessage": runtime.can_send_message,
            "runtime": runtime,
            "companion": companion,
            "companion_id": companion_id,
            "origin": origin,
            "channel_platform": channel_platform,
        });
        self.broadcaster.broadcast(WebSocketMessage::new("turn.started", payload));
    }
}

// ── Conversation CRUD ───────────────────────────────────────────────

impl ConversationService {
    /// Create a new conversation.
    ///
    /// Generates a `conv_{uuidv7}` ID, sets status to `pending`, defaults
    /// source to `nomifun`, and broadcasts `conversation.listChanged(created)`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, agent_type = ?req.r#type))]
    pub async fn create(
        &self,
        user_id: &str,
        req: CreateConversationRequest,
    ) -> Result<ConversationResponse, AppError> {
        let now = now_ms();
        let source = req.source.unwrap_or(ConversationSource::Nomifun);

        // Type-aware rule: top-level `model` is nomi-only. Other agent types
        // carry model/mode via `extra` (see spec 2026-05-12). Reject early so
        // clients that still ship the legacy shape get a loud 400 instead of
        // a silent write to a column nobody reads.
        if req.r#type != AgentType::Nomi && req.model.is_some() {
            return Err(AppError::BadRequest(format!(
                "top-level `model` is only accepted for nomi conversations; pass model via `extra` for {}",
                req.r#type.serde_name()
            )));
        }

        let mut extra = req.extra;

        // nomi source-of-truth rule: top-level `model` wins. If an older client
        // still packs `extra.model`, strip it before persist so the stored row
        // has a single canonical model representation.
        if req.r#type == AgentType::Nomi
            && let Some(obj) = extra.as_object_mut()
            && obj.remove("model").is_some()
        {
            warn!("nomi create: stripped legacy `extra.model`; top-level `model` is canonical");
        }

        // Determine whether the user chose this workspace ("custom") or we
        // auto-provision one under `{data_dir}/conversations/{label}-temp-{id}/`.
        // `is_custom_workspace` is the authoritative signal consumed later to
        // decide whether we should wire skill symlinks (temp workspaces only
        // — user-chosen paths must not be mutated).
        let user_supplied_workspace = match extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            Some(workspace) => Some(normalize_workspace_path(workspace)?),
            None => None,
        };
        let is_custom_workspace = user_supplied_workspace.is_some();
        if let Some(workspace) = user_supplied_workspace.as_ref() {
            extra["workspace"] = serde_json::Value::String(workspace.clone());
        }

        // For auto-provisioned (temp) workspaces the directory name embeds the
        // conversation id, which is now minted by the DB at `create()`. We
        // therefore defer the actual directory creation and the
        // `extra.workspace` write until after the row exists (and `new_id` is
        // known). The edge-whitespace guard, however, only depends on the data
        // dir (`workspace_root`) — the `{label}-temp-{id}` segment is never
        // whitespace — so we validate it loudly here, before any persist, by
        // probing a placeholder path. This preserves the "fail before write"
        // contract of the original flow.
        let auto_workspace_label = if user_supplied_workspace.is_none() {
            let label = conversation_label(&req.r#type, extra.get("backend"));
            let probe_path = self
                .workspace_root
                .join("conversations")
                .join(format!("{label}-temp-0"));
            if workspace_path_has_edge_whitespace_segment(&probe_path) {
                return Err(AppError::WorkspacePathEdgeWhitespace(probe_path.display().to_string()));
            }
            Some(label)
        } else {
            None
        };

        // Strip the request-only custom_workspace toggle — it was read above
        // and must not be persisted as an extra field.
        if let Some(obj) = extra.as_object_mut() {
            obj.remove("custom_workspace");
        }

        // Consume transient skill-shaping inputs and freeze the initial
        // `skills` snapshot into `extra.skills`. These request-only fields
        // must not land in the stored row. Legacy names (`enabled_skills`,
        // `exclude_builtin_skills`) are accepted as aliases for compatibility
        // with older frontend builds and pre-snapshot presets (§7.1).
        fn take_string_array(obj: &mut serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Vec<String> {
            for key in keys {
                if let Some(v) = obj.remove(*key)
                    && let Ok(arr) = serde_json::from_value::<Vec<String>>(v)
                {
                    return arr;
                }
            }
            Vec::new()
        }

        let (preset_enabled, exclude_auto_inject) = match extra.as_object_mut() {
            Some(obj) => {
                let preset = take_string_array(obj, &["preset_enabled_skills", "enabled_skills"]);
                let exclude = take_string_array(obj, &["exclude_auto_inject_skills", "exclude_builtin_skills"]);
                // Strip the stale cache field if a clone copied it in.
                obj.remove("loaded_skills");
                (preset, exclude)
            }
            None => (Vec::new(), Vec::new()),
        };

        let auto_inject_names = self.skill_resolver.auto_inject_names().await;
        let initial_skills = compute_initial_skills(&auto_inject_names, &preset_enabled, &exclude_auto_inject);

        // Skill symlinks are wired into the auto-provisioned workspace *after*
        // the row is created, because the workspace directory name embeds the
        // DB-minted conversation id. Capture the inputs now (the `skills`
        // snapshot below consumes `initial_skills` into `extra`).
        let skills_for_links = initial_skills.clone();

        if let Some(obj) = extra.as_object_mut() {
            obj.insert(
                "skills".to_owned(),
                serde_json::Value::Array(initial_skills.into_iter().map(serde_json::Value::String).collect()),
            );
        }

        // Selection arrives from the client as `extra.selected_mcp_server_ids`.
        // Parsing lives in `parse_selected_mcp_server_ids`. The selection is no
        // longer persisted to `extra` — it lands in the `conversation_mcp_servers`
        // junction after the row exists (FK requires the parent first).
        let selected_mcp_server_ids: Option<Vec<i64>> = match extra.as_object_mut() {
            Some(obj) => parse_selected_mcp_server_ids(obj),
            None => None,
        };
        let selected_session_mcp_servers = match extra.as_object_mut() {
            Some(obj) => match obj.remove("selected_session_mcp_servers") {
                Some(value) => Some(
                    serde_json::from_value::<Vec<SessionMcpServer>>(value)
                        .map_err(|e| AppError::BadRequest(format!("Invalid selected_session_mcp_servers: {e}")))?,
                ),
                None => None,
            },
            None => None,
        };

        let mcp_support = self.resolve_mcp_support_policy(&req.r#type, &extra).await?;
        let mut selected_row_ids: Vec<i64> = Vec::new();
        let mut selected_mcp_names: Vec<String> = Vec::new();
        let mut selected_mcp_statuses: Vec<ConversationMcpStatus> = Vec::new();
        let mut seen_mcp_names = HashSet::new();
        let mut status_index_by_name: HashMap<String, usize> = HashMap::new();
        let repo = self
            .mcp_server_repo
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned());
        if let Some(repo) = repo {
            let rows = match selected_mcp_server_ids.as_ref() {
                Some(ids) => repo
                    .list_by_ids_any(ids)
                    .await
                    .map_err(|e| AppError::Internal(format!("Failed to load selected MCP servers: {e}")))?,
                None => repo
                    .list()
                    .await
                    .map_err(|e| AppError::Internal(format!("Failed to list MCP servers: {e}")))?,
            };
            let selected_rows = rows
                .into_iter()
                .filter(|row| !row.builtin)
                .filter(|row| match selected_mcp_server_ids.as_ref() {
                    Some(ids) => ids.iter().any(|id| *id == row.id),
                    None => row.enabled,
                })
                .collect::<Vec<_>>();
            selected_row_ids = selected_rows.iter().map(|row| row.id).collect();
            for row in &selected_rows {
                if seen_mcp_names.insert(row.name.clone()) {
                    selected_mcp_names.push(row.name.clone());
                }
                upsert_conversation_mcp_status(
                    &mut selected_mcp_statuses,
                    &mut status_index_by_name,
                    classify_repo_mcp_status(row, mcp_support),
                );
            }
        }

        if let Some(session_servers) = selected_session_mcp_servers.as_ref() {
            for server in session_servers {
                if seen_mcp_names.insert(server.name.clone()) {
                    selected_mcp_names.push(server.name.clone());
                }
                upsert_conversation_mcp_status(
                    &mut selected_mcp_statuses,
                    &mut status_index_by_name,
                    classify_session_mcp_status(server, mcp_support),
                );
            }
        }

        if let Some(obj) = extra.as_object_mut() {
            // Build-extra contract: the ai-agent factory's `load_user_mcp_servers`
            // reads `extra.mcp_server_ids` as `Option<Vec<String>>` and parses each
            // back to i64. Stringify the junction-bound INTEGER ids here so that
            // read path keeps working after the selection moved off `extra`.
            obj.insert(
                "mcp_server_ids".to_owned(),
                serde_json::Value::Array(
                    selected_row_ids
                        .iter()
                        .map(|id| serde_json::Value::String(id.to_string()))
                        .collect(),
                ),
            );
            obj.insert(
                "mcp_servers".to_owned(),
                serde_json::Value::Array(selected_mcp_names.into_iter().map(serde_json::Value::String).collect()),
            );
            obj.insert(
                "mcp_statuses".to_owned(),
                serde_json::to_value(&selected_mcp_statuses)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize MCP status snapshot: {e}")))?,
            );
            if let Some(session_servers) = selected_session_mcp_servers.as_ref() {
                obj.insert(
                    "session_mcp_servers".to_owned(),
                    serde_json::to_value(session_servers)
                        .map_err(|e| AppError::Internal(format!("Failed to serialize session MCP snapshot: {e}")))?,
                );
            }
        }

        // `cron_job_id` is now a first-class column (was `extra.cronJobId`).
        // Promote it off `extra` at create time so the FK column is the single
        // source of truth; the cron executor's atomic backfill (§9.A) clears or
        // sets it later for `new_conversation`.
        let cron_job_id = extra
            .get("cron_job_id")
            .and_then(|value| value.as_str())
            .or_else(|| extra.get("cronJobId").and_then(|value| value.as_str()))
            .map(ToOwned::to_owned);

        let row = nomifun_db::models::ConversationRow {
            // Placeholder: the integer PK is allocated by SQLite inside
            // `create()` and returned as `new_id`. The repo ignores this field.
            id: 0,
            user_id: user_id.to_owned(),
            name: req.name.unwrap_or_default(),
            r#type: enum_to_db(&req.r#type)?,
            extra: serde_json::to_string(&extra)
                .map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?,
            model: req
                .model
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| AppError::Internal(format!("Failed to serialize model: {e}")))?,
            status: Some(enum_to_db(&ConversationStatus::Pending)?),
            source: Some(enum_to_db(&source)?),
            channel_chat_id: req.channel_chat_id,
            pinned: false,
            pinned_at: None,
            cron_job_id,
            created_at: now,
            updated_at: now,
        };

        let new_id = self.conversation_repo.create(&row).await?;

        // Now that the conversation id exists, provision the auto (temp)
        // workspace whose directory name embeds it: create the directory, wire
        // skill symlinks (temp workspaces only), record the path in `extra`,
        // and persist the updated `extra` back. User-supplied workspaces are
        // left untouched (already in `extra` and validated above).
        if let Some(label) = auto_workspace_label.as_ref() {
            let ws_path = self
                .workspace_root
                .join("conversations")
                .join(format!("{label}-temp-{new_id}"));
            std::fs::create_dir_all(&ws_path)
                .map_err(|e| AppError::Internal(format!("Failed to create workspace: {e}")))?;

            // Wire skill symlinks into the auto-provisioned workspace so the
            // agent CLI picks them up via its native skills dir (e.g.
            // `.claude/skills/`). Runs only for temp workspaces — a user-chosen
            // path must not be mutated.
            if !is_custom_workspace
                && !skills_for_links.is_empty()
                && let Some(rel_dirs) =
                    native_skills_dirs(&self.agent_metadata_repo, &req.r#type, extra.get("backend")).await
            {
                let resolved = self.skill_resolver.resolve_skills(&skills_for_links).await;
                if !resolved.is_empty() {
                    let rel_dirs_refs: Vec<&str> = rel_dirs.iter().map(String::as_str).collect();
                    let n = self
                        .skill_resolver
                        .link_workspace_skills(&ws_path, &rel_dirs_refs, &resolved)
                        .await;
                    debug!(
                        conversation_id = new_id,
                        workspace = %ws_path.display(),
                        links = n,
                        "wired skill symlinks into workspace"
                    );
                }
            }

            extra["workspace"] = serde_json::Value::String(ws_path.to_string_lossy().into_owned());
            let extra_json = serde_json::to_string(&extra)
                .map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?;
            let workspace_update = ConversationRowUpdate {
                extra: Some(extra_json),
                updated_at: Some(now),
                ..Default::default()
            };
            self.conversation_repo.update(new_id, &workspace_update).await?;
        }

        // Persist the MCP selection into the `conversation_mcp_servers` junction
        // now that the parent row exists (the junction FK requires it). The
        // build-extra `mcp_server_ids` snapshot above already feeds the agent
        // factory; this write is the durable selection store that replaces the
        // retired `extra.selected_mcp_server_ids` array.
        if selected_mcp_server_ids.is_some()
            && let Err(e) = self.conversation_repo.set_mcp_server_ids(new_id, &selected_row_ids).await
        {
            warn!(error = %ErrorChain(&e), conversation_id = new_id, "failed to persist MCP server selection");
        }
        // ACP conversations own one `acp_session` row (1:1 by
        // conversation_id). Other agent types have no session-level
        // state so we only create it for ACP.
        if req.r#type == AgentType::Acp {
            self.create_acp_session_row(&new_id.to_string(), &extra).await?;
        }

        // Build the response from a row carrying the real id and the final
        // `extra` (with the resolved workspace, if any).
        let response_row = nomifun_db::models::ConversationRow {
            id: new_id,
            extra: serde_json::to_string(&extra)
                .map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?,
            ..row
        };
        let response = row_to_response(response_row, &self.workspace_root)?;

        self.broadcast_list_changed(&new_id.to_string(), "created", response.source.as_ref());

        log_conversation_created(&response, &extra);

        Ok(response)
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    async fn create_acp_session_row(&self, conversation_id: &str, extra: &serde_json::Value) -> Result<(), AppError> {
        debug!("Creating acp_session row");

        let conv_id = parse_conv_id(conversation_id)?;

        // Identity comes from the user's agent choice in `extra`.
        // `agent_id` is the catalog row id; `backend` is the vendor
        // label; `agent_source` says builtin/extension/custom. The
        // frontend always posts agent_id for picked rows, but older
        // payloads may only carry `backend`, so we resolve defensively.
        let agent_id_from_extra = extra.get("agent_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let backend = extra.get("backend").and_then(|v| v.as_str()).unwrap_or_default();
        let agent_source = extra.get("agent_source").and_then(|v| v.as_str()).unwrap_or("builtin");

        // Fallback: older clients (electron main, legacy webhooks) only
        // post `backend` without `agent_id`. Resolve the builtin row for
        // that vendor so the session still has a concrete catalog
        // reference. Non-builtin agents must provide `agent_id`
        // explicitly — custom/extension rows have no unique lookup key
        // from `(backend, agent_source)` alone.
        let resolved_agent_id = match agent_id_from_extra {
            Some(id) => id.to_owned(),
            None if !backend.is_empty() && agent_source == "builtin" => self
                .agent_metadata_repo
                .find_builtin_by_backend(backend)
                .await
                .map_err(|e| AppError::Internal(format!("agent_metadata lookup: {e}")))?
                .map(|row| row.id)
                .unwrap_or_default(),
            None => String::new(),
        };

        let params = CreateAcpSessionParams {
            conversation_id: conv_id,
            agent_backend: backend,
            agent_source,
            agent_id: &resolved_agent_id,
        };
        self.acp_session_repo
            .create(&params)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to create acp_session row: {e}")))?;

        // Seed optional runtime state from create payload. Empty strings are
        // treated as absent, matching the "send key only when value present"
        // contract on the wire. Mode/model take effect on the first
        // reconcile right after session/new.
        let mode = extra
            .get("current_mode_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let model = extra
            .get("current_model_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if mode.is_some() || model.is_some() {
            let params = SaveRuntimeStateParams {
                current_mode_id: mode.map(Some),
                current_model_id: model.map(Some),
                config_selections_json: None,
                context_usage_json: None,
            };
            self.acp_session_repo
                .save_runtime_state(conv_id, &params)
                .await
                .map_err(|e| AppError::Internal(format!("Failed to seed acp_session runtime state: {e}")))?;
        }
        Ok(())
    }

    /// Get a single conversation by ID.
    ///
    /// Returns `NotFound` if the conversation does not exist or does not
    /// belong to the given user (avoids leaking existence to other users).
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn get(&self, user_id: &str, id: &str) -> Result<ConversationResponse, AppError> {
        let row = self
            .conversation_repo
            .get(parse_conv_id(id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;

        let mut extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|e| AppError::Internal(format!("Invalid extra JSON: {e}")))?;
        self.backfill_extra_inplace(row.id, &mut extra).await;
        let mut response = row_to_response_with_extra(row, extra, &self.workspace_root)?;
        response.runtime = Some(self.runtime_summary_for(id).await);
        Ok(response)
    }

    /// List conversations with cursor-based pagination and optional filters.
    ///
    /// `exclude_companion_companion`: when `true`, work-partner (companion companion)
    /// single sessions are filtered out of both the page and the `total`
    /// count. The public `/api/conversations` route passes `false` (companion
    /// rows still returned; the frontend sidebar filters them); the companion's own
    /// gateway listing passes `true` so its companion thread does not inflate
    /// the "how many conversations" count.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn list(
        &self,
        user_id: &str,
        query: ListConversationsQuery,
        exclude_companion_companion: bool,
    ) -> Result<ConversationListResponse, AppError> {
        let filters = ConversationFilters {
            // The cursor arrives as a query-string param (String); the repo
            // paginates on the integer PK. A non-numeric cursor is a malformed
            // pagination hint — drop it (start from the top) rather than error.
            cursor: query.cursor.as_deref().and_then(|c| c.parse::<i64>().ok()),
            limit: query.limit.unwrap_or(0),
            source: query.source,
            cron_job_id: query.cron_job_id,
            pinned: query.pinned,
            exclude_companion_companion,
        };

        let result = self.conversation_repo.list_paginated(user_id, &filters).await?;

        // Tolerate per-row deserialization failures — a single legacy row
        // (e.g. an abandoned agent_type='gemini' conversation post-migration)
        // must not take down the whole listing. Skip-and-log is the
        // explicit resilience contract from the Gemini→ACP migration spec.
        let mut items = Vec::with_capacity(result.items.len());
        for row in result.items {
            let row_id = row.id.clone();
            let mut extra: serde_json::Value = match serde_json::from_str(&row.extra) {
                Ok(v) => v,
                Err(err) => {
                    warn!(
                        conversation_id = %row_id,
                        error = %ErrorChain(&err),
                        "Skipping unreadable conversation row in list"
                    );
                    continue;
                }
            };
            self.backfill_extra_inplace(row_id, &mut extra).await;
            match row_to_response_with_extra(row, extra, &self.workspace_root) {
                Ok(resp) => items.push(resp),
                Err(err) => warn!(
                    conversation_id = %row_id,
                    error = %ErrorChain(&err),
                    "Skipping unreadable conversation row in list"
                ),
            }
        }

        Ok(PaginatedResult {
            items,
            total: result.total,
            has_more: result.has_more,
        })
    }

    /// Update a conversation (partial update with extra-merge semantics).
    ///
    /// If `extra` is provided, it is merged into the existing extra JSON
    /// (top-level keys are overwritten, unlisted keys are preserved).
    /// Broadcasts `conversation.listChanged(updated)`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn update(
        &self,
        user_id: &str,
        id: &str,
        req: UpdateConversationRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<ConversationResponse, AppError> {
        let existing = self
            .conversation_repo
            .get(parse_conv_id(id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;

        // Snapshot invariant: once written at create time, `extra.skills`
        // must not be re-shaped by PATCH. The frontend must clone the
        // conversation to produce a new snapshot.
        if let Some(incoming) = &req.extra
            && (incoming.get("skills").is_some()
                || incoming.get("mcp_server_ids").is_some()
                || incoming.get("mcp_servers").is_some()
                || incoming.get("mcp_statuses").is_some()
                || incoming.get("session_mcp_servers").is_some())
        {
            return Err(AppError::BadRequest(
                "extra.skills and MCP snapshots are immutable post-creation".into(),
            ));
        }

        // Type-aware rule: top-level `model` is nomi-only. For non-nomi
        // conversations, model/mode must be updated via `extra` (see spec
        // 2026-05-12).
        let existing_type: AgentType = string_to_enum(&existing.r#type)?;
        if existing_type != AgentType::Nomi && req.model.is_some() {
            return Err(AppError::BadRequest(format!(
                "top-level `model` is only accepted for nomi conversations; pass model via `extra` for {}",
                existing.r#type
            )));
        }

        let now = now_ms();

        // Merge extra if provided. For nomi, strip `extra.model` post-merge
        // so the row keeps a single canonical model source (top-level column).
        let merged_extra = if let Some(new_extra) = &req.extra {
            let mut existing_extra: serde_json::Value =
                serde_json::from_str(&existing.extra).unwrap_or_else(|_| serde_json::json!({}));
            merge_json(&mut existing_extra, new_extra);
            if existing_type == AgentType::Nomi
                && let Some(obj) = existing_extra.as_object_mut()
                && obj.remove("model").is_some()
            {
                warn!("nomi update: stripped legacy `extra.model` from merged extra");
            }
            if new_extra.get("workspace").is_some() {
                normalize_workspace_extra(&mut existing_extra)?;
            }
            Some(
                serde_json::to_string(&existing_extra)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize merged extra: {e}")))?,
            )
        } else {
            None
        };

        // Handle pinned_at: set timestamp on pin, clear on unpin
        let pinned_at = req.pinned.map(|p| if p { Some(now) } else { None });

        let model_changed = req.model.as_ref().is_some_and(|new_model| {
            let new_json = serde_json::to_string(new_model).unwrap_or_default();
            existing.model.as_deref() != Some(new_json.as_str())
        });

        // A workspace repoint (e.g. binding a temporary session to a real
        // project directory) changes the agent's cwd — and, via the surface
        // scope, its native/gateway file authority. The cached agent baked the
        // old cwd at build time, so it must be recycled for the change to take
        // effect on the next message (same rationale as the model-change kill
        // below). Detected by comparing the pre/post merged `extra.workspace`.
        let workspace_changed = req.extra.as_ref().is_some_and(|e| e.get("workspace").is_some()) && {
            let ws_of = |raw: &str| -> Option<String> {
                serde_json::from_str::<serde_json::Value>(raw)
                    .ok()
                    .and_then(|v| v.get("workspace").and_then(|w| w.as_str()).map(str::to_owned))
            };
            let old_ws = ws_of(&existing.extra);
            let new_ws = merged_extra.as_deref().and_then(ws_of);
            old_ws != new_ws
        };

        let model_json = req
            .model
            .as_ref()
            .map(|m| {
                serde_json::to_string(m)
                    .map(Some)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize model: {e}")))
            })
            .transpose()?;

        let updates = ConversationRowUpdate {
            name: req.name,
            pinned: req.pinned,
            pinned_at,
            model: model_json,
            extra: merged_extra,
            status: None,
            cron_job_id: None,
            updated_at: Some(now),
        };

        self.conversation_repo.update(parse_conv_id(id)?, &updates).await?;

        if model_changed || workspace_changed {
            info!(
                model_changed,
                workspace_changed,
                "Conversation updated, killing agent task so the change takes effect on the next message"
            );
            if let Err(e) = task_manager.kill(id, None) {
                warn!(error = %ErrorChain(&e), "Failed to kill agent after model/workspace change");
            }
        }

        // Re-fetch to return the updated version
        let updated = self
            .conversation_repo
            .get(parse_conv_id(id)?)
            .await?
            .ok_or_else(|| AppError::Internal("Conversation vanished after update".into()))?;

        let response = row_to_response(updated, &self.workspace_root)?;

        info!("Conversation updated");
        self.broadcast_list_changed(id, "updated", response.source.as_ref());

        Ok(response)
    }

    /// Merge a JSON patch into `conversation.extra` without touching model,
    /// name, pinned flag, or task lifecycle. Intended for internal callers
    /// (e.g. `TeamSessionService::ensure_session` writing
    /// `team_mcp_stdio_config`) where a full `update()` would kill the agent
    /// on a spurious model comparison.
    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    pub async fn update_extra(&self, conversation_id: &str, patch: serde_json::Value) -> Result<(), AppError> {
        let existing = self
            .conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let mut merged: serde_json::Value =
            serde_json::from_str(&existing.extra).unwrap_or_else(|_| serde_json::json!({}));
        merge_json(&mut merged, &patch);
        if patch.get("workspace").is_some() {
            normalize_workspace_extra(&mut merged)?;
        }

        let updates = ConversationRowUpdate {
            extra: Some(
                serde_json::to_string(&merged)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize merged extra: {e}")))?,
            ),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo.update(parse_conv_id(conversation_id)?, &updates).await?;
        debug!("Conversation extra merged");
        Ok(())
    }

    /// Link an orchestrator run to its originating conversation.
    ///
    /// Merges `extra.orchestrator_run_id` (only touching `extra` — never
    /// model/name/pinned, never killing the agent task) so the frontend can
    /// light up the canvas entry, then broadcasts `conversation.listChanged`
    /// so the conversation list subscription re-fetches. This is the single
    /// shared link-write point for both orchestrator launch paths (caps and
    /// route), keeping the write DRY.
    ///
    /// An empty `conversation_id` (MCP / no-session callers) is a silent
    /// no-op: returns `Ok(())` and broadcasts nothing.
    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id, run_id = %run_id))]
    pub async fn link_orchestrator_run(&self, conversation_id: &str, run_id: &str) -> Result<(), AppError> {
        if conversation_id.is_empty() {
            return Ok(());
        }
        self.update_extra(conversation_id, serde_json::json!({ "orchestrator_run_id": run_id }))
            .await?;
        self.broadcast_list_changed(conversation_id, "updated", None);
        Ok(())
    }

    pub async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), AppError> {
        let params = SaveRuntimeStateParams {
            current_mode_id: Some(Some(mode)),
            ..Default::default()
        };
        self.acp_session_repo
            .save_runtime_state(parse_conv_id(conversation_id)?, &params)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to persist runtime mode: {e}")))?;
        Ok(())
    }

    /// Delete a conversation (messages cascade via FK).
    ///
    /// Broadcasts `conversation.listChanged(deleted)`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn delete(&self, user_id: &str, id: &str) -> Result<(), AppError> {
        let conv_id = parse_conv_id(id)?;
        // Get existing to retrieve source for broadcast and verify ownership
        let existing = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;

        let source: Option<ConversationSource> = existing
            .source
            .as_deref()
            .and_then(|s| string_to_enum::<ConversationSource>(s).ok());

        self.conversation_repo.delete(conv_id).await?;
        // No FK / CASCADE on `acp_session`: clean it up here so non-ACP
        // conversations that used to be ACP (shouldn't happen but is
        // cheap to cover) still drop their orphaned session row.
        if let Err(err) = self.acp_session_repo.delete(conv_id).await {
            warn!(
                error = %ErrorChain(&err),
                "Failed to delete acp_session row on conversation delete"
            );
        }

        // Snapshot the hook list under the read lock, then drop the guard
        // before awaiting — `RwLockReadGuard` is not `Send`, so holding it
        // across `.await` would make this future non-`Send`.
        let hooks: Vec<Arc<dyn OnConversationDelete>> =
            self.delete_hooks.read().map(|guard| guard.clone()).unwrap_or_default();
        for hook in hooks {
            hook.on_conversation_deleted(conv_id).await;
        }

        // Drop the in-memory knowledge signature so the map does not retain
        // entries for deleted conversations across a long-lived process.
        self.runtime_state.clear_knowledge_signature(&conv_id.to_string());
        // Likewise drop any accumulated token total — an orchestrator worker
        // conversation that errored before the worker `take`d it would otherwise
        // linger here until process restart (a small in-memory leak). No-op for the
        // common chat/companion conversation (which never records a token total).
        self.runtime_state.clear_turn_tokens(&conv_id.to_string());

        info!("Conversation deleted");
        self.broadcast_list_changed(id, "deleted", source.as_ref());

        Ok(())
    }

    /// Create a conversation from a `CloneConversationRequest`.
    ///
    /// Historically this method supported cloning from a source conversation
    /// (inheriting name / extra / cron binding). That use case has been
    /// removed — the method is retained only because `POST
    /// /api/conversations/clone` has three active callers
    /// (`_AddNewConversation`, worker task manager, legacy repo shim) that
    /// send a pre-built payload shape. New code should prefer `create`.
    pub async fn clone_create(
        &self,
        user_id: &str,
        req: CloneConversationRequest,
    ) -> Result<ConversationResponse, AppError> {
        self.create(user_id, req.conversation).await
    }

    /// Reset a conversation: clear messages and set status back to pending.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn reset(&self, user_id: &str, id: &str) -> Result<(), AppError> {
        // Verify existence and ownership
        self.conversation_repo
            .get(parse_conv_id(id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;

        // Delete all messages
        self.conversation_repo.delete_messages_by_conversation(parse_conv_id(id)?).await?;
        self.conversation_repo.delete_artifacts_by_conversation(parse_conv_id(id)?).await?;

        // Reset status to pending
        let now = now_ms();
        let updates = ConversationRowUpdate {
            status: Some(enum_to_db(&ConversationStatus::Pending)?),
            updated_at: Some(now),
            ..Default::default()
        };
        self.conversation_repo.update(parse_conv_id(id)?, &updates).await?;

        info!("Conversation reset");
        Ok(())
    }

    /// List conversations associated by the same workspace.
    pub async fn list_associated(&self, user_id: &str, id: &str) -> Result<Vec<ConversationResponse>, AppError> {
        let rows = self.conversation_repo.list_associated(user_id, parse_conv_id(id)?).await?;
        rows.into_iter()
            .map(|row| row_to_response(row, &self.workspace_root))
            .collect()
    }

    /// List conversations spawned by a specific cron job.
    pub async fn list_by_cron_job(
        &self,
        user_id: &str,
        cron_job_id: &str,
    ) -> Result<Vec<ConversationResponse>, AppError> {
        let rows = self.conversation_repo.list_by_cron_job(user_id, cron_job_id).await?;
        rows.into_iter()
            .map(|row| row_to_response(row, &self.workspace_root))
            .collect()
    }
}

// ── Messages & Artifacts ────────────────────────────────────────────

impl ConversationService {
    /// List messages for a conversation with page-based pagination.
    pub async fn list_messages(
        &self,
        user_id: &str,
        conversation_id: &str,
        query: ListMessagesQuery,
    ) -> Result<MessageListResponse, AppError> {
        // Verify conversation exists and belongs to user
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let compact_content = matches!(query.content_mode.as_deref(), Some("compact"));

        // Keyset (cursor) path: incremental newest-first windows for long
        // sessions (e.g. a companion's single session, which now also absorbs
        // every IM-channel turn). The frontend opts in by sending `cursor`: ""
        // for the latest window, or "<created_at>:<id>" (the oldest currently
        // loaded message) to page older. page/page_size offset pagination is
        // bypassed; `page_size` is the window size. `total` is not computed —
        // the client drives "load older" off `has_more` and derives the next
        // cursor from items[0]. `cursor: None` keeps the legacy offset path so
        // other callers are unaffected.
        if let Some(cursor) = query.cursor.as_deref() {
            let limit = query.page_size.unwrap_or(40);
            let before = if cursor.trim().is_empty() {
                None
            } else {
                Some(parse_message_cursor(cursor)?)
            };
            let mut result = self
                .conversation_repo
                .get_messages_keyset(parse_conv_id(conversation_id)?, before, limit)
                .await?;
            // Repo returns newest-first; present oldest-first so the chat renders
            // top→bottom and the client can prepend older windows above it.
            result.items.reverse();
            let mut items = Vec::with_capacity(result.items.len());
            for row in result.items {
                items.push(if compact_content {
                    row_to_message_response_compact(row)?
                } else {
                    row_to_message_response(row)?
                });
            }
            return Ok(PaginatedResult {
                items,
                total: 0,
                has_more: result.has_more,
            });
        }

        let page = query.page.unwrap_or(1);
        let page_size = query.page_size.unwrap_or(50);
        let order = match query.order.as_deref() {
            Some("DESC" | "desc") => SortOrder::Desc,
            _ => SortOrder::Asc,
        };

        let result = self
            .conversation_repo
            .get_messages(parse_conv_id(conversation_id)?, page, page_size, order)
            .await?;

        let mut compacted_count = 0usize;
        let mut total_original_content_bytes = 0usize;
        let mut total_response_content_bytes = 0usize;
        let mut items = Vec::with_capacity(result.items.len());
        for row in result.items {
            let original_content_bytes = row.content.len();
            total_original_content_bytes += original_content_bytes;
            let response = if compact_content {
                row_to_message_response_compact(row)?
            } else {
                row_to_message_response(row)?
            };

            if compact_content {
                if response
                    .content
                    .get("_compact")
                    .and_then(|compact| compact.get("truncated"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    compacted_count += 1;
                }
                total_response_content_bytes += response.content.to_string().len();
            }
            items.push(response);
        }

        if compact_content && compacted_count > 0 {
            info!(
                conversation_id,
                page,
                page_size,
                order = ?order,
                items = items.len(),
                total = result.total,
                compacted = compacted_count,
                total_original_content_bytes,
                total_response_content_bytes,
                "Compacted tool message list response"
            );
        }

        Ok(PaginatedResult {
            items,
            total: result.total,
            has_more: result.has_more,
        })
    }

    /// Return one full message for a conversation after verifying ownership.
    pub async fn get_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<MessageResponse, AppError> {
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let row = self
            .conversation_repo
            .get_message(parse_conv_id(conversation_id)?, message_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Message {message_id} not found")))?;

        let content_bytes = row.content.len();
        let response = row_to_message_response(row)?;
        if is_tool_message_type(response.r#type) || content_bytes > TOOL_CONTENT_COMPACT_THRESHOLD_BYTES {
            info!(
                conversation_id,
                message_id,
                message_type = ?response.r#type,
                content_bytes,
                "Loaded full message content"
            );
        }

        Ok(response)
    }

    /// List artifacts for a conversation with durable status state.
    pub async fn list_artifacts(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationArtifactListResponse, AppError> {
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let mut items = self
            .conversation_repo
            .list_artifacts(parse_conv_id(conversation_id)?)
            .await?
            .into_iter()
            .map(row_to_artifact_response)
            .collect::<Result<Vec<_>, _>>()?;

        let mut legacy_items = self
            .conversation_repo
            .list_legacy_cron_trigger_messages(parse_conv_id(conversation_id)?)
            .await?
            .into_iter()
            .filter_map(|row| legacy_cron_trigger_to_artifact(row).ok())
            .collect::<Vec<_>>();

        items.append(&mut legacy_items);
        items.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });

        Ok(items)
    }

    /// Update the durable status of a conversation artifact and broadcast the upsert.
    pub async fn update_artifact(
        &self,
        user_id: &str,
        conversation_id: &str,
        artifact_id: i64,
        req: UpdateConversationArtifactRequest,
    ) -> Result<ConversationArtifactResponse, AppError> {
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let status = serde_json::to_value(req.status)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| AppError::Internal("Failed to serialize artifact status".into()))?;

        let row = self
            .conversation_repo
            .update_artifact_status(parse_conv_id(conversation_id)?, artifact_id, &status, now_ms())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Artifact {artifact_id} not found")))?;

        let response = row_to_artifact_response(row)?;
        self.broadcaster.broadcast(WebSocketMessage::new(
            "conversation.artifact",
            serde_json::to_value(&response)
                .map_err(|e| AppError::Internal(format!("Failed to serialize artifact event: {e}")))?,
        ));

        Ok(response)
    }

    /// Search messages across all conversations for the user.
    pub async fn search_messages(
        &self,
        user_id: &str,
        query: SearchMessagesQuery,
    ) -> Result<MessageSearchResponse, AppError> {
        if query.keyword.trim().is_empty() {
            return Err(AppError::BadRequest("keyword must not be empty".into()));
        }

        let page = query.page.unwrap_or(1);
        let page_size = query.page_size.unwrap_or(20);

        let result = self
            .conversation_repo
            .search_messages(user_id, &query.keyword, page, page_size)
            .await?;

        let items = result
            .items
            .into_iter()
            .map(|row| search_row_to_item(row, &self.workspace_root))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PaginatedResult {
            items,
            total: result.total,
            has_more: result.has_more,
        })
    }
}

// ── Confirmation System ─────────────────────────────────────────────

impl ConversationService {
    /// Get the list of pending confirmations for a conversation.
    pub async fn list_confirmations(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<ConfirmationListResponse, AppError> {
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let agent = match task_manager.get_task(conversation_id) {
            Some(a) => a,
            None => return Ok(Vec::new()),
        };

        Ok(agent.get_confirmations())
    }

    /// Confirm a pending tool call.
    ///
    /// Sends the confirmation result to the agent and broadcasts a
    /// `confirmation.remove` WebSocket event.
    pub async fn confirm(
        &self,
        user_id: &str,
        conversation_id: &str,
        call_id: &str,
        req: ConfirmRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), AppError> {
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let agent = task_manager
            .get_task(conversation_id)
            .ok_or_else(|| AppError::NotFound("No active agent for this conversation".into()))?;

        let confirmations = agent.get_confirmations();
        let conf_id = confirmations
            .iter()
            .find(|c| c.call_id == call_id)
            .map(|c| c.id.clone());

        agent.confirm(&req.msg_id, call_id, req.data, req.always_allow)?;

        if let Some(conf_id) = conf_id {
            let payload = serde_json::json!({
                "conversation_id": parse_conv_id(conversation_id).unwrap_or_default(),
                "id": conf_id,
            });
            let msg = WebSocketMessage::new("confirmation.remove", payload);
            self.broadcaster.broadcast(msg);
        }

        Ok(())
    }

    /// Check whether an action has been auto-approved in the current session.
    pub async fn check_approval(
        &self,
        user_id: &str,
        conversation_id: &str,
        action: &str,
        command_type: Option<&str>,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<ApprovalCheckResponse, AppError> {
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let approved = task_manager
            .get_task(conversation_id)
            .is_some_and(|agent| agent.check_approval(action, command_type));

        Ok(ApprovalCheckResponse { approved })
    }
}

// ── Message Flow (send / stop / warmup) ─────────────────────────────

impl ConversationService {
    /// Send a user message to the conversation.
    ///
    /// 1. Validates the conversation belongs to the user
    /// 2. Stores the user message (position: "right", status: "finish")
    /// 3. Claims the conversation in runtime state
    /// 4. Spawns background agent build/send and stream relay work
    /// 5. Returns immediately (202 Accepted semantics)
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn send_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: SendMessageRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<String, AppError> {
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }
        let send_started_at = now_ms();

        // Verify conversation exists and belongs to user
        let row = self
            .conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        // Short-circuit for legacy Gemini conversations: the dedicated Gemini
        // runtime has been removed, so we cannot build an agent for this row.
        // Emit CONVERSATION_ARCHIVED (HTTP 410 Gone) without touching the
        // legacy `model` column, which may hold shapes the new parser can't
        // deserialize. The client identifies this case by `code` and renders
        // a dedicated archived-conversation UI rather than a generic banner.
        if row.r#type == "gemini" {
            return Err(AppError::ConversationArchived(
                "This conversation was created with the legacy Gemini runtime, which has been \
                 removed. Please start a new conversation with the Gemini ACP backend to continue."
                    .into(),
            ));
        }

        let turn_claim = self.runtime_state.try_claim_turn(conversation_id)?;

        // Store user message. `msg_id` is server-generated so the WebSocket
        // stream, DB row, and client-side message index all agree on the same
        // key. We reuse the same value for `id` (primary key) and `msg_id`
        // to preserve legacy callers that still rely on `id == msg_id`.
        let user_msg_id = Self::mint_msg_id();
        let user_msg = nomifun_db::models::MessageRow {
            id: user_msg_id.clone(),
            conversation_id: parse_conv_id(conversation_id)?,
            msg_id: Some(user_msg_id.clone()),
            r#type: "text".into(),
            content: serde_json::json!({ "content": req.content }).to_string(),
            position: Some("right".into()),
            status: Some("finish".into()),
            hidden: req.hidden,
            created_at: now_ms(),
        };
        if let Err(e) = self.conversation_repo.insert_message(&user_msg).await {
            warn!(msg_id = %user_msg_id, error = %ErrorChain(&e), "Failed to insert user message");
            return Err(e.into());
        }

        info!(msg_id = %user_msg_id, "User message persisted");

        // Companion wire markers (see `companion_context_from_extra`): stamped on
        // every broadcast of this turn so the companion collector can classify the
        // conversation without a local registry lookup.
        let (companion, companion_id, extra_channel_platform) = companion_context_from_extra(&row.extra);
        // A per-turn `channel_platform` (an IM-channel turn routed into the
        // companion's shared single session) takes precedence over the
        // conversation's static `extra.channelPlatform`. The shared companion
        // session carries no `channelPlatform` of its own, so this is how an IM
        // turn is tagged for the floating window's remote-turn rendering while a
        // desktop/owner turn (no per-turn marker) stays a local turn.
        let channel_platform = req
            .channel_platform
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or(extra_channel_platform);

        // Normalized origin marker (companion/cron/autowork/idmm; None = the human
        // owner). Stamped on message.userCreated AND, via the relay, on every
        // message.stream / turn.completed of this turn — agent-driven turns
        // must be recognizable end to end, or their assistant replies would
        // still be distilled as the owner's work (the indirect feedback loop).
        let origin = req
            .origin
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        self.broadcaster.broadcast(WebSocketMessage::new(
            "message.userCreated",
            serde_json::json!({
                "conversation_id": user_msg.conversation_id,
                "msg_id": &user_msg_id,
                "content": &req.content,
                "position": "right",
                "status": "finish",
                "hidden": req.hidden,
                "origin": origin,
                "companion": companion,
                "companion_id": companion_id,
                "channel_platform": channel_platform,
                "created_at": user_msg.created_at,
            }),
        ));

        // Build task options from conversation row
        let mut build_opts = match self.build_task_options(&row) {
            Ok(opts) => opts,
            Err(err) => {
                error!(
                    error_code = err.error_code(),
                    error = %ErrorChain(&err),
                    "Failed to build task options for message send"
                );
                let _ = self.persist_send_failure_tip(conversation_id, &err).await;
                return Err(err);
            }
        };
        self.ensure_auto_workspace_skill_links(&row, &build_opts).await;
        self.apply_knowledge_mounts(&row, &mut build_opts).await;
        let stored_workspace = build_opts.workspace.clone();

        let first_turn_msg_id = Self::mint_msg_id();
        self.broadcast_turn_started_with_context(
            conversation_id,
            &first_turn_msg_id,
            companion,
            companion_id.clone(),
            origin.clone(),
            channel_platform.clone(),
        )
        .await;

        let conv_id = conversation_id.to_owned();
        let repo = Arc::clone(&self.conversation_repo);
        let broadcaster = Arc::clone(&self.broadcaster);
        let cron_service = self.current_cron_service();
        let user_id_owned = user_id.to_owned();
        let service = self.clone();
        let task_manager = Arc::clone(task_manager);
        // Orchestrator-driven worker conversations carry `orchestrator_run_id` in
        // their `extra` (stamped by `build_worker_extra`). For those — and ONLY
        // those — hand the relay a clone of the runtime state so it accumulates
        // each turn's `TurnCompleted` token usage into the conversation's running
        // total (read back by the worker after the turn settles to fill
        // `orch_run_tasks.tokens`). Gating on this key keeps the common chat /
        // companion path free of any per-turn map entry (no growth, no behaviour
        // change) — those conversations never read the total, so accumulating it
        // would only leak across a long-lived process.
        let token_runtime_state: Option<Arc<ConversationRuntimeStateService>> =
            serde_json::from_str::<serde_json::Value>(&row.extra)
                .ok()
                .as_ref()
                .and_then(|extra| extra.get("orchestrator_run_id"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|_| Arc::clone(&self.runtime_state));
        // Phase 3 (plan D3): the conversation's `extra` JSON drives the failover
        // config resolution (session-level `extra.model_failover` override else
        // global). Captured once at turn start — the config does not change
        // mid-turn, and `perform_model_failover` re-fetches the row for the
        // freshly-written model when it rebuilds.
        let failover_extra_json = row.extra.clone();

        // Send message to the agent in a background task.
        // prompt() blocks until the PromptResponse arrives (turn completed),
        // but the HTTP handler should return 202 immediately.
        //
        // Every turn mints a fresh msg_id and passes it as the agent
        // correlation id so DB row, WebSocket stream events, and
        // agent-internal tracing all share one identifier per turn.
        let user_msg_id_ret = user_msg_id.clone();
        tokio::spawn(async move {
            let mut turn_claim = turn_claim;
            let build_started_at = now_ms();
            info!(conversation_id = %conv_id, "Agent task build started");
            let mut agent = match task_manager.get_or_build_task(&conv_id, build_opts).await {
                Ok(agent) => agent,
                Err(err) => {
                    error!(
                        conversation_id = %conv_id,
                        error_code = err.error_code(),
                        error = %ErrorChain(&err),
                        "Agent task build failed"
                    );
                    service.persist_and_broadcast_send_failure_tip(&conv_id, &err).await;
                    turn_claim.release();
                    service
                        .complete_turn_with_companion_context(&conv_id, companion, companion_id.clone(), origin.clone(), channel_platform.clone())
                        .await;
                    return;
                }
            };

            // Arm IDMM supervision now that the agent task exists (so the
            // probe's `observe` attaches to THIS turn's event stream). The
            // user-driven desktop chat path has no AutoWork loop / boot-resume
            // to arm it, so without this an enabled 智能决策 never observed the
            // turn that asks "请回复编号". Fire-and-forget; a no-op when IDMM is
            // disabled or already supervising this conversation.
            if let Some(hook) = service.current_supervision_hook() {
                hook.on_turn_start(&conv_id);
            }

            // If the factory resolved a different workspace (e.g. auto-created temp
            // dir for a legacy conversation with empty workspace), persist it back.
            if let Err(err) = service
                .maybe_persist_workspace(&conv_id, &stored_workspace, agent.workspace())
                .await
            {
                error!(
                    conversation_id = %conv_id,
                    error_code = err.error_code(),
                    error = %ErrorChain(&err),
                    "Failed to persist resolved workspace"
                );
                service.persist_and_broadcast_send_failure_tip(&conv_id, &err).await;
                turn_claim.release();
                service
                    .complete_turn_with_companion_context(&conv_id, companion, companion_id.clone(), origin.clone(), channel_platform.clone())
                    .await;
                return;
            }

            info!(
                conversation_id = %conv_id,
                agent_type = ?agent.agent_type(),
                elapsed_ms = now_ms().saturating_sub(build_started_at),
                "Agent task ready"
            );

            let mut pending_send = Some((
                SendMessageData {
                    content: req.content,
                    msg_id: first_turn_msg_id.clone(),
                    files: req.files,
                    inject_skills: req.inject_skills,
                    origin: origin.clone(),
                },
                first_turn_msg_id,
            ));
            let mut continuation_count = 0usize;
            // Phase 3 (plan D3): bounded count of model-failover switches this
            // turn. The seam stops switching at min(max_switches, queue.len()),
            // and a queue-exhausted pick surfaces the ORIGINAL error.
            let mut failover_switches_done: u32 = 0;
            // 本轮已做过的"剔图重跑"次数(bounded=1,防死循环)。
            let mut image_strip_retries_done: u32 = 0;
            // Phase 3 (review #2): models already switched to this turn. Passed
            // to the picker so it advances MONOTONICALLY — never re-tries a
            // candidate it already failed over to (no queue thrash).
            let mut failover_tried: Vec<nomifun_common::ProviderWithModel> = Vec::new();
            // Phase 3 (review #1/#5): resolve the effective failover config ONCE
            // (it does not change mid-turn). Used to build the relay's error
            // suppressor so a pre-response provider fault that WILL be failed over
            // is swallowed at source (no WS error, no error tips row) — the user
            // sees only the backup model's turn. `enabled == false` / no deps →
            // `None` → relay never suppresses (current behaviour preserved).
            let failover_config = if agent.agent_type() == AgentType::Nomi {
                service.resolve_failover_config(&failover_extra_json).await.filter(|c| c.enabled)
            } else {
                None
            };

            while let Some((current_send, msg_id)) = pending_send.take() {
                if continuation_count >= MAX_CRON_CONTINUATIONS_PER_TURN {
                    warn!(
                        conversation_id = %conv_id,
                        max = MAX_CRON_CONTINUATIONS_PER_TURN,
                        "Reached cron continuation limit; ending turn early"
                    );
                    break;
                }

                let mut relay = StreamRelay::new(
                    conv_id.clone(),
                    msg_id,
                    user_id_owned.clone(),
                    Arc::clone(&repo),
                    Arc::clone(&broadcaster),
                    cron_service.clone(),
                )
                .with_turn_completion(false)
                .with_companion_context(companion, companion_id.clone())
                .with_origin(origin.clone())
                .with_channel_platform(channel_platform.clone());

                // Orchestrator worker turns: let the relay accumulate this turn's
                // token usage into the conversation's running total (read back by
                // the worker after settle). No-op for every other conversation.
                if let Some(state) = token_runtime_state.clone() {
                    relay = relay.with_runtime_state(state);
                }

                // 为 nomi 轮安装 pre-response 错误抑制器:既隐藏"将被换模型重试"的
                // provider fault(在切换上限内),也隐藏"将被同模型剔图重试"的
                // image-unsupported 400(每轮一次)。被吞的错误进 outcome.suppressed_error,
                // 若两种重试都未触发,则下方原样 re-surface。
                if agent.agent_type() == AgentType::Nomi {
                    let failover_within_bound = failover_config.as_ref().is_some_and(|c| {
                        failover_switches_done < c.max_switches.min(c.queue.len() as u32)
                    });
                    let image_retry_available = image_strip_retries_done == 0;
                    if failover_within_bound || image_retry_available {
                        relay = relay.with_failover_suppressor(Arc::new(move |code| {
                            (failover_within_bound
                                && crate::model_failover::is_provider_fault(code))
                                || (image_retry_available
                                    && code
                                        == nomifun_api_types::AgentErrorCode::UserLlmProviderImageUnsupported)
                        }));
                    }
                }

                let rx = agent.subscribe();
                let send_agent = agent.clone();
                let conv_id_send = conv_id.clone();
                // Phase 3: keep a copy of this turn's send so a pre-response
                // provider fault can resend the SAME content to the next model.
                let resend_payload = current_send.clone();
                let (send_error_tx, send_error_rx) = oneshot::channel();
                // 1. Send the message to the agent and concurrently run the relay to stream events.
                tokio::spawn(async move {
                    if let Err(e) = send_agent.send_message(current_send).await {
                        error!(conversation_id = %conv_id_send, error = %ErrorChain(&e), "Agent send_message failed");
                        let _ = send_error_tx.send(e);
                    }
                });
                // 2. Wait for the agent to process the message and complete the turn, while the relay streams events in real time.
                let outcome = relay.consume_with_send_error(rx, send_error_rx).await;

                if let Some(session_key) = agent.get_session_key() {
                    persist_session_key(&repo, &conv_id, &session_key).await;
                }

                // Phase 3 (plan D3): model failover. Only fires on a pre-response
                // nomi provider fault, bounded by min(max_switches, queue.len()).
                // On a usable next model we swap `agent` to the rebuilt task and
                // resend the SAME content with a fresh msg_id; on None (queue
                // exhausted / disabled / not eligible) we fall through to the
                // ACP-eviction + error-surfacing path unchanged. This runs BEFORE
                // `evict_acp_task_after_terminal_error` (which only acts on ACP),
                // so a successful nomi failover short-circuits via `continue`.
                if let Some(switch) = service
                    .maybe_failover_in_send_loop(
                        &conv_id,
                        agent.agent_type(),
                        &outcome,
                        failover_switches_done,
                        &failover_tried,
                        &failover_extra_json,
                        &task_manager,
                    )
                    .await
                {
                    failover_switches_done += 1;
                    failover_tried.push(switch.picked.clone());
                    info!(
                        conversation_id = %conv_id,
                        switch = failover_switches_done,
                        provider_id = %switch.picked.provider_id,
                        model = %switch.picked.model,
                        "Model failover succeeded; resending turn to next model"
                    );
                    agent = switch.agent;
                    let resend_msg_id = Self::mint_msg_id();
                    pending_send = Some((
                        SendMessageData {
                            msg_id: resend_msg_id.clone(),
                            ..resend_payload
                        },
                        resend_msg_id,
                    ));
                    continue;
                }

                // 图片不支持降级:pre-response 的 image-unsupported 400 → 记忆 + 提示 +
                // 同模型剔图重跑(每轮一次)。重跑因命中 registry 而剔图,通常成功。
                // 未触发(已重跑过 / 非 nomi / 有响应 / 码不符 / 重建失败)则落到下方
                // re-surface,把原始错误显示给用户。
                if image_strip_retries_done == 0
                    && agent.agent_type() == AgentType::Nomi
                    && outcome.terminal.is_error()
                    && !outcome.emitted_response
                    && outcome.terminal.code()
                        == Some(nomifun_api_types::AgentErrorCode::UserLlmProviderImageUnsupported)
                {
                    if let Some(rebuilt) = service
                        .strip_images_and_rebuild(&conv_id, &task_manager)
                        .await
                    {
                        service.persist_images_stripped_tip(&conv_id).await;
                        info!(
                            conversation_id = %conv_id,
                            "Model rejected images; stripped images and resending same model"
                        );
                        agent = rebuilt;
                        image_strip_retries_done += 1;
                        let resend_msg_id = Self::mint_msg_id();
                        pending_send = Some((
                            SendMessageData {
                                msg_id: resend_msg_id.clone(),
                                ..resend_payload
                            },
                            resend_msg_id,
                        ));
                        continue;
                    }
                }

                // review #1/#5: the relay SUPPRESSED a pre-response provider error
                // expecting a failover, but the failover did NOT fire above (picker
                // exhausted at runtime / disabled / rebuild failed). Re-surface the
                // ORIGINAL error so the user is not left with a silently-dropped
                // turn — preserves the "queue-exhausted → original error" invariant.
                if let Some(suppressed) = outcome.suppressed_error.as_ref() {
                    let surface_relay = StreamRelay::new(
                        conv_id.clone(),
                        Self::mint_msg_id(),
                        user_id_owned.clone(),
                        Arc::clone(&repo),
                        Arc::clone(&broadcaster),
                        cron_service.clone(),
                    )
                    .with_companion_context(companion, companion_id.clone())
                    .with_origin(origin.clone())
                    .with_channel_platform(channel_platform.clone());
                    surface_relay.surface_terminal_error(suppressed).await;
                }

                if service
                    .evict_acp_task_after_terminal_error(&conv_id, agent.agent_type(), &outcome, &task_manager)
                    .await
                {
                    break;
                }

                if outcome.system_responses.is_empty() {
                    break;
                }
                continuation_count += 1;
                let next_turn_msg_id = Self::mint_msg_id();
                pending_send = Some((
                    SendMessageData {
                        content: outcome.system_responses.join("\n"),
                        msg_id: next_turn_msg_id.clone(),
                        files: vec![],
                        inject_skills: vec![],
                        // A system-driven continuation is not the human owner
                        // speaking; mark it so it is never distilled. Falls
                        // back to the turn's own origin when one was set.
                        origin: Some(origin.clone().unwrap_or_else(|| "autowork".to_owned())),
                    },
                    next_turn_msg_id,
                ));
            }

            turn_claim.release();
            service
                .complete_turn_with_companion_context(&conv_id, companion, companion_id, origin, channel_platform)
                .await;
        });

        info!(
            msg_id = %user_msg_id_ret,
            elapsed_ms = now_ms().saturating_sub(send_started_at),
            "Message accepted, agent work scheduled"
        );
        Ok(user_msg_id_ret)
    }

    /// Inject a user interjection into a RUNNING turn (mid-turn steering). If no
    /// turn is live, falls back to a normal [`Self::send_message`] (new turn). If
    /// the live agent cannot be steered (non-Nomi engine), returns a BadRequest the
    /// route maps to `steer_unsupported` so the client falls back to the queue.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn steer_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: SendMessageRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<String, AppError> {
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }

        // Verify conversation exists and belongs to user.
        let row = self
            .conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        // No live turn → nothing to steer; send normally (new turn).
        let Some(instance) = task_manager.get_task(conversation_id) else {
            return self.send_message(user_id, conversation_id, req, task_manager).await;
        };
        if instance.status() != Some(ConversationStatus::Running) {
            return self.send_message(user_id, conversation_id, req, task_manager).await;
        }

        // Push into the running turn's steering inbox FIRST, so we persist the
        // transcript row exactly once on the path that actually steers:
        //  - Ok(true):  steered into the live turn → persist + broadcast below.
        //  - Ok(false): the turn ended between the status check and here → fall
        //    back to a fresh send (which persists its own user row). Persisting
        //    here too would double-write the interjection.
        //  - Err: non-Nomi engine (`steer_unsupported`) → propagate. The client
        //    falls back to the pending queue, which sends (and persists) later.
        //    Persisting here would duplicate that.
        match instance.steer(req.content.clone()) {
            Ok(true) => {}
            Ok(false) => {
                return self.send_message(user_id, conversation_id, req, task_manager).await;
            }
            Err(e) => return Err(e),
        }

        // Steered successfully — persist the interjection as a normal user
        // message (transcript shows it at the point it was sent), same shape as
        // `send_message`.
        let user_msg_id = Self::mint_msg_id();
        let user_msg = nomifun_db::models::MessageRow {
            id: user_msg_id.clone(),
            conversation_id: parse_conv_id(conversation_id)?,
            msg_id: Some(user_msg_id.clone()),
            r#type: "text".into(),
            content: serde_json::json!({ "content": req.content }).to_string(),
            position: Some("right".into()),
            status: Some("finish".into()),
            hidden: req.hidden,
            created_at: now_ms(),
        };
        if let Err(e) = self.conversation_repo.insert_message(&user_msg).await {
            warn!(msg_id = %user_msg_id, error = %ErrorChain(&e), "Failed to insert steered user message");
            return Err(e.into());
        }

        info!(msg_id = %user_msg_id, "Steered interjection persisted and injected into running turn");

        // Companion wire markers (see `send_message`), so the companion collector
        // can classify this message off the wire. A mid-turn interjection is the
        // human owner speaking into a live turn — no per-turn channel marker.
        let (companion, companion_id, _) = companion_context_from_extra(&row.extra);
        self.broadcaster.broadcast(WebSocketMessage::new(
            "message.userCreated",
            serde_json::json!({
                "conversation_id": user_msg.conversation_id,
                "msg_id": &user_msg_id,
                "content": &req.content,
                "position": "right",
                "status": "finish",
                "hidden": req.hidden,
                "origin": serde_json::Value::Null,
                "companion": companion,
                "companion_id": companion_id,
                "channel_platform": serde_json::Value::Null,
                "created_at": user_msg.created_at,
            }),
        ));

        Ok(user_msg_id)
    }

    async fn persist_and_broadcast_send_failure_tip(&self, conversation_id: &str, err: &AppError) {
        let Some(row) = self.persist_send_failure_tip(conversation_id, err).await else {
            return;
        };

        let msg_id = row.msg_id.clone().unwrap_or_else(|| row.id.clone());
        let content_value: serde_json::Value =
            serde_json::from_str(&row.content).unwrap_or_else(|_| serde_json::Value::String(row.content.clone()));
        self.broadcaster.broadcast(WebSocketMessage::new(
            "message.stream",
            serde_json::json!({
                "conversation_id": row.conversation_id,
                "msg_id": msg_id,
                "type": row.r#type,
                "data": content_value,
                "position": row.position,
                "status": row.status,
                "hidden": row.hidden,
                "replace": true,
            }),
        ));
    }

    /// Insert a pre-built `MessageRow` into the conversation's message history
    /// and broadcast a `message.stream` event so live subscribers render it
    /// immediately.
    ///
    /// Used by paths outside the normal user→agent turn (e.g. the team
    /// scheduler writing an incoming teammate message as a left bubble in the
    /// target agent's conversation so the UI shows who spoke).
    pub async fn insert_raw_message(&self, row: &MessageRow) -> Result<(), AppError> {
        self.conversation_repo.insert_message(row).await?;

        let msg_id = row.msg_id.clone().unwrap_or_else(|| row.id.clone());
        let content_value: serde_json::Value =
            serde_json::from_str(&row.content).unwrap_or_else(|_| serde_json::Value::String(row.content.clone()));
        let payload = serde_json::json!({
            "conversation_id": row.conversation_id,
            "msg_id": msg_id,
            "type": row.r#type,
            "data": content_value,
            "position": row.position,
            "status": row.status,
            "hidden": row.hidden,
            "replace": true,
        });
        self.broadcaster
            .broadcast(WebSocketMessage::new("message.stream", payload));
        Ok(())
    }

    /// Edit the most recent user message and re-run from there (Nomi only).
    /// Rewinds the engine's last turn, truncates the DB transcript at the
    /// target message (inclusive), then re-sends the edited content as a fresh
    /// turn. Rejects non-Nomi conversations and any target that is not the
    /// latest user message (so the engine rewind and DB truncation stay aligned).
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id, message_id = %message_id))]
    pub async fn edit_and_resubmit(
        &self,
        user_id: &str,
        conversation_id: &str,
        message_id: &str,
        req: SendMessageRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<String, AppError> {
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }
        let conv_id = parse_conv_id(conversation_id)?;

        // 1. 归属校验
        let row = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        // 2. 仅 Nomi
        if row.r#type != "nomi" {
            return Err(AppError::BadRequest(
                "Edit & resubmit is only supported for Nomi conversations".into(),
            ));
        }

        // 3. 目标必须是最近一条用户(right/text)消息（保证引擎"回退最后一个 turn"
        //    与 DB"删除该条及其后"对齐）。
        let recent = self.conversation_repo.get_messages_keyset(conv_id, None, 50).await?;
        let latest_user = recent
            .items
            .iter()
            .find(|m| m.position.as_deref() == Some("right") && m.r#type == "text");
        let Some(target) = latest_user else {
            return Err(AppError::BadRequest("No editable user message found".into()));
        };
        if target.id != message_id {
            return Err(AppError::BadRequest(
                "Only the most recent user message can be edited".into(),
            ));
        }
        let (from_created_at, from_id) = (target.created_at, target.id.clone());

        // 4. 取在飞 agent 并回退最后一个 turn（内部会先停掉在飞 turn）。
        let agent = self.task(conversation_id)?;
        agent.rewind_last_turn().await?;

        // 5. 截断 DB：删除目标(含)及其后所有消息。
        self.conversation_repo
            .delete_messages_from(conv_id, from_created_at, &from_id)
            .await?;

        // 6. 复用正常发送：重新插入用户消息行 + 起新 turn。
        self.send_message(user_id, conversation_id, req, task_manager).await
    }

    /// Stop the current streaming response for a conversation.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn cancel(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), AppError> {
        // Verify conversation exists and belongs to user
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        // Record the user's intent BEFORE touching the agent: even when no
        // agent is live (claimed-but-not-yet-injected AutoWork window), the
        // stamp tells the orchestrator this work was deliberately stopped.
        self.note_user_cancel(conversation_id);

        let Some(agent) = task_manager.get_task(conversation_id) else {
            info!("No active agent to cancel; treating as idempotent success");
            return Ok(());
        };

        if let Err(e) = agent.cancel().await {
            warn!(error = %ErrorChain(&e), "Failed to cancel agent");
            return Err(e);
        }

        info!("Stream canceled");
        Ok(())
    }

    fn note_user_cancel(&self, conversation_id: &str) {
        if let Ok(mut stamps) = self.user_cancel_stamps.lock() {
            stamps.insert(conversation_id.to_string(), nomifun_common::now_ms());
        }
    }

    /// Whether the user cancelled this conversation's streaming response at or
    /// after `since_ms`. Used by AutoWork to classify a turn that ended while
    /// (or right before) a user cancel as a USER INTERRUPT — pause the tag —
    /// rather than a failed attempt to retry.
    pub fn user_cancelled_since(&self, conversation_id: &str, since_ms: i64) -> bool {
        self.user_cancel_stamps
            .lock()
            .ok()
            .and_then(|stamps| stamps.get(conversation_id).copied())
            .is_some_and(|stamped_at| stamped_at >= since_ms)
    }

    /// Clear a conversation's agent context ("release model context") while
    /// **keeping** the persisted message history.
    ///
    /// Unlike [`Self::reset`] (which deletes DB messages but leaves the live
    /// CLI session intact, so the model still remembers everything), this:
    ///  1. resets the live agent's in-memory/session context if one is running
    ///     (ACP rotates to a fresh `session/new`, Nomi empties its engine,
    ///     OpenClaw/Remote forget their gateway session) — see
    ///     [`AgentInstance::clear_context`]; and
    ///  2. clears the persisted `acp_session` row (NULL session_id + drop
    ///     cached usage) so a process rebuild starts fresh instead of resuming.
    ///
    /// Idempotent: a conversation with no live agent still succeeds (step 2
    /// covers the persisted-but-idle ACP case; non-ACP rows simply no-op).
    /// Message rows are intentionally left untouched.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn clear_context(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), AppError> {
        // Verify conversation exists and belongs to user.
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        // 1. Reset the live agent's context, if one is running.
        if let Some(agent) = task_manager.get_task(conversation_id) {
            agent.clear_context().await?;
        } else {
            info!("No active agent; clearing persisted state only");
        }

        // 2. Forget the persisted ACP session so a rebuild does not resume the
        //    old session. Returns false (no-op) for non-ACP conversations.
        if let Err(e) = self.acp_session_repo.clear_session_id(parse_conv_id(conversation_id)?).await {
            warn!(error = %ErrorChain(&e), "Failed to clear persisted acp_session during clear_context");
        }

        info!("Conversation context cleared");
        Ok(())
    }

    /// Clear a conversation's **messages** (and artifacts) while keeping the
    /// conversation row — the work-partner「清空上下文」按钮。
    ///
    /// Combines [`Self::reset`]'s message/artifact deletion with
    /// [`Self::clear_context`]'s live-agent reset, but — unlike `reset` — it
    /// does **not** touch `status`. It also never touches the companion store:
    /// `companion_memories` live in a separate sqlite owned by another crate, so
    /// wiping a session's transcript leaves accumulated memories intact.
    ///
    /// Idempotent: a conversation with no live agent still succeeds (the ACP
    /// session clear no-ops for non-ACP rows).
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn clear_messages(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), AppError> {
        // Verify existence and ownership (same pattern as clear_context).
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        // 1. Delete the persisted transcript and artifacts (NOT status).
        let conv_id = parse_conv_id(conversation_id)?;
        self.conversation_repo.delete_messages_by_conversation(conv_id).await?;
        self.conversation_repo.delete_artifacts_by_conversation(conv_id).await?;

        // 2. Reset the live agent's context, if one is running (same as
        //    clear_context: ACP rotates to a fresh session, Nomi empties its
        //    engine, etc.).
        if let Some(agent) = task_manager.get_task(conversation_id) {
            agent.clear_context().await?;
        } else {
            info!("No active agent; clearing persisted messages only");
        }

        // 3. Forget the persisted ACP session so a rebuild does not resume the
        //    old session. Returns false (no-op) for non-ACP conversations.
        if let Err(e) = self.acp_session_repo.clear_session_id(conv_id).await {
            warn!(error = %ErrorChain(&e), "Failed to clear persisted acp_session during clear_messages");
        }

        info!("Conversation messages cleared");
        Ok(())
    }

    /// Pre-initialize an agent task for a conversation (warmup).
    ///
    /// This builds the agent task without sending a message, so the
    /// first real message can be processed faster.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn warmup(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), AppError> {
        let row = self
            .conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let mut build_opts = self.build_task_options(&row)?;
        self.ensure_auto_workspace_skill_links(&row, &build_opts).await;
        self.apply_knowledge_mounts(&row, &mut build_opts).await;
        let stored_workspace = build_opts.workspace.clone();
        let agent = task_manager.get_or_build_task(conversation_id, build_opts).await?;

        // Persist auto-resolved workspace if factory picked a different path.
        self.maybe_persist_workspace(conversation_id, &stored_workspace, agent.workspace())
            .await?;

        debug!("Agent warmed up");
        Ok(())
    }
}

// ── Internal Helpers ────────────────────────────────────────────────

impl ConversationService {
    /// Build [`BuildTaskOptions`] from a conversation database row.
    ///
    /// Provider/model resolution lives in [`crate::task_options::provider_model_from_conversation_row`]
    /// so the cron executor can derive identical values for the same row.
    /// Diverging the lookup here historically produced
    /// `Provider '<vendor>' not found` failures under cron when the
    /// interactive path worked fine (Sentry ELECTRON-1HM).
    pub(crate) fn build_task_options(&self, row: &nomifun_db::models::ConversationRow) -> Result<BuildTaskOptions, AppError> {
        let agent_type = string_to_enum(&row.r#type)?;

        let model = crate::task_options::provider_model_from_conversation_row(row);

        let mut extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|e| AppError::Internal(format!("Invalid extra JSON: {e}")))?;

        // Inject user_id into extra so the Guide MCP bridge can pass it to
        // nomi_create_team without a separate lookup. Harmless for non-ACP types.
        if let Some(obj) = extra.as_object_mut() {
            obj.entry("user_id")
                .or_insert_with(|| serde_json::Value::String(row.user_id.clone()));
        }

        // Extract workspace from extra (common across agent types)
        let workspace = match extra.get("workspace").and_then(|v| v.as_str()) {
            Some(workspace) if !workspace.is_empty() => {
                let normalized = validate_runtime_workspace_path(workspace)?;
                if normalized != workspace {
                    extra["workspace"] = serde_json::Value::String(normalized.clone());
                }
                normalized
            }
            _ => String::new(),
        };

        Ok(BuildTaskOptions {
            agent_type,
            workspace,
            model,
            conversation_id: row.id.to_string(),
            extra,
            // Stamp/validate the nomi session against this conversation instance.
            conversation_created_at: Some(row.created_at),
        })
    }

    async fn ensure_auto_workspace_skill_links(&self, row: &ConversationRow, build_opts: &BuildTaskOptions) {
        let expected_workspace = self.workspace_root.join("conversations").join(format!(
            "{}-temp-{}",
            conversation_label(&build_opts.agent_type, build_opts.extra.get("backend")),
            row.id
        ));

        let stored_workspace = build_opts.workspace.trim();
        let workspace = if stored_workspace.is_empty() {
            expected_workspace
        } else {
            let workspace = PathBuf::from(stored_workspace);
            if workspace != expected_workspace {
                return;
            }
            workspace
        };

        let skill_names = build_opts
            .extra
            .get("skills")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default();
        if skill_names.is_empty() {
            return;
        }

        let Some(rel_dirs) = native_skills_dirs(
            &self.agent_metadata_repo,
            &build_opts.agent_type,
            build_opts.extra.get("backend"),
        )
        .await
        else {
            return;
        };
        if rel_dirs.is_empty() {
            return;
        }

        let resolved = self.skill_resolver.resolve_skills(&skill_names).await;
        if resolved.is_empty() {
            return;
        }

        let rel_dirs_refs: Vec<&str> = rel_dirs.iter().map(String::as_str).collect();
        let n = self
            .skill_resolver
            .link_workspace_skills(&workspace, &rel_dirs_refs, &resolved)
            .await;
        debug!(
            conversation_id = %row.id,
            workspace = %workspace.display(),
            links = n,
            "ensured skill symlinks in auto workspace"
        );
    }

    /// Mount the knowledge bases bound to this conversation into its
    /// workspace (idempotent sync — stale links from a changed binding are
    /// removed) and surface the result through `extra.knowledge_mounts` /
    /// `extra.knowledge_writeback` so the ACP assembler can compose the
    /// knowledge prompt section.
    ///
    /// Unlike skill links, this also applies to user-chosen custom
    /// workspaces: the binding is explicit per-session opt-in, and the mounts
    /// stay confined to the hidden `.nomi/knowledge/` directory. Never
    /// fails the task start — mount errors degrade to warnings.
    ///
    /// Binding target selection (spec §3 ruling 6 / §4.5): a conversation
    /// whose `extra.companionId` is a non-blank string mounts the companion-level
    /// binding `('companion', companionId)`; everything else keeps the per-conversation
    /// binding `('conversation', conversation_id)`. No merge between the two.
    async fn apply_knowledge_mounts(&self, row: &ConversationRow, build_opts: &mut BuildTaskOptions) {
        let service = self.knowledge_service.read().ok().and_then(|guard| guard.clone());
        let Some(service) = service else { return };

        let stored_workspace = build_opts.workspace.trim();
        let workspace = if stored_workspace.is_empty() {
            self.workspace_root.join("conversations").join(format!(
                "{}-temp-{}",
                conversation_label(&build_opts.agent_type, build_opts.extra.get("backend")),
                row.id
            ))
        } else {
            PathBuf::from(stored_workspace)
        };

        let (target_kind, target_id) = knowledge_binding_target(&build_opts.extra, &build_opts.conversation_id);
        let target_id = target_id.to_owned();
        // Workpath-first for conversation sessions (session-list unification
        // spec §7): the binding belongs to the workspace path, not the
        // individual conversation. `session_workpath_key` maps a
        // backend-managed (temporary) workspace — one under `workspace_root`,
        // the same root `row_to_response` treats as the data dir for the
        // `is_temporary_workspace` flag — to the `__default__` sentinel, and
        // every user-chosen directory to its normalized key. The knowledge
        // service looks up the `('workpath', key)` row first and only falls
        // back to the legacy `('conversation', id)` binding on a full miss.
        // Companion sessions keep their `('companion', companionId)` binding unchanged — they
        // are not per-workspace.
        let outcome = if target_kind == "conversation" {
            let wp_key = nomifun_knowledge::session_workpath_key(&workspace, &self.workspace_root);
            service
                .ensure_mounts_for_session(&wp_key, target_kind, &target_id, &workspace)
                .await
        } else {
            service
                .ensure_mounts_for_target(target_kind, &target_id, &workspace)
                .await
        };

        // Recycle the cached agent when the resolved knowledge context changed
        // since it was last built. The agent bakes the retrieval-protocol
        // section into its prompt at build time and is cached per conversation
        // (`get_or_build_task` is a per-conversation `OnceCell`), so a
        // `挂载知识库` toggle on an already-warmed/used session would otherwise
        // never reach the running agent — the freshly-resolved mounts here would
        // be discarded by the cache. That silently breaks the UI's promise that
        // a binding change "takes effect on the next message" (the reported bug:
        // KB enabled mid-session → task dispatched → retrieval never triggers).
        // Killing the in-memory task lets the imminent `get_or_build_task`
        // rebuild with the new mounts; the conversation and any persisted ACP
        // session are preserved (the rebuilt ACP agent resumes and re-delivers
        // the section via the knowledge prelude hook).
        let conversation_id = build_opts.conversation_id.clone();
        let new_signature = knowledge_mounts_signature(&outcome);
        let signature_changed =
            self.runtime_state.knowledge_signature(&conversation_id).as_deref() != Some(new_signature.as_str());
        if signature_changed {
            match self.task_manager.get_task(&conversation_id) {
                // Mid-turn: never recycle a running agent (it would abort the
                // live turn). Leave the signature stale so the next idle send
                // or warmup reconciles the change.
                Some(agent) if agent.status() == Some(ConversationStatus::Running) => {
                    debug!(
                        conversation_id = %conversation_id,
                        "knowledge binding changed while agent is mid-turn; deferring recycle"
                    );
                }
                Some(_) => {
                    info!(
                        conversation_id = %conversation_id,
                        "knowledge binding changed; recycling cached agent so the new mounts take effect on the next message"
                    );
                    if let Err(e) = self
                        .task_manager
                        .kill(&conversation_id, Some(AgentKillReason::KnowledgeBindingChanged))
                    {
                        warn!(
                            conversation_id = %conversation_id,
                            error = %ErrorChain(&e),
                            "failed to recycle agent after knowledge binding change"
                        );
                    }
                    self.runtime_state.set_knowledge_signature(&conversation_id, new_signature);
                }
                // No live agent yet — the imminent build bakes the current
                // mounts. Just record the signature for future change detection.
                None => {
                    self.runtime_state.set_knowledge_signature(&conversation_id, new_signature);
                }
            }
        }

        let Some(obj) = build_opts.extra.as_object_mut() else { return };
        if outcome.mounts.is_empty() {
            obj.remove("knowledge_mounts");
            obj.remove("knowledge_writeback");
            obj.remove("knowledge_writeback_mode");
            obj.remove("knowledge_writeback_eagerness");
            obj.remove("knowledge_channel_write_enabled");
            return;
        }
        debug!(
            conversation_id = %row.id,
            target_kind,
            target_id = %target_id,
            mounts = outcome.mounts.len(),
            writeback = outcome.writeback,
            writeback_mode = %outcome.writeback_mode,
            writeback_eagerness = %outcome.writeback_eagerness,
            "knowledge bases mounted into workspace"
        );
        obj.insert("knowledge_mounts".into(), serde_json::json!(outcome.mounts));
        obj.insert(
            "knowledge_writeback".into(),
            serde_json::Value::Bool(outcome.writeback),
        );
        obj.insert(
            "knowledge_writeback_mode".into(),
            serde_json::Value::String(outcome.writeback_mode),
        );
        obj.insert(
            "knowledge_writeback_eagerness".into(),
            serde_json::Value::String(outcome.writeback_eagerness),
        );
        obj.insert(
            "knowledge_channel_write_enabled".into(),
            serde_json::Value::Bool(outcome.channel_write_enabled),
        );
    }

    /// Write the resolved workspace back to `conversation.extra.workspace` when
    /// the factory picked a different (auto-generated) path than what was stored.
    ///
    /// This handles legacy conversations whose `extra.workspace` was empty:
    /// the factory creates a temp dir at task-build time, and we persist that
    /// path here so the frontend can display the workspace panel correctly.
    async fn maybe_persist_workspace(
        &self,
        conversation_id: &str,
        stored_workspace: &str,
        resolved_workspace: &str,
    ) -> Result<(), AppError> {
        if resolved_workspace.is_empty() || resolved_workspace == stored_workspace {
            return Ok(());
        }

        // Fetch latest extra, merge the resolved workspace path in, and persist.
        let row = self
            .conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .ok_or_else(|| AppError::Internal("Conversation vanished during workspace sync".into()))?;

        let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        extra["workspace"] = serde_json::Value::String(resolved_workspace.to_owned());

        let extra_json =
            serde_json::to_string(&extra).map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?;

        let update = ConversationRowUpdate {
            extra: Some(extra_json),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo.update(parse_conv_id(conversation_id)?, &update).await?;

        debug!(
            conversation_id,
            workspace = resolved_workspace,
            "Persisted auto-resolved workspace to conversation.extra"
        );
        Ok(())
    }

    /// Broadcast a `conversation.listChanged` WebSocket event.
    pub(crate) fn broadcast_list_changed(
        &self,
        conversation_id: &str,
        action: &str,
        source: Option<&ConversationSource>,
    ) {
        let payload = serde_json::json!({
            // The wire `conversation_id` mirrors the i64 DTO id. Callers pass
            // the String form (their public-API key); emit the integer.
            "conversation_id": parse_conv_id(conversation_id).unwrap_or_default(),
            "action": action,
            "source": source,
        });
        let event = WebSocketMessage::new("conversation.listChanged", payload);
        self.broadcaster.broadcast(event);
    }

    fn current_cron_service(&self) -> Option<Arc<dyn ICronService>> {
        match self.cron_service.read() {
            Ok(guard) => guard.as_ref().map(Arc::clone),
            Err(_) => None,
        }
    }

    fn current_supervision_hook(&self) -> Option<Arc<dyn ConversationSupervisionHook>> {
        match self.supervision_hook.read() {
            Ok(guard) => guard.as_ref().map(Arc::clone),
            Err(_) => None,
        }
    }

    /// Backfill `extra.skills` if the row predates the snapshot model.
    /// Persists the mutation asynchronously; failures are logged and
    /// swallowed so a read path never 500s because of a backfill write
    /// failure.
    async fn backfill_extra_inplace(&self, conversation_id: i64, extra: &mut serde_json::Value) {
        let auto_inject = self.skill_resolver.auto_inject_names().await;
        let mutated = backfill_skills_if_missing(extra, &auto_inject);
        if !mutated {
            return;
        }
        let serialized = match serde_json::to_string(extra) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    conversation_id,
                    error = %ErrorChain(&e),
                    "backfill serialize failed; returning in-memory value"
                );
                return;
            }
        };
        let update = ConversationRowUpdate {
            extra: Some(serialized),
            ..Default::default()
        };
        if let Err(e) = self.conversation_repo.update(conversation_id, &update).await {
            warn!(
                conversation_id,
                error = %ErrorChain(&e),
                "backfill persist failed; returning in-memory value"
            );
        }
    }
}

fn normalize_workspace_extra(extra: &mut serde_json::Value) -> Result<(), AppError> {
    let Some(obj) = extra.as_object_mut() else {
        return Ok(());
    };
    let Some(workspace) = obj
        .get("workspace")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
    else {
        return Ok(());
    };
    if workspace.is_empty() {
        return Ok(());
    }

    let normalized = normalize_workspace_path(&workspace)?;
    if normalized != workspace.as_str() {
        obj.insert("workspace".to_owned(), serde_json::Value::String(normalized));
    }
    Ok(())
}

fn normalize_workspace_path(workspace: &str) -> Result<String, AppError> {
    if workspace.trim().is_empty() {
        return Err(AppError::BadRequest("Workspace directory is empty".into()));
    }

    let workspace_path = PathBuf::from(workspace);
    if workspace_path_has_edge_whitespace_segment(&workspace_path) {
        return Err(AppError::WorkspacePathEdgeWhitespace(
            workspace_path.display().to_string(),
        ));
    }

    Ok(workspace.to_owned())
}

fn validate_runtime_workspace_path(workspace: &str) -> Result<String, AppError> {
    if workspace.trim().is_empty() {
        return Err(AppError::BadRequest("Workspace directory is empty".into()));
    }

    let workspace_path = PathBuf::from(workspace);
    if workspace_path_has_edge_whitespace_segment(&workspace_path) {
        return Err(AppError::WorkspacePathEdgeWhitespaceRuntimeUnsupported(
            workspace_path.display().to_string(),
        ));
    }

    Ok(workspace.to_owned())
}

// ── Helpers ────────────────────────────────────────────────────────

/// Compute the label used in auto-provisioned workspace directory names.
///
/// For ACP conversations the label is the vendor string from
/// `extra.backend` (e.g. `"claude"`); otherwise the `AgentType` serde
/// name (e.g. `"nomi"`). Falls back to the agent type's serde name
/// when the backend field is missing or not a string.
fn conversation_label(agent_type: &AgentType, backend: Option<&serde_json::Value>) -> String {
    if *agent_type == AgentType::Acp
        && let Some(serde_json::Value::String(s)) = backend
        && !s.is_empty()
    {
        return s.clone();
    }
    agent_type.serde_name().to_owned()
}

/// Resolve the native skills directory list for an agent by looking it
/// up in the `agent_metadata` catalog (ACP vendors) or the bundled
/// `AgentType` table (non-ACP built-ins).
///
/// Returns `None` when the agent does not support native skill
/// discovery — callers should then skip the workspace-symlink step and
/// rely on prompt injection instead.
async fn native_skills_dirs(
    repo: &Arc<dyn IAgentMetadataRepository>,
    agent_type: &AgentType,
    backend: Option<&serde_json::Value>,
) -> Option<Vec<String>> {
    if *agent_type == AgentType::Acp
        && let Some(serde_json::Value::String(vendor)) = backend
        && !vendor.is_empty()
    {
        let row = repo.find_builtin_by_backend(vendor).await.ok().flatten()?;
        let raw = row.native_skills_dirs?;
        return serde_json::from_str::<Vec<String>>(&raw).ok();
    }
    agent_type
        .native_skills_dirs()
        .map(|dirs| dirs.iter().map(|s| (*s).to_owned()).collect())
}

impl ConversationService {
    async fn resolve_mcp_support_policy(
        &self,
        agent_type: &AgentType,
        extra: &serde_json::Value,
    ) -> Result<McpSupportPolicy, AppError> {
        match agent_type {
            AgentType::Acp => resolve_acp_mcp_support_policy(&self.agent_metadata_repo, extra).await,
            AgentType::Nomi => Ok(McpSupportPolicy::NOMI),
            _ => Ok(McpSupportPolicy::NOMI),
        }
    }
}

async fn resolve_acp_mcp_support_policy(
    repo: &Arc<dyn IAgentMetadataRepository>,
    extra: &serde_json::Value,
) -> Result<McpSupportPolicy, AppError> {
    let agent_id = extra
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    let backend = extra
        .get("backend")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    let agent_source = extra
        .get("agent_source")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("builtin");

    let row = match agent_id {
        Some(id) => repo
            .get(id)
            .await
            .map_err(|e| AppError::Internal(format!("agent_metadata lookup: {e}")))?,
        None if agent_source == "builtin" => match backend {
            Some(vendor) => repo
                .find_builtin_by_backend(vendor)
                .await
                .map_err(|e| AppError::Internal(format!("agent_metadata lookup: {e}")))?,
            None => None,
        },
        None => None,
    };

    let capabilities = row
        .as_ref()
        .and_then(|row| row.agent_capabilities.as_deref())
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .map(|value| parse_acp_mcp_capabilities(&value))
        .unwrap_or_default();

    Ok(McpSupportPolicy::from_acp_capabilities(capabilities))
}

fn upsert_conversation_mcp_status(
    statuses: &mut Vec<ConversationMcpStatus>,
    status_index_by_name: &mut HashMap<String, usize>,
    status: ConversationMcpStatus,
) {
    if let Some(index) = status_index_by_name.get(&status.name).copied() {
        statuses[index] = status;
        return;
    }
    status_index_by_name.insert(status.name.clone(), statuses.len());
    statuses.push(status);
}

fn classify_repo_mcp_status(
    row: &nomifun_db::models::McpServerRow,
    support: McpSupportPolicy,
) -> ConversationMcpStatus {
    if !support.supports_row_transport(&row.transport_type) {
        return ConversationMcpStatus {
            id: row.id.to_string(),
            name: row.name.clone(),
            status: ConversationMcpStatusKind::Unsupported,
            reason: Some(format!(
                "transport '{}' is not supported by this agent",
                row.transport_type
            )),
        };
    }

    match validate_repo_transport(row.transport_type.as_str(), &row.transport_config) {
        Ok(()) => ConversationMcpStatus {
            id: row.id.to_string(),
            name: row.name.clone(),
            status: ConversationMcpStatusKind::Loaded,
            reason: None,
        },
        Err(reason) => ConversationMcpStatus {
            id: row.id.to_string(),
            name: row.name.clone(),
            status: ConversationMcpStatusKind::Failed,
            reason: Some(reason),
        },
    }
}

fn classify_session_mcp_status(server: &SessionMcpServer, support: McpSupportPolicy) -> ConversationMcpStatus {
    if !support.supports_session_transport(&server.transport) {
        let transport = match &server.transport {
            SessionMcpTransport::Stdio { .. } => "stdio",
            SessionMcpTransport::Http { .. } => "http",
            SessionMcpTransport::Sse { .. } => "sse",
            SessionMcpTransport::StreamableHttp { .. } => "streamable_http",
        };
        return ConversationMcpStatus {
            id: server.id.clone(),
            name: server.name.clone(),
            status: ConversationMcpStatusKind::Unsupported,
            reason: Some(format!("transport '{transport}' is not supported by this agent")),
        };
    }

    match validate_session_transport(&server.transport) {
        Ok(()) => ConversationMcpStatus {
            id: server.id.clone(),
            name: server.name.clone(),
            status: ConversationMcpStatusKind::Loaded,
            reason: None,
        },
        Err(reason) => ConversationMcpStatus {
            id: server.id.clone(),
            name: server.name.clone(),
            status: ConversationMcpStatusKind::Failed,
            reason: Some(reason),
        },
    }
}

fn validate_repo_transport(transport_type: &str, transport_config: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(transport_config).map_err(|e| format!("invalid transport config: {e}"))?;

    match transport_type {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "stdio transport is missing command".to_owned())?;
            validate_stdio_command(command)
        }
        "http" | "streamable_http" => validate_url_field("http", value.get("url").and_then(serde_json::Value::as_str)),
        "sse" => validate_url_field("sse", value.get("url").and_then(serde_json::Value::as_str)),
        other => Err(format!("unknown transport type: {other}")),
    }
}

fn validate_session_transport(transport: &SessionMcpTransport) -> Result<(), String> {
    match transport {
        SessionMcpTransport::Stdio { command, .. } => validate_stdio_command(command),
        SessionMcpTransport::Http { url, .. } => validate_url_field("http", Some(url)),
        SessionMcpTransport::Sse { url, .. } => validate_url_field("sse", Some(url)),
        SessionMcpTransport::StreamableHttp { url, .. } => validate_url_field("streamable_http", Some(url)),
    }
}

fn validate_stdio_command(command: &str) -> Result<(), String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("stdio transport is missing command".to_owned());
    }

    let path = std::path::Path::new(trimmed);
    let looks_like_path = path.is_absolute()
        || trimmed.contains(std::path::MAIN_SEPARATOR)
        || trimmed.contains('/')
        || trimmed.contains('\\');

    if looks_like_path {
        if path.exists() {
            return Ok(());
        }
        return Err(format!("command '{trimmed}' does not exist"));
    }

    if resolve_command_path(trimmed).is_some() {
        Ok(())
    } else {
        Err(format!("command '{trimmed}' was not found in PATH"))
    }
}

fn validate_url_field(transport: &str, url: Option<&str>) -> Result<(), String> {
    match url.map(str::trim).filter(|value| !value.is_empty()) {
        Some(_) => Ok(()),
        None => Err(format!("{transport} transport is missing url")),
    }
}

/// Serialize a serde-compatible enum to its JSON string form for DB storage.
///
/// e.g. `AgentType::Acp` → `"acp"`
fn enum_to_db<T: serde::Serialize>(val: &T) -> Result<String, AppError> {
    let json_val =
        serde_json::to_value(val).map_err(|e| AppError::Internal(format!("Enum serialization failed: {e}")))?;
    json_val
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| AppError::Internal("Expected string enum value".into()))
}

/// Persist the agent's session key into `conversation.extra.sessionKey`.
///
/// Called after send_message completes so the session can be resumed
/// when the user re-enters this conversation later.
async fn persist_session_key(repo: &Arc<dyn IConversationRepository>, conversation_id: &str, session_key: &str) {
    let Ok(conv_id) = parse_conv_id(conversation_id) else {
        return;
    };
    let row = match repo.get(conv_id).await {
        Ok(Some(r)) => r,
        _ => return,
    };

    let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));

    if extra.get("sessionKey").and_then(|v| v.as_str()) == Some(session_key) {
        return;
    }

    extra["sessionKey"] = serde_json::Value::String(session_key.to_owned());

    let extra_json = match serde_json::to_string(&extra) {
        Ok(j) => j,
        Err(e) => {
            warn!(conversation_id, error = %ErrorChain(&e), "Failed to serialize extra for session key persist");
            return;
        }
    };

    let update = ConversationRowUpdate {
        extra: Some(extra_json),
        updated_at: Some(now_ms()),
        ..Default::default()
    };
    if let Err(e) = repo.update(conv_id, &update).await {
        warn!(conversation_id, error = %ErrorChain(&e), "Failed to persist session key");
    } else {
        debug!(conversation_id, "Persisted session key to conversation.extra");
    }
}

fn legacy_cron_trigger_to_artifact(row: MessageRow) -> Result<ConversationArtifactResponse, AppError> {
    let payload: serde_json::Value = serde_json::from_str(&row.content)
        .map_err(|e| AppError::Internal(format!("Invalid legacy cron trigger payload JSON: {e}")))?;
    let cron_job_id = payload
        .get("cron_job_id")
        .or_else(|| payload.get("cronJobId"))
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);

    // Legacy cron-trigger cards are synthesized from `messages`, not backed by a
    // real `conversation_artifacts` row, so they have no allocated INTEGER PK.
    // The artifact id is now i64, so we mint a stable *negative* sentinel from a
    // hash of the source message id: negatives never collide with real
    // (positive, auto-incremented) artifact ids, and distinct legacy messages
    // still get distinct keys for the frontend list.
    let synthetic_id = {
        let mut hasher = DefaultHasher::new();
        row.id.hash(&mut hasher);
        // Fold to a negative i64 (avoid i64::MIN's abs overflow).
        -((hasher.finish() >> 1) as i64).abs().max(1)
    };

    Ok(ConversationArtifactResponse {
        id: synthetic_id,
        conversation_id: row.conversation_id,
        cron_job_id,
        kind: ConversationArtifactKind::CronTrigger,
        status: ConversationArtifactStatus::Active,
        payload,
        created_at: row.created_at,
        updated_at: row.created_at,
    })
}

/// Merge `patch` into `base` (top-level key overwrite).
fn merge_json(base: &mut serde_json::Value, patch: &serde_json::Value) {
    if let (Some(base_obj), Some(patch_obj)) = (base.as_object_mut(), patch.as_object()) {
        for (key, value) in patch_obj {
            base_obj.insert(key.clone(), value.clone());
        }
    }
}

/// Parse a message keyset cursor `"<created_at_ms>:<id>"` — the oldest message
/// currently loaded in the client. The id (`msg_{uuidv7}`) contains no `:`, so
/// splitting on the first `:` is unambiguous.
fn parse_message_cursor(cursor: &str) -> Result<(i64, String), AppError> {
    let (created_at, id) = cursor
        .split_once(':')
        .ok_or_else(|| AppError::BadRequest(format!("invalid message cursor (expected '<created_at>:<id>'): {cursor}")))?;
    let created_at: i64 = created_at
        .parse()
        .map_err(|_| AppError::BadRequest(format!("invalid message cursor created_at: {cursor}")))?;
    if id.is_empty() {
        return Err(AppError::BadRequest(format!("invalid message cursor id: {cursor}")));
    }
    Ok((created_at, id.to_owned()))
}

/// Parse the companion-companion wire markers from a conversation row's `extra`
/// JSON string: (`extra.companionSession == true`, non-blank `extra.companionId`,
/// non-blank `extra.channelPlatform`).
///
/// These markers ride on `message.userCreated` / `message.stream` /
/// `turn.completed` broadcasts so downstream consumers (the companion memory
/// collector, the companion window's remote-turn bubble) can recognize companion
/// conversations — including channel master sessions that never register in
/// the companion-side thread table — straight off the wire.
fn companion_context_from_extra(extra: &str) -> (bool, Option<String>, Option<String>) {
    let value: serde_json::Value = serde_json::from_str(extra).unwrap_or_default();
    let companion = value
        .get("companionSession")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let companion_id = value
        .get("companionId")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let channel_platform = value
        .get("channelPlatform")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    (companion, companion_id, channel_platform)
}

/// Decide which knowledge-binding target a conversation mounts from
/// (spec §3 ruling 6 / §4.5).
///
/// A conversation whose `extra.companionId` is a non-blank string routes to the
/// companion-level binding `("companion", companionId)` — companion sessions and channel
/// master sessions of a companion share its knowledge. Anything else (missing
/// key, non-string value, empty or whitespace-only string) falls back to
/// the per-conversation binding `("conversation", conversation_id)`.
/// No merge semantics: exactly one target applies.
fn knowledge_binding_target<'a>(extra: &'a serde_json::Value, conversation_id: &'a str) -> (&'static str, &'a str) {
    match extra.get("companionId").and_then(serde_json::Value::as_str).map(str::trim) {
        Some(companion_id) if !companion_id.is_empty() => ("companion", companion_id),
        _ => ("conversation", conversation_id),
    }
}

/// Stable signature of the knowledge context an agent would be built with,
/// used by [`ConversationService::apply_knowledge_mounts`] to detect a binding
/// or mount change and recycle the cached agent. Covers the mounted bases
/// (id, name, relative path, TOC, summary, live sources — everything
/// [`nomifun_knowledge::build_knowledge_context`] renders) plus the write-back
/// contract. An empty mount set yields a stable "no knowledge" signature, so
/// turning a binding OFF is detected the same as turning it on.
fn knowledge_mounts_signature(outcome: &nomifun_knowledge::MountOutcome) -> String {
    let mounts = serde_json::to_string(&outcome.mounts).unwrap_or_default();
    format!(
        "{}|{}|{}|{}|{}",
        mounts,
        outcome.writeback,
        outcome.writeback_mode,
        outcome.writeback_eagerness,
        outcome.channel_write_enabled
    )
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AssistantLineage<'a> {
    agent_type: &'a str,
    preset_assistant_id: &'a str,
    custom_agent_id: &'a str,
    agent_id: &'a str,
    agent_name: &'a str,
    backend: &'a str,
    current_model_id: &'a str,
    session_mode: &'a str,
}

impl<'a> AssistantLineage<'a> {
    fn from_response_and_extra(response: &'a ConversationResponse, extra: &'a serde_json::Value) -> Self {
        fn s<'a>(extra: &'a serde_json::Value, key: &str) -> &'a str {
            extra.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
        }
        Self {
            agent_type: response.r#type.serde_name(),
            preset_assistant_id: s(extra, "preset_assistant_id"),
            custom_agent_id: s(extra, "custom_agent_id"),
            agent_id: s(extra, "agent_id"),
            agent_name: s(extra, "agent_name"),
            backend: s(extra, "backend"),
            current_model_id: s(extra, "current_model_id"),
            session_mode: s(extra, "session_mode"),
        }
    }

    fn has_any_identity(&self) -> bool {
        !self.preset_assistant_id.is_empty()
            || !self.custom_agent_id.is_empty()
            || !self.agent_id.is_empty()
            || !self.agent_name.is_empty()
    }
}

fn log_conversation_created(response: &ConversationResponse, extra: &serde_json::Value) {
    let lineage = AssistantLineage::from_response_and_extra(response, extra);
    if lineage.has_any_identity() {
        info!(
            conversation_id = %response.id,
            agent_type = lineage.agent_type,
            preset_assistant_id = lineage.preset_assistant_id,
            custom_agent_id = lineage.custom_agent_id,
            agent_id = lineage.agent_id,
            agent_name = lineage.agent_name,
            backend = lineage.backend,
            current_model_id = lineage.current_model_id,
            session_mode = lineage.session_mode,
            "Conversation created from assistant"
        );
    } else {
        info!(
            conversation_id = %response.id,
            agent_type = lineage.agent_type,
            "Conversation created (no assistant)"
        );
    }
}

fn is_tool_message_type(message_type: MessageType) -> bool {
    matches!(
        message_type,
        MessageType::ToolCall | MessageType::ToolGroup | MessageType::AcpToolCall
    )
}

/// Parse the optional per-conversation MCP-server selection out of the request
/// `extra`. `None` = the client sent no `selected_mcp_server_ids` key (→ bind
/// all enabled non-builtin servers); `Some(ids)` = an explicit selection
/// (`Some(vec![])` is a deliberate "select none"). The key is consumed.
///
/// Accepts ids as JSON numbers (current clients, integer-PK era) OR numeric
/// strings (legacy TEXT-PK clients / saved presets). The integer-PK migration
/// flipped this wire field from string[] to number[]; the previous
/// `Vec<String>`-only deserialize silently dropped every numeric id — yielding
/// `Some([])` ("select none") and disabling per-conversation MCP selection on
/// every new conversation. Lenient by design so all client versions resolve.
fn parse_selected_mcp_server_ids(obj: &mut serde_json::Map<String, serde_json::Value>) -> Option<Vec<i64>> {
    if !obj.contains_key("selected_mcp_server_ids") {
        return None;
    }
    let ids = match obj.remove("selected_mcp_server_ids") {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::Number(n) => n.as_i64(),
                serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enum_to_db_agent_type() {
        use nomifun_common::AgentType;
        assert_eq!(enum_to_db(&AgentType::Acp).unwrap(), "acp");
        assert_eq!(enum_to_db(&AgentType::Nanobot).unwrap(), "nanobot");
        assert_eq!(enum_to_db(&AgentType::OpenclawGateway).unwrap(), "openclaw-gateway");
    }

    #[test]
    fn enum_to_db_status() {
        assert_eq!(enum_to_db(&ConversationStatus::Pending).unwrap(), "pending");
        assert_eq!(enum_to_db(&ConversationStatus::Running).unwrap(), "running");
        assert_eq!(enum_to_db(&ConversationStatus::Finished).unwrap(), "finished");
    }

    #[test]
    fn enum_to_db_source() {
        assert_eq!(enum_to_db(&ConversationSource::Nomifun).unwrap(), "nomifun");
        assert_eq!(enum_to_db(&ConversationSource::Telegram).unwrap(), "telegram");
    }

    #[test]
    fn parse_selected_mcp_ids_accepts_number_array() {
        // REGRESSION: after the integer-PK migration the frontend sends
        // selected_mcp_server_ids as a JSON NUMBER array; the old Vec<String>
        // deserialize silently dropped them all (→ Some([]) = "select none"),
        // disabling per-conversation MCP server selection.
        let mut obj = json!({ "selected_mcp_server_ids": [1, 2, 3] }).as_object().unwrap().clone();
        assert_eq!(parse_selected_mcp_server_ids(&mut obj), Some(vec![1, 2, 3]));
    }

    #[test]
    fn parse_selected_mcp_ids_accepts_legacy_string_array() {
        // Back-compat: older clients / saved presets sent numeric STRINGS.
        let mut obj = json!({ "selected_mcp_server_ids": ["4", "5"] }).as_object().unwrap().clone();
        assert_eq!(parse_selected_mcp_server_ids(&mut obj), Some(vec![4, 5]));
    }

    #[test]
    fn parse_selected_mcp_ids_absent_is_none() {
        // No key → None → bind all enabled non-builtin servers (NOT "select none").
        let mut obj = json!({ "workspace": "/p" }).as_object().unwrap().clone();
        assert_eq!(parse_selected_mcp_server_ids(&mut obj), None);
    }

    #[test]
    fn parse_selected_mcp_ids_empty_is_explicit_none_selected() {
        // Present-but-empty → Some([]) → deliberate "select none".
        let mut obj = json!({ "selected_mcp_server_ids": [] }).as_object().unwrap().clone();
        assert_eq!(parse_selected_mcp_server_ids(&mut obj), Some(vec![]));
    }

    #[test]
    fn merge_json_top_level_overwrite() {
        let mut base = json!({"a": 1, "b": 2});
        let patch = json!({"b": 3, "c": 4});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!({"a": 1, "b": 3, "c": 4}));
    }

    #[test]
    fn merge_json_into_empty() {
        let mut base = json!({});
        let patch = json!({"x": "hello"});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!({"x": "hello"}));
    }

    #[test]
    fn merge_json_non_object_noop() {
        let mut base = json!("string");
        let patch = json!({"a": 1});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!("string"));
    }

    #[test]
    fn merge_json_empty_patch() {
        let mut base = json!({"a": 1});
        let patch = json!({});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!({"a": 1}));
    }

    #[test]
    fn knowledge_binding_target_companion_id_routes_to_companion() {
        let extra = json!({"companionId": "companion-42"});
        assert_eq!(knowledge_binding_target(&extra, "conv-1"), ("companion", "companion-42"));
    }

    #[test]
    fn knowledge_binding_target_companion_id_is_trimmed() {
        let extra = json!({"companionId": "  companion-42  "});
        assert_eq!(knowledge_binding_target(&extra, "conv-1"), ("companion", "companion-42"));
    }

    #[test]
    fn knowledge_binding_target_empty_companion_id_falls_back() {
        let extra = json!({"companionId": ""});
        assert_eq!(knowledge_binding_target(&extra, "conv-1"), ("conversation", "conv-1"));
    }

    #[test]
    fn knowledge_binding_target_blank_companion_id_falls_back() {
        let extra = json!({"companionId": "   \t "});
        assert_eq!(knowledge_binding_target(&extra, "conv-1"), ("conversation", "conv-1"));
    }

    #[test]
    fn knowledge_binding_target_missing_companion_id_falls_back() {
        let extra = json!({"workspace": "/tmp/ws"});
        assert_eq!(knowledge_binding_target(&extra, "conv-1"), ("conversation", "conv-1"));
    }

    #[test]
    fn knowledge_binding_target_non_object_extra_falls_back() {
        // build_task_options can yield a non-object extra only in degenerate
        // cases, but the helper must still not panic on them.
        let extra = serde_json::Value::Null;
        assert_eq!(knowledge_binding_target(&extra, "conv-1"), ("conversation", "conv-1"));
    }

    #[test]
    fn knowledge_binding_target_non_string_companion_id_falls_back() {
        let extra = json!({"companionId": 42});
        assert_eq!(knowledge_binding_target(&extra, "conv-1"), ("conversation", "conv-1"));
    }

    fn response_with_type(agent_type: nomifun_common::AgentType) -> ConversationResponse {
        ConversationResponse {
            id: 1,
            name: "test".into(),
            r#type: agent_type,
            model: None,
            status: ConversationStatus::Pending,
            runtime: None,
            source: None,
            pinned: false,
            pinned_at: None,
            channel_chat_id: None,
            created_at: 0,
            modified_at: 0,
            extra: json!({}),
        }
    }

    #[test]
    fn assistant_lineage_extracts_acp_builtin_fields() {
        use nomifun_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({
            "agent_id": "abc-123",
            "agent_name": "Claude Code",
            "backend": "claude",
            "current_model_id": "opus",
            "session_mode": "default",
        });
        let lineage = AssistantLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "acp");
        assert_eq!(lineage.agent_id, "abc-123");
        assert_eq!(lineage.agent_name, "Claude Code");
        assert_eq!(lineage.backend, "claude");
        assert_eq!(lineage.current_model_id, "opus");
        assert_eq!(lineage.session_mode, "default");
        assert_eq!(lineage.preset_assistant_id, "");
        assert_eq!(lineage.custom_agent_id, "");
        assert!(lineage.has_any_identity());
    }

    #[test]
    fn assistant_lineage_extracts_nomi_preset_id() {
        use nomifun_common::AgentType;
        let response = response_with_type(AgentType::Nomi);
        let extra = json!({ "preset_assistant_id": "preset-xyz" });
        let lineage = AssistantLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "nomi");
        assert_eq!(lineage.preset_assistant_id, "preset-xyz");
        assert!(lineage.has_any_identity());
    }

    #[test]
    fn assistant_lineage_extracts_acp_custom_agent_id() {
        use nomifun_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({
            "custom_agent_id": "custom-1",
            "backend": "openrouter",
        });
        let lineage = AssistantLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "acp");
        assert_eq!(lineage.custom_agent_id, "custom-1");
        assert_eq!(lineage.backend, "openrouter");
        assert!(lineage.has_any_identity());
    }

    #[test]
    fn assistant_lineage_no_identity_when_extra_lacks_assistant_fields() {
        use nomifun_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({ "workspace": "/project" });
        let lineage = AssistantLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "acp");
        assert!(!lineage.has_any_identity());
    }

    #[test]
    fn assistant_lineage_treats_non_string_fields_as_missing() {
        use nomifun_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({
            "agent_id": 42,
            "agent_name": null,
        });
        let lineage = AssistantLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_id, "");
        assert_eq!(lineage.agent_name, "");
        assert!(!lineage.has_any_identity());
    }

    #[test]
    fn classify_session_mcp_status_marks_unsupported_transport() {
        let status = classify_session_mcp_status(
            &SessionMcpServer {
                id: "mcp-http".into(),
                name: "remote-http".into(),
                transport: SessionMcpTransport::Http {
                    url: "https://example.com/mcp".into(),
                    headers: HashMap::new(),
                },
            },
            McpSupportPolicy {
                stdio: true,
                http: false,
                sse: false,
                streamable_http: false,
            },
        );

        assert_eq!(status.status, ConversationMcpStatusKind::Unsupported);
    }

    #[test]
    fn classify_session_mcp_status_marks_missing_stdio_command_failed() {
        let status = classify_session_mcp_status(
            &SessionMcpServer {
                id: "mcp-stdio".into(),
                name: "broken-stdio".into(),
                transport: SessionMcpTransport::Stdio {
                    command: "__definitely_missing_nomifun_mcp_command__".into(),
                    args: Vec::new(),
                    env: HashMap::new(),
                },
            },
            McpSupportPolicy::NOMI,
        );

        assert_eq!(status.status, ConversationMcpStatusKind::Failed);
    }
}
