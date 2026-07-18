use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

use nomifun_ai_agent::types::{AgentRuntimeBuildOptions, SendMessageData};
use nomifun_ai_agent::{AgentRuntimeHandle, AgentRuntimeRegistry, TurnStopReason};
use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;

use crate::response_middleware::ICronService;
use crate::runtime_state::{
    AgentTurnHandle, ConversationDeletionGuard, ConversationRuntimeStateService,
    InMemoryCancelAuthority, RuntimeBuildLease, TurnWireContext,
};
use crate::ExecutionConversationBoundary;
use nomifun_api_types::{
    ApprovalCheckResponse, CloneConversationRequest, ConfirmRequest, ConfirmationListResponse,
    ConversationArtifactListResponse, ConversationArtifactResponse, ConversationListResponse,
    ConversationMcpStatus, ConversationMcpStatusKind,
    ConversationResponse, ConversationRuntimeSummary, CreateConversationRequest, KnowledgeMountInfo, ListConversationsQuery,
    ListMessagesQuery, MessageListResponse, MessageResponse, MessageSearchResponse, SearchMessagesQuery,
    ExecutionModelPool, ExecutionModelRef, SendMessageRequest, SessionMcpServer, SessionMcpTransport, UpdateConversationArtifactRequest,
    UpdateConversationRequest, WebSocketMessage,
};
use nomifun_common::{
    AgentExecutionTemplateId, AgentKillReason, McpServerId, AgentType, AppError, CompanionId, ConversationArtifactId, ConversationId, ConversationSource,
    ConversationStatus, DecisionPolicy, DelegationPolicy, ErrorChain, ExecutionAuthority, MessageId, MessageType, OnConversationDelete, PaginatedResult, ProviderId, ProviderWithModel,
    CronJobId, generate_prefixed_id, now_ms, validate_prefixed_id, workspace_path_has_edge_whitespace_segment,
};
use nomifun_db::models::{ConversationRow, MessageRow};
use nomifun_db::{
    ConversationFilters, ConversationRowUpdate, CreateAcpSessionParams, IAcpSessionRepository,
    IAgentMetadataRepository, IConversationRepository,
    IMcpServerRepository, SaveRuntimeStateParams, SortOrder,
};
use nomifun_mcp::{AcpMcpCapabilities, parse_acp_mcp_capabilities};
use nomifun_realtime::UserEventSink;
use nomifun_runtime::resolve_command_path;
use std::collections::{HashMap, HashSet};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::convert::{
    TOOL_CONTENT_COMPACT_THRESHOLD_BYTES, parse_provider_with_model, row_to_artifact_response, row_to_message_response,
    row_to_message_response_compact, row_to_response, row_to_response_with_extra, search_row_to_item, string_to_enum,
};
use crate::skill_resolver::SkillResolver;
use crate::skill_snapshot::{backfill_skills_if_missing, compute_initial_skills};
use crate::stream_relay::{RelayTerminal, StreamRelay, run_turn_writeback_report};
use std::sync::RwLock;

const MAX_CRON_CONTINUATIONS_PER_TURN: usize = 4;
const TEMP_WORKSPACE_ID_EXTRA_KEY: &str = "temp_workspace_id";
const RECEIPT_FOREGROUND_BUDGET: Duration = Duration::from_secs(2);
/// Stop waits for relay/receipt cleanup, then generation-safely releases the
/// exact cancelled turn so the endpoint itself always remains bounded.
const CANCEL_RELEASE_GRACE: Duration = Duration::from_secs(3);
const CANCEL_TEARDOWN_GRACE: Duration = Duration::from_secs(7);
const CANCEL_COMPLETION_GRACE: Duration = Duration::from_secs(2);
const CANCEL_HANDLER_GRACE: Duration = Duration::from_secs(11);
const CANCEL_AUTH_PREFLIGHT_GRACE: Duration = Duration::from_secs(2);
const TURN_WRITEBACK_GRACE: Duration = Duration::from_secs(5);
/// One relay terminal may trigger session persistence, failover/image rebuild,
/// and ACP eviction. They share one deadline so several individually bounded
/// operations cannot add up to an apparently permanent Running turn.
const POST_TERMINAL_TRANSITION_GRACE: Duration = Duration::from_secs(10);
const DELETE_CORE_GRACE: Duration = Duration::from_secs(5);
const DELETE_CLEANUP_ITEM_GRACE: Duration = Duration::from_secs(5);
const DELETE_STOP_GRACE: Duration = Duration::from_secs(30);

/// Remove state that identifies or resumes the source conversation instance.
///
/// `POST /clone` means "create a new isolated conversation". Reusing a
/// workspace is a separate, explicit operation; accepting these fields here
/// would make the clone share the source directory/session and expose its
/// historical files or runtime context. This service-level boundary also
/// protects non-HTTP callers that invoke [`ConversationService::clone_create`]
/// directly.
pub(crate) fn strip_clone_instance_state(extra: &mut serde_json::Value) {
    let Some(map) = extra.as_object_mut() else {
        return;
    };

    for key in [
        // Workspace identity and response-only workspace metadata.
        "workspace",
        "custom_workspace",
        "is_temporary_workspace",
        TEMP_WORKSPACE_ID_EXTRA_KEY,
        "workspace_id",
        "workspaceId",
        // ACP/runtime resume snapshots.
        "acp_session_id",
        "acp_session_conversation_id",
        "acp_session_updated_at",
        "current_mode_id",
        "current_model_id",
        "cached_config_options",
        "pending_config_options",
        // Other engines' persisted resume/validation state.
        "sessionKey",
        "session_key",
        "runtimeValidation",
        "runtime_validation",
    ] {
        map.remove(key);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotentMessageDelivery {
    pub message_id: String,
    pub completed: bool,
    pub result_ok: Option<bool>,
    pub result_text: Option<String>,
    pub result_error: Option<String>,
}

#[derive(Debug, Clone)]
struct DurableOperationLease {
    message_id: String,
    generation: u64,
}

type DurableOperationGuards =
    Arc<std::sync::Mutex<std::collections::HashMap<String, DurableOperationLease>>>;

/// Ownership token passed from the synchronous idempotent admission path into
/// whichever path is responsible for completing the durable receipt.  The
/// explicit handoff prevents an outer `Err` handler from opening redelivery
/// while a bounded receipt retry was already detached in the background.
#[derive(Debug, Clone)]
struct DurableDeliveryLease {
    operation_id: String,
    message_id: String,
    guard_key: String,
    guard_generation: u64,
    receipt_handed_off: Arc<AtomicBool>,
}

impl DurableDeliveryLease {
    fn handoff_receipt(&self) {
        self.receipt_handed_off.store(true, Ordering::Release);
    }

    fn receipt_was_handed_off(&self) -> bool {
        self.receipt_handed_off.load(Ordering::Acquire)
    }
}

/// Abort-safe owner for a durable delivery receipt. Dropping a Tokio
/// JoinHandle does not cancel its task, but an explicit AbortHandle does; in
/// that case no async epilogue runs. This guard transfers receipt completion to
/// the existing idempotent compensation loop so a cancelled/panicked owner
/// cannot leave the operation lease pending forever.
struct OwnerReceiptGuard {
    repo: Arc<dyn IConversationRepository>,
    user_id: String,
    conversation_id: String,
    operation_id: Option<String>,
    operation_guard: Option<(DurableOperationGuards, String, u64)>,
    armed: bool,
}

impl OwnerReceiptGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OwnerReceiptGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(operation_id) = self.operation_id.clone() else {
            return;
        };
        ConversationService::continue_delivery_receipt_in_background(
            Arc::clone(&self.repo),
            self.user_id.clone(),
            self.conversation_id.clone(),
            operation_id,
            false,
            None,
            Some("Agent turn owner exited before completing its delivery receipt".to_owned()),
            self.operation_guard.clone(),
        );
    }
}

/// Validate the canonical Conversation entity ID at an input boundary.
///
/// The returned slice is the original wire/storage value. There is no numeric
/// compatibility path and invalid IDs never degrade to 0.
pub(crate) fn parse_conv_id(id: &str) -> Result<&str, nomifun_common::AppError> {
    ConversationId::try_from(id)
        .map(|_| id)
        .map_err(|_| nomifun_common::AppError::NotFound(format!("conversation {id}")))
}

fn parse_message_id(id: &str) -> Result<&str, AppError> {
    MessageId::try_from(id)
        .map(|_| id)
        .map_err(|_| AppError::NotFound(format!("message {id}")))
}

fn conversation_lead_model(model: &ProviderWithModel) -> Result<ExecutionModelRef, AppError> {
    ProviderId::try_from(model.provider_id.as_str()).map_err(|_| {
        AppError::BadRequest("Conversation model requires a canonical provider_id".to_owned())
    })?;
    if model.model.trim().is_empty() || model.model.trim() != model.model {
        return Err(AppError::BadRequest(
            "Conversation model requires a trimmed model name".to_owned(),
        ));
    }
    if model.use_model.as_deref().is_some_and(|candidate| {
        candidate.trim().is_empty() || candidate.trim() != candidate
    }) {
        return Err(AppError::BadRequest(
            "Conversation model override must be trimmed and non-empty".to_owned(),
        ));
    }
    let selected_model = model.use_model.as_deref().unwrap_or(&model.model);
    Ok(ExecutionModelRef {
        provider_id: model.provider_id.clone(),
        model: selected_model.to_owned(),
    })
}

fn validate_conversation_model_authority(
    model: Option<&ProviderWithModel>,
    pool: Option<&ExecutionModelPool>,
) -> Result<(), AppError> {
    let lead = model.map(conversation_lead_model).transpose()?;
    if let Some(pool) = pool {
        pool.validate().map_err(AppError::BadRequest)?;
        if let Some(lead) = lead.as_ref()
            && !pool.contains(lead)
        {
            return Err(AppError::BadRequest(format!(
                "Conversation lead model {}/{} must belong to execution_model_pool",
                lead.provider_id, lead.model
            )));
        }
    }
    Ok(())
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
/// per turn after the Agent runtime exists; the implementation resolves config
/// internally and is a cheap no-op when IDMM is disabled or already supervising.
///
/// Mirrors AutoWork's `IdmmHandle::ensure_supervising` (which arms per
/// polling iteration) for the plain, user-driven desktop chat path —
/// the only path that otherwise never armed IDMM (no AutoWork loop, no
/// boot-resume), so an enabled 智能决策 silently never observed the turn.
pub trait ConversationSupervisionHook: Send + Sync {
    /// Ensure IDMM supervision is (idempotently) running for this conversation.
    fn on_turn_start(&self, conversation_id: &str);
}

#[derive(Clone)]
pub struct ConversationService {
    /// Immutable installation owner used to derive the maximum runtime
    /// authority for every persisted Conversation owner.  This keeps host
    /// capability decisions inside the service and out of open `extra` JSON.
    authoritative_user_id: Arc<str>,
    workspace_root: PathBuf,
    user_events: Arc<dyn UserEventSink>,
    skill_resolver: Arc<dyn SkillResolver>,
    runtime_registry: Arc<dyn AgentRuntimeRegistry>,
    /// Hooks invoked at the end of `delete()` so other services
    /// (`InMemoryAgentRuntimeRegistry`, `CronService`, …) can clean up their
    /// per-conversation state. Wrapped in `Arc<RwLock<…>>` so registration
    /// can happen post-construction without breaking the `Clone` impl —
    /// mirrors the `cron_service` slot pattern below.
    delete_hooks: Arc<RwLock<Vec<Arc<dyn OnConversationDelete>>>>,
    cron_service: Arc<RwLock<Option<Arc<dyn ICronService>>>>,
    mcp_server_repo: Arc<RwLock<Option<Arc<dyn IMcpServerRepository>>>>,
    /// Knowledge base service slot (same post-construction registration
    /// pattern as `cron_service`). When wired, bound knowledge bases are
    /// mounted into the workspace when its Agent runtime is created and surfaced to the agent
    /// via `extra.knowledge_mounts` / `extra.knowledge_writeback`.
    knowledge_service: Arc<RwLock<Option<Arc<nomifun_knowledge::KnowledgeService>>>>,
    /// Unified preset resolver. When a create request carries `preset_id`, the
    /// server resolves and freezes the preset before any model/skill/knowledge
    /// shaping runs; clients cannot inject a forged snapshot.
    preset_service: Arc<RwLock<Option<Arc<nomifun_preset::PresetService>>>>,
    runtime_state: Arc<ConversationRuntimeStateService>,
    /// Per-conversation timestamp (ms) of the most recent USER-initiated
    /// cancel (`POST /api/conversations/{id}/cancel`). The AutoWork runner
    /// consults this after a turn ends (`user_cancelled_since`) to tell "the
    /// user deliberately stopped this work" apart from a turn failure —
    /// engine stream events alone can't carry that intent reliably across
    /// every agent type. In-memory only; bounded by the number of
    /// conversations a user ever cancels in one process lifetime.
    user_cancel_stamps: Arc<std::sync::Mutex<std::collections::HashMap<String, i64>>>,
    /// Process-local ownership for pending durable turn receipts. Interactive
    /// cancellation may release the UI turn while receipt compensation retries;
    /// this guard prevents a concurrent redelivery from invoking the model/tool
    /// side effects again in the same live process.
    durable_operations_in_flight: DurableOperationGuards,
    next_durable_operation_generation: Arc<AtomicU64>,

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
    /// Mandatory read-side for the explicit Conversation↔Execution relation.
    /// Production assembly shares one repository-backed instance across every
    /// ConversationService; isolated tests must opt into the explicit no-op
    /// implementation instead of silently omitting this authority.
    execution_conversation_boundary: Arc<dyn ExecutionConversationBoundary>,
}

// ── Construction & Dependency Injection ──────────────────────────────

impl ConversationService {
    async fn try_complete_delivery_receipt(
        repo: &Arc<dyn IConversationRepository>,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        result_ok: bool,
        result_text: Option<&str>,
        result_error: Option<&str>,
    ) -> bool {
        match repo
            .complete_delivery_receipt(
                user_id,
                conversation_id,
                operation_id,
                result_ok,
                result_text,
                result_error,
                now_ms(),
            )
            .await
        {
            Ok(true) => true,
            Ok(false) => {
                if matches!(repo.get(conversation_id).await, Ok(None)) {
                    // The only legal receipt deletion is an owning account or
                    // Conversation cascade. With no addressable receiver,
                    // replay is impossible and compensation is unnecessary.
                    return true;
                }
                error!(
                    conversation_id,
                    operation_id,
                    "Durable Conversation delivery receipt was not acknowledged; retrying"
                );
                false
            }
            Err(receipt_error) => {
                error!(
                    conversation_id,
                    operation_id,
                    error = %ErrorChain(&receipt_error),
                    "Failed to persist durable Conversation delivery receipt; retrying"
                );
                false
            }
        }
    }

    fn durable_operation_key(user_id: &str, conversation_id: &str, operation_id: &str) -> String {
        format!("{user_id}\0{conversation_id}\0{operation_id}")
    }

    fn release_durable_operation_guard(
        guard: &DurableOperationGuards,
        key: &str,
        generation: u64,
    ) {
        if let Ok(mut operations) = guard.lock() {
            if operations
                .get(key)
                .is_some_and(|lease| lease.generation == generation)
            {
                operations.remove(key);
            }
        }
    }

    fn continue_delivery_receipt_in_background(
        repo: Arc<dyn IConversationRepository>,
        user_id: String,
        conversation_id: String,
        operation_id: String,
        result_ok: bool,
        result_text: Option<String>,
        result_error: Option<String>,
        operation_guard: Option<(
            DurableOperationGuards,
            String,
            u64,
        )>,
    ) {
        tokio::spawn(async move {
            let mut retry_delay_ms = 100_u64;
            loop {
                if Self::try_complete_delivery_receipt(
                    &repo,
                    &user_id,
                    &conversation_id,
                    &operation_id,
                    result_ok,
                    result_text.as_deref(),
                    result_error.as_deref(),
                )
                .await
                {
                    if let Some((guard, key, generation)) = operation_guard.as_ref() {
                        Self::release_durable_operation_guard(guard, key, *generation);
                    }
                    info!(conversation_id, operation_id, "Durable delivery receipt compensation completed");
                    return;
                }
                tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
                retry_delay_ms = (retry_delay_ms * 2).min(2_000);
            }
        });
    }

    /// Give the receipt a bounded foreground window, then release the
    /// interactive turn while a process-local operation guard prevents a
    /// concurrent redelivery from repeating side effects during compensation.
    async fn complete_delivery_receipt_before_release(
        repo: &Arc<dyn IConversationRepository>,
        user_id: &str,
        conversation_id: &str,
        operation_id: Option<&str>,
        result_ok: bool,
        result_text: Option<&str>,
        result_error: Option<&str>,
        cancellation: &CancellationToken,
        operation_guard: Option<(
            &DurableOperationGuards,
            &str,
            u64,
        )>,
    ) {
        let Some(operation_id) = operation_id else {
            return;
        };
        let deadline = tokio::time::Instant::now() + RECEIPT_FOREGROUND_BUDGET;
        let mut retry_delay_ms = 25_u64;
        loop {
            if cancellation.is_cancelled() || tokio::time::Instant::now() >= deadline {
                Self::continue_delivery_receipt_in_background(
                    Arc::clone(repo),
                    user_id.to_owned(),
                    conversation_id.to_owned(),
                    operation_id.to_owned(),
                    result_ok,
                    result_text.map(str::to_owned),
                    result_error.map(str::to_owned),
                    operation_guard
                        .map(|(guard, key, generation)| (Arc::clone(guard), key.to_owned(), generation)),
                );
                return;
            }

            let attempt = Self::try_complete_delivery_receipt(
                repo,
                user_id,
                conversation_id,
                operation_id,
                result_ok,
                result_text,
                result_error,
            );
            let completed = tokio::select! {
                biased;
                _ = cancellation.cancelled() => None,
                _ = tokio::time::sleep_until(deadline) => None,
                result = attempt => Some(result),
            };
            match completed {
                Some(true) => {
                    if let Some((guard, key, generation)) = operation_guard {
                        Self::release_durable_operation_guard(guard, key, generation);
                    }
                    return;
                }
                Some(false) => {}
                None => {
                    Self::continue_delivery_receipt_in_background(
                        Arc::clone(repo),
                        user_id.to_owned(),
                        conversation_id.to_owned(),
                        operation_id.to_owned(),
                        result_ok,
                        result_text.map(str::to_owned),
                        result_error.map(str::to_owned),
                        operation_guard
                            .map(|(guard, key, generation)| (Arc::clone(guard), key.to_owned(), generation)),
                    );
                    return;
                }
            }

            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    Self::continue_delivery_receipt_in_background(
                        Arc::clone(repo),
                        user_id.to_owned(),
                        conversation_id.to_owned(),
                        operation_id.to_owned(),
                        result_ok,
                        result_text.map(str::to_owned),
                        result_error.map(str::to_owned),
                        operation_guard
                            .map(|(guard, key, generation)| (Arc::clone(guard), key.to_owned(), generation)),
                    );
                    return;
                }
                _ = tokio::time::sleep(
                    Duration::from_millis(retry_delay_ms)
                        .min(deadline.saturating_duration_since(tokio::time::Instant::now()))
                ) => {}
            }
            retry_delay_ms = (retry_delay_ms * 2).min(2_000);
        }
    }

    pub fn new(
        authoritative_user_id: Arc<str>,
        workspace_root: PathBuf,
        user_events: Arc<dyn UserEventSink>,
        skill_resolver: Arc<dyn SkillResolver>,
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,

        conversation_repo: Arc<dyn IConversationRepository>,
        agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
        acp_session_repo: Arc<dyn IAcpSessionRepository>,
        execution_conversation_boundary: Arc<dyn ExecutionConversationBoundary>,
    ) -> Self {
        Self {
            authoritative_user_id,
            workspace_root,
            user_events,
            skill_resolver,
            runtime_registry,
            delete_hooks: Arc::new(RwLock::new(Vec::new())),
            cron_service: Arc::new(RwLock::new(None)),
            mcp_server_repo: Arc::new(RwLock::new(None)),
            knowledge_service: Arc::new(RwLock::new(None)),
            preset_service: Arc::new(RwLock::new(None)),
            runtime_state: Arc::new(ConversationRuntimeStateService::default()),
            user_cancel_stamps: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            durable_operations_in_flight: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            next_durable_operation_generation: Arc::new(AtomicU64::new(0)),

            conversation_repo,
            agent_metadata_repo,
            acp_session_repo,
            supervision_hook: Arc::new(RwLock::new(None)),
            failover_provider_repo: Arc::new(RwLock::new(None)),
            failover_client_prefs: Arc::new(RwLock::new(None)),
            execution_conversation_boundary,
        }
    }

    fn execution_authority(&self, user_id: &str) -> ExecutionAuthority {
        ExecutionAuthority::resolve(user_id, self.authoritative_user_id.as_ref())
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

    pub fn with_preset_service(&self, service: Arc<nomifun_preset::PresetService>) {
        if let Ok(mut guard) = self.preset_service.write() {
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
    /// `delete()`. Used by `nomifun-app` to wire up `InMemoryAgentRuntimeRegistry`
    /// (terminate the Agent process) and `CronService` (cascade-delete cron jobs).
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
    /// not wired with the runtime state. An execution attempt consumes this
    /// exactly once after its Agent turn settles; removing the entry keeps the
    /// map bounded and prevents reuse by a later attempt.
    pub fn take_turn_tokens(&self, conversation_id: &str) -> Option<i64> {
        self.runtime_state.take_turn_tokens(conversation_id)
    }

    async fn project_execution_relation(
        &self,
        user_id: &str,
        response: &mut ConversationResponse,
    ) -> Result<(), AppError> {
        let projection = self
            .execution_conversation_boundary
            .projection(user_id, &response.id)
            .await?;
        response.linked_execution_id = projection.linked_execution_id;
        response.execution_step_id = projection.execution_step_id;
        response.execution_attempt_id = projection.execution_attempt_id;
        Ok(())
    }

    async fn is_active_execution_attempt_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<bool, AppError> {
        self.execution_conversation_boundary
            .is_active_attempt(user_id, conversation_id)
            .await
    }

    async fn is_execution_attempt_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<bool, AppError> {
        self.execution_conversation_boundary
            .is_retained_attempt(user_id, conversation_id)
            .await
    }

    async fn ensure_not_retained_execution_attempt(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<(), AppError> {
        if self
            .is_execution_attempt_conversation(user_id, conversation_id)
            .await?
        {
            return Err(AppError::Conflict(
                "Execution Attempt Conversations are owned audit transcripts; use Agent Execution decision/control APIs"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Shared preflight for public/background initiators that would mutate a
    /// Conversation or its runtime outside the normal service methods. Call it
    /// before building an Agent, staging files, clearing context or acquiring a
    /// turn. Agent Execution uses its separate infrastructure port instead.
    pub async fn ensure_public_mutation_allowed(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<(), AppError> {
        let conversation_id = parse_conv_id(conversation_id)?;
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|conversation| conversation.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        self.ensure_not_retained_execution_attempt(user_id, conversation_id)
            .await
    }

    pub(crate) fn runtime_handle(&self, conversation_id: &str) -> Result<AgentRuntimeHandle, AppError> {
        self.runtime_registry
            .get_runtime(conversation_id)
            .ok_or_else(|| AppError::NotFound(format!("No active agent for conversation '{conversation_id}'")))
    }

    pub async fn runtime_summary_for(&self, conversation_id: &str) -> ConversationRuntimeSummary {
        let agent = self.runtime_registry.get_runtime(conversation_id);
        let has_runtime = agent.is_some();
        let runtime_status = agent.as_ref().and_then(|agent| agent.status());
        let pending_confirmations = agent.as_ref().map(|agent| agent.get_confirmations().len()).unwrap_or(0);

        self.runtime_state
            .summary_from_parts(conversation_id, runtime_status, has_runtime, pending_confirmations)
    }

    pub fn begin_runtime_build(&self, conversation_id: &str) -> Result<RuntimeBuildLease, AppError> {
        self.runtime_state.begin_runtime_build(conversation_id)
    }

    pub fn begin_public_runtime_build(
        &self,
        conversation_id: &str,
        requester_user_id: &str,
    ) -> Result<RuntimeBuildLease, AppError> {
        self.runtime_state.begin_runtime_build_for_requester(
            conversation_id,
            Some(requester_user_id.to_owned()),
            true,
        )
    }

    pub async fn complete_turn_with_companion_context(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
        companion: bool,
        companion_id: Option<CompanionId>,
        origin: Option<String>,
        channel_platform: Option<String>,
    ) {
        // Completion may deliberately hold an admission-only completion fence.
        // GET must remain busy until the event is published, while the event
        // itself must carry the post-fence authoritative idle state so clients
        // can release their queue immediately after receiving it.
        let runtime = self.final_completion_runtime(conversation_id);
        StreamRelay::complete_conversation_with_context(
            &self.conversation_repo,
            &self.user_events,
            user_id,
            conversation_id,
            Some(turn_id.to_owned()),
            Some(runtime),
            companion,
            companion_id,
            origin,
            channel_platform,
        )
        .await;
    }

    fn final_completion_runtime(&self, conversation_id: &str) -> ConversationRuntimeSummary {
        let agent = self.runtime_registry.get_runtime(conversation_id);
        ConversationRuntimeSummary {
            state: nomifun_api_types::ConversationRuntimeStateKind::Idle,
            can_send_message: true,
            has_runtime: agent.is_some(),
            runtime_status: agent.as_ref().and_then(|agent| agent.status()),
            is_processing: false,
            pending_confirmations: 0,
            processing_started_at: None,
        }
    }

    async fn release_and_complete_turn(
        &self,
        turn_handle: &mut AgentTurnHandle,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
        companion: bool,
        companion_id: Option<CompanionId>,
        origin: Option<String>,
        channel_platform: Option<String>,
    ) {
        // Keep admission fenced across exact release -> persisted finished ->
        // turn.completed. Without this narrow fence a replacement turn could
        // start in between and be overwritten by the old turn's completion.
        let completion_fence = self
            .runtime_state
            .begin_turn_completion(conversation_id, turn_handle.turn_id());
        let completion_fence = match completion_fence {
            Ok(Some(guard)) => Some(guard),
            Ok(None) => {
                // A stop/delete won the shared admission ordering. Release is
                // allowed to signal owner quiescence, but that worker owns the
                // only terminal completion from here.
                let _ = turn_handle.release();
                return;
            }
            Err(error) => {
                warn!(
                    conversation_id,
                    error = %ErrorChain(&error),
                    "Failed to acquire completion admission fence"
                );
                None
            }
        };
        if !turn_handle.release() {
            return;
        }
        StreamRelay::persist_conversation_finished(&self.conversation_repo, conversation_id).await;
        let allowed_completion_owners = usize::from(completion_fence.is_some());
        let user_events = Arc::clone(&self.user_events);
        let runtime = self.final_completion_runtime(conversation_id);
        let completion_published = self
            .runtime_state
            .linearize_cleanup_event(
                conversation_id,
                0,
                allowed_completion_owners,
                CANCEL_TEARDOWN_GRACE,
                move || {
                    StreamRelay::broadcast_turn_completed_with_context(
                        &user_events,
                        user_id,
                        conversation_id,
                        Some(turn_id.to_owned()),
                        Some(runtime),
                        companion,
                        companion_id,
                        origin,
                        channel_platform,
                    );
                    drop(completion_fence);
                },
            )
            .await;
        if !completion_published {
            warn!(
                conversation_id,
                turn_id,
                "Completion event withheld because another cleanup fence remained active"
            );
        }
    }

    async fn broadcast_turn_started_with_context(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
        companion: bool,
        companion_id: Option<CompanionId>,
        origin: Option<String>,
        channel_platform: Option<String>,
    ) {
        let runtime = self.runtime_summary_for(conversation_id).await;
        let payload = serde_json::json!({
            "conversation_id": conversation_id,
            "turn_id": turn_id,
            "status": "running",
            "phase": "starting",
            "state": "initializing",
            "can_send_message": runtime.can_send_message,
            "runtime": runtime,
            "companion": companion,
            "companion_id": companion_id,
            "origin": origin,
            "channel_platform": channel_platform,
        });
        self.user_events
            .send_to_user(user_id, WebSocketMessage::new("turn.started", payload));
    }
}

// ── Conversation CRUD ───────────────────────────────────────────────

impl ConversationService {
    /// Create a new conversation.
    ///
    /// Generates a `conv_{uuidv7}` ID, sets status to `pending`, defaults
    /// source to `nomifun`, and broadcasts `conversation.listChanged(created)`.
    pub async fn create(
        &self,
        user_id: &str,
        req: CreateConversationRequest,
    ) -> Result<ConversationResponse, AppError> {
        self.create_inner(user_id, req, None, None).await
    }

    /// Trusted in-process create with a durable operation identity.  This is
    /// intentionally separate from `CreateConversationRequest`: public callers
    /// retain server-generated semantics and cannot choose an idempotency key.
    pub async fn create_idempotent(
        &self,
        user_id: &str,
        req: CreateConversationRequest,
        creation_key: &str,
    ) -> Result<ConversationResponse, AppError> {
        self.create_inner(user_id, req, None, Some(creation_key)).await
    }

    /// Trusted in-process creation path for long-lived consumers that already
    /// hold a frozen snapshot (cron, delegated Agent attempts, companions). This
    /// never re-resolves the catalog, so an existing target cannot drift to a
    /// newer preset revision. It is intentionally not exposed by HTTP DTOs.
    pub async fn create_from_preset_snapshot(
        &self,
        user_id: &str,
        mut req: CreateConversationRequest,
        snapshot: nomifun_api_types::ResolvedPresetSnapshot,
    ) -> Result<ConversationResponse, AppError> {
        req.preset_id = None;
        req.preset_overrides = None;
        self.create_inner(user_id, req, Some(snapshot), None).await
    }

    /// Snapshot-preserving counterpart of [`Self::create_idempotent`].
    pub async fn create_from_preset_snapshot_idempotent(
        &self,
        user_id: &str,
        mut req: CreateConversationRequest,
        snapshot: nomifun_api_types::ResolvedPresetSnapshot,
        creation_key: &str,
    ) -> Result<ConversationResponse, AppError> {
        req.preset_id = None;
        req.preset_overrides = None;
        self.create_inner(user_id, req, Some(snapshot), Some(creation_key))
            .await
    }

    /// Remove a creation-keyed conversation that never acquired its owning
    /// durable relation.  The normal execution-attempt deletion guard remains
    /// authoritative: if the link transaction actually committed but its
    /// acknowledgement was lost, this returns Conflict and preserves history.
    pub async fn discard_unlinked_creation(
        &self,
        user_id: &str,
        creation_key: &str,
    ) -> Result<(), AppError> {
        let Some(conversation) = self
            .conversation_repo
            .find_by_creation_key(user_id, creation_key)
            .await?
        else {
            return Ok(());
        };
        self.delete(user_id, &conversation.id.to_string()).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, agent_type = ?req.r#type))]
    async fn create_inner(
        &self,
        user_id: &str,
        mut req: CreateConversationRequest,
        trusted_snapshot: Option<nomifun_api_types::ResolvedPresetSnapshot>,
        creation_key: Option<&str>,
    ) -> Result<ConversationResponse, AppError> {
        let authority = self.execution_authority(user_id);
        if !authority.controls_host() {
            if req.r#type != AgentType::Nomi {
                return Err(AppError::Forbidden(format!(
                    "Agent type '{}' requires the installation owner; non-owner conversations are model-only",
                    req.r#type.serde_name()
                )));
            }

            // Open Conversation JSON is a presentation/config bag, never a
            // capability grant.  A model-only principal gets a backend-owned
            // temporary workspace and no preset, skill, MCP, channel,
            // collaboration or custom-path authority regardless of payload.
            req.extra = serde_json::json!({});
            req.preset_id = None;
            req.preset_overrides = None;
            req.delegation_policy = DelegationPolicy::Disabled;
            req.execution_model_pool = None;
            req.decision_policy = DecisionPolicy::default();
            req.execution_template_id = None;
            req.channel_chat_id = None;
        }

        let now = now_ms();
        if let Some(template_id) = req.execution_template_id.as_deref() {
            AgentExecutionTemplateId::try_from(template_id).map_err(|error| {
                AppError::BadRequest(format!("invalid execution_template_id: {error}"))
            })?;
        }
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
        reject_execution_policy_extra_keys(&extra)?;
        let preset_id = req.preset_id.take().map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
        let preset_overrides = req.preset_overrides.take().unwrap_or_default();
        let mut resolved_preset_snapshot = authority
            .controls_host()
            .then_some(trusted_snapshot)
            .flatten();
        // Snapshot/lineage are server-owned first-class columns. Never trust a
        // similarly named value hidden in the open-ended `extra` bag.
        if let Some(object) = extra.as_object_mut() {
            object.remove("preset_id");
            object.remove("preset_overrides");
            object.remove("preset_snapshot");
            object.remove("preset_revision");
        }

        // A preset id is the only client-supplied reference. Resolution is
        // backend-authoritative and produces an immutable execution snapshot;
        // any incoming `preset_snapshot` is discarded before resolving.
        if resolved_preset_snapshot.is_none() && let Some(preset_id) = preset_id {
            let service = self
                .preset_service
                .read()
                .ok()
                .and_then(|guard| guard.as_ref().cloned())
                .ok_or_else(|| AppError::Internal("preset service is not wired".into()))?;
            resolved_preset_snapshot = Some(service
                .resolve(
                    &preset_id,
                    nomifun_api_types::PresetTarget::Conversation,
                    None,
                    preset_overrides,
                )
                .await?);
        }

        if let Some(snapshot) = resolved_preset_snapshot.as_ref() {
            if let Some(agent_id) = snapshot.resolved_agent_id.as_ref()
                && let Some(agent) = self.agent_metadata_repo.get(agent_id).await?
            {
                req.r#type = serde_json::from_value(serde_json::Value::String(agent.agent_type.clone()))
                    .map_err(|_| AppError::BadRequest(format!("preset resolved unknown agent type '{}'", agent.agent_type)))?;
                if let Some(obj) = extra.as_object_mut() {
                    obj.insert("agent_id".into(), serde_json::Value::String(agent.id));
                    if let Some(backend) = agent.backend {
                        obj.insert("backend".into(), serde_json::Value::String(backend));
                    }
                }
            }
            if let Some(model) = snapshot.resolved_model.as_ref() {
                if req.r#type == AgentType::Nomi {
                    let provider_id = model.provider_id.as_ref().ok_or_else(|| {
                        AppError::BadRequest(format!(
                            "preset model '{}' has no resolved provider",
                            model.model
                        ))
                    })?;
                    req.model = Some(nomifun_common::ProviderWithModel {
                        provider_id: provider_id.clone(),
                        model: model.model.clone(),
                        use_model: Some(model.model.clone()),
                    });
                } else if let Some(obj) = extra.as_object_mut() {
                    // ACP executors own their provider connection; the preset
                    // contributes only the agent-visible model id.
                    obj.insert(
                        "current_model_id".into(),
                        serde_json::Value::String(model.model.clone()),
                    );
                    req.model = None;
                }
            }
            if let Some(obj) = extra.as_object_mut() {
                let instructions_embedded = obj
                    .get("preset_instructions_embedded")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if !instructions_embedded && !snapshot.instructions.trim().is_empty() {
                    obj.insert("preset_rules".into(), serde_json::Value::String(snapshot.instructions.clone()));
                }
                obj.insert("preset_enabled_skills".into(), serde_json::to_value(&snapshot.included_skills).unwrap_or_default());
                obj.insert("exclude_auto_inject_skills".into(), serde_json::to_value(&snapshot.excluded_auto_skills).unwrap_or_default());
                obj.insert("preset_knowledge_binding".into(), serde_json::Value::Bool(true));
            }
        }

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
        // auto-provision one under `{data_dir}/conversations/{label}-temp-{token}/`.
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

        // Auto-provisioned workspace directories use a separate durable token,
        // keeping filesystem instance identity independent from the public
        // conversation ID. Directory creation and the `extra.workspace` write
        // remain deferred until after the row exists.
        let auto_workspace = if user_supplied_workspace.is_none() {
            let label = conversation_label(&req.r#type, extra.get("backend"));
            let (temp_workspace_id, probe_path) = allocate_temp_workspace_id(&self.workspace_root, &label);
            if workspace_path_has_edge_whitespace_segment(&probe_path) {
                return Err(AppError::WorkspacePathEdgeWhitespace(probe_path.display().to_string()));
            }
            Some((label, temp_workspace_id))
        } else {
            None
        };

        // Strip request-only / derived workspace flags and then stamp the
        // backend-owned temp workspace token for auto-provisioned sessions.
        if let Some(obj) = extra.as_object_mut() {
            obj.remove("custom_workspace");
            obj.remove("is_temporary_workspace");
            obj.remove(TEMP_WORKSPACE_ID_EXTRA_KEY);
            if let Some((_, temp_workspace_id)) = auto_workspace.as_ref() {
                obj.insert(
                    TEMP_WORKSPACE_ID_EXTRA_KEY.to_owned(),
                    serde_json::Value::String(temp_workspace_id.clone()),
                );
            }
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

        let auto_inject_names = if authority.controls_host() {
            self.skill_resolver.auto_inject_names().await
        } else {
            Vec::new()
        };
        let initial_skills = compute_initial_skills(&auto_inject_names, &preset_enabled, &exclude_auto_inject);

        // Skill symlinks are wired into the auto-provisioned workspace *after*
        // the row is created, when the tokenized workspace path is materialized.
        // Capture the inputs now (the `skills` snapshot below consumes
        // `initial_skills` into `extra`).
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
        let selected_mcp_server_ids: Option<Vec<McpServerId>> = match extra.as_object_mut() {
            Some(obj) => parse_selected_mcp_server_ids(obj)?,
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
        let mut selected_row_ids: Vec<McpServerId> = Vec::new();
        let mut selected_mcp_names: Vec<String> = Vec::new();
        let mut selected_mcp_statuses: Vec<ConversationMcpStatus> = Vec::new();
        let mut seen_mcp_names = HashSet::new();
        let mut status_index_by_name: HashMap<String, usize> = HashMap::new();
        let repo = self
            .mcp_server_repo
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned());
        if authority.controls_host() && let Some(repo) = repo {
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
            selected_row_ids = selected_rows.iter().map(|row| row.id.clone()).collect();
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
            // reads `extra.mcp_server_ids` as canonical `McpServerId` strings.
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
        let preset_snapshot_value = resolved_preset_snapshot
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| AppError::Internal(format!("Failed to serialize preset snapshot: {e}")))?;
        let preset_snapshot = preset_snapshot_value
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| AppError::BadRequest(format!("Invalid preset_snapshot: {e}")))?;
        let preset_id = preset_snapshot_value
            .as_ref()
            .and_then(|value| value.get("preset_id"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| extra.get("preset_id").and_then(serde_json::Value::as_str))
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned);
        let preset_revision = preset_snapshot_value
            .as_ref()
            .and_then(|value| value.get("preset_revision"))
            .and_then(serde_json::Value::as_i64)
            .or_else(|| extra.get("preset_revision").and_then(serde_json::Value::as_i64));

        if let Some(pool) = req.execution_model_pool.as_ref() {
            pool.validate().map_err(AppError::BadRequest)?;
        }
        validate_conversation_model_authority(
            req.model.as_ref(),
            req.execution_model_pool.as_ref(),
        )?;

        let row = nomifun_db::models::ConversationRow {
            id: ConversationId::new().into_string(),
            user_id: user_id.to_owned(),
            name: req.name.unwrap_or_default(),
            r#type: enum_to_db(&req.r#type)?,
            extra: serde_json::to_string(&extra)
                .map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?,
            delegation_policy: req.delegation_policy.as_str().to_owned(),
            execution_model_pool: req
                .execution_model_pool
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| AppError::Internal(format!("Failed to serialize execution model pool: {e}")))?,
            decision_policy: req.decision_policy.as_str().to_owned(),
            execution_template_id: req.execution_template_id,
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
            preset_id,
            preset_revision,
            preset_snapshot,
            created_at: now,
            updated_at: now,
        };

        let (new_id, created_now) = match creation_key {
            Some(key) => self.conversation_repo.create_idempotent(&row, key).await?,
            None => (self.conversation_repo.create(&row).await?, true),
        };
        if !created_now {
            let existing = self
                .conversation_repo
                .get(&new_id)
                .await?
                .filter(|existing| existing.user_id == user_id)
                .ok_or_else(|| {
                    AppError::Conflict(
                        "conversation creation key resolved outside its owner boundary".to_owned(),
                    )
                })?;
            let mut existing = existing;
            rebase_managed_workspace_in_row(&mut existing, &self.workspace_root)?;
            let mut response = row_to_response(existing, &self.workspace_root)?;
            self.project_execution_relation(user_id, &mut response).await?;
            return Ok(response);
        }

        // Materialize the preset's knowledge policy as an explicit target
        // binding. This deliberately bypasses workpath inheritance at runtime:
        // selecting a preset must reproduce its KB scope without mutating or
        // silently sharing the user's general workspace binding.
        if let Some(snapshot_value) = preset_snapshot_value.as_ref()
            && let Ok(snapshot) = serde_json::from_value::<nomifun_api_types::ResolvedPresetSnapshot>(
                snapshot_value.clone(),
            )
            && let Some(service) = self
                .knowledge_service
                .read()
                .ok()
                .and_then(|guard| guard.as_ref().cloned())
        {
            let (target_kind, target_id) = knowledge_binding_target(&extra, &new_id)?;
            let mode = match snapshot.knowledge_policy.mode.as_str() {
                "direct" => "direct",
                _ => "staged",
            };
            service
                .set_binding(
                    target_kind,
                    target_id,
                    nomifun_knowledge::KnowledgeBinding {
                        enabled: snapshot.knowledge_policy.enabled,
                        writeback: snapshot.knowledge_policy.writeback,
                        writeback_mode: mode.to_owned(),
                        writeback_eagerness: snapshot
                            .knowledge_policy
                            .eagerness
                            .unwrap_or_else(|| "conservative".to_owned()),
                        // Presets never self-authorize unattended channel writes.
                        channel_write_enabled: false,
                        kb_ids: snapshot.knowledge_base_ids,
                    },
                )
                .await?;
        }

        // Now that the row exists, provision the auto (temp) workspace: create
        // the tokenized directory, wire skill symlinks (temp workspaces only),
        // record the path in `extra`, and persist the updated `extra` back.
        // User-supplied workspaces are left untouched (already in `extra` and
        // validated above).
        if let Some((label, temp_workspace_id)) = auto_workspace.as_ref() {
            let ws_path = auto_temp_workspace_path(&self.workspace_root, label, temp_workspace_id);
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
            self.conversation_repo.update(&new_id, &workspace_update).await?;
        }

        // Persist the MCP selection into the `conversation_mcp_servers` junction
        // now that the parent row exists (the junction FK requires it). The
        // build-extra `mcp_server_ids` snapshot above already feeds the agent
        // factory; this write is the durable selection store that replaces the
        // retired `extra.selected_mcp_server_ids` array.
        if selected_mcp_server_ids.is_some()
            && let Err(e) = self.conversation_repo.set_mcp_server_ids(&new_id, &selected_row_ids).await
        {
            warn!(error = %ErrorChain(&e), conversation_id = new_id, "failed to persist MCP server selection");
        }
        // ACP conversations own one `acp_session` row (1:1 by
        // conversation_id). Other agent types have no session-level
        // state so we only create it for ACP.
        if req.r#type == AgentType::Acp {
            self.create_acp_session_row(&new_id, &extra).await?;
        }

        // Build the response from a row carrying the real id and the final
        // `extra` (with the resolved workspace, if any).
        let response_row = nomifun_db::models::ConversationRow {
            id: new_id.clone(),
            extra: serde_json::to_string(&extra)
                .map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?,
            ..row
        };
        let mut response = row_to_response(response_row, &self.workspace_root)?;
        self.project_execution_relation(user_id, &mut response).await?;

        self.broadcast_list_changed(user_id, &new_id, "created", response.source.as_ref());

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
        let mut row = self
            .conversation_repo
            .get(parse_conv_id(id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;
        rebase_managed_workspace_in_row(&mut row, &self.workspace_root)?;

        let mut extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|e| AppError::Internal(format!("Invalid extra JSON: {e}")))?;
        self.backfill_extra_inplace(&row.id, &mut extra).await;
        let mut response = row_to_response_with_extra(row, extra, &self.workspace_root)?;
        response.runtime = Some(self.runtime_summary_for(id).await);
        self.project_execution_relation(user_id, &mut response).await?;
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
        if let Some(cron_job_id) = query.cron_job_id.as_deref() {
            CronJobId::try_from(cron_job_id).map_err(|error| {
                AppError::BadRequest(format!("invalid cron_job_id: {error}"))
            })?;
        }
        let filters = ConversationFilters {
            // The cursor arrives as a query-string parameter and remains the
            // canonical conversation ID used by keyset pagination. Malformed
            // cursors fail closed at this boundary.
            cursor: query
                .cursor
                .map(|cursor| {
                    parse_conv_id(&cursor)?;
                    Ok::<_, AppError>(cursor)
                })
                .transpose()?,
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
        for mut row in result.items {
            rebase_managed_workspace_in_row(&mut row, &self.workspace_root)?;
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
            self.backfill_extra_inplace(&row_id, &mut extra).await;
            match row_to_response_with_extra(row, extra, &self.workspace_root) {
                Ok(mut response) => {
                    self.project_execution_relation(user_id, &mut response).await?;
                    items.push(response);
                }
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
        mut req: UpdateConversationRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<ConversationResponse, AppError> {
        let existing = self
            .conversation_repo
            .get(parse_conv_id(id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;

        let authority = self.execution_authority(user_id);
        if !authority.controls_host() {
            if string_to_enum::<AgentType>(&existing.r#type)? != AgentType::Nomi {
                return Err(AppError::Forbidden(
                    "Non-owner conversations are model-only; this legacy runtime cannot be resumed"
                        .into(),
                ));
            }
            // Only ordinary presentation/model fields are mutable.  Runtime
            // grants, paths and collaboration policy are backend-owned.
            req.extra = None;
            req.delegation_policy = Some(DelegationPolicy::Disabled);
            req.execution_model_pool = Some(None);
            req.decision_policy = Some(DecisionPolicy::default());
            req.execution_template_id = Some(None);
        }

        // Public PATCH cannot mutate or recycle the runtime snapshot owned by
        // an Execution Attempt. Backend-only metadata seams such as
        // `update_extra` remain separate and intentionally bypass this API.
        self.ensure_not_retained_execution_attempt(user_id, &existing.id)
            .await?;

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
        if let Some(incoming) = &req.extra {
            reject_execution_policy_extra_keys(incoming)?;
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
        // effect on the next message (same rationale as the model-change termination
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
        let preset_changed = req
            .extra
            .as_ref()
            .is_some_and(|extra| extra.get("preset_snapshot").is_some())
            && merged_extra.as_deref() != Some(existing.extra.as_str());
        let merged_extra_value = merged_extra
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        let preset_id_update = req.extra.as_ref().and_then(|extra| {
            extra.get("preset_id").map(|_| {
                merged_extra_value
                    .as_ref()
                    .and_then(|value| value.get("preset_id"))
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            })
        });
        let preset_revision_update = req.extra.as_ref().and_then(|extra| {
            extra.get("preset_revision").map(|_| {
                merged_extra_value
                    .as_ref()
                    .and_then(|value| value.get("preset_revision"))
                    .and_then(serde_json::Value::as_i64)
            })
        });
        let preset_snapshot_update = req.extra.as_ref().and_then(|extra| {
            extra.get("preset_snapshot").map(|_| {
                merged_extra_value
                    .as_ref()
                    .and_then(|value| value.get("preset_snapshot"))
                    .filter(|value| !value.is_null())
                    .and_then(|value| serde_json::to_string(value).ok())
            })
        });

        let model_json = req
            .model
            .as_ref()
            .map(|m| {
                serde_json::to_string(m)
                    .map(Some)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize model: {e}")))
            })
            .transpose()?;

        let delegation_policy_changed = req
            .delegation_policy
            .is_some_and(|policy| existing.delegation_policy != policy.as_str());
        let delegation_policy = req
            .delegation_policy
            .map(|policy| policy.as_str().to_owned());
        if let Some(Some(pool)) = req.execution_model_pool.as_ref() {
            pool.validate().map_err(AppError::BadRequest)?;
        }
        let persisted_model = existing
            .model
            .as_deref()
            .map(parse_provider_with_model)
            .transpose()?;
        let persisted_pool = existing
            .execution_model_pool
            .as_deref()
            .map(serde_json::from_str::<ExecutionModelPool>)
            .transpose()
            .map_err(|error| {
                AppError::Internal(format!("Invalid persisted execution model pool: {error}"))
            })?;
        let effective_pool = match req.execution_model_pool.as_ref() {
            None => persisted_pool.as_ref(),
            Some(None) => None,
            Some(Some(pool)) => Some(pool),
        };
        validate_conversation_model_authority(
            req.model.as_ref().or(persisted_model.as_ref()),
            effective_pool,
        )?;
        let execution_model_pool = match req.execution_model_pool.as_ref() {
            None => None,
            Some(None) => Some(None),
            Some(Some(pool)) => Some(Some(serde_json::to_string(pool).map_err(|error| {
                AppError::Internal(format!("Failed to serialize execution model pool: {error}"))
            })?)),
        };
        let decision_policy = req
            .decision_policy
            .map(|policy| policy.as_str().to_owned());
        let execution_template_id = req.execution_template_id;
        if let Some(Some(template_id)) = execution_template_id.as_ref() {
            AgentExecutionTemplateId::try_from(template_id.as_str()).map_err(|error| {
                AppError::BadRequest(format!("invalid execution_template_id: {error}"))
            })?;
        }

        let updates = ConversationRowUpdate {
            name: req.name,
            pinned: req.pinned,
            pinned_at,
            model: model_json,
            extra: merged_extra,
            delegation_policy,
            execution_model_pool,
            decision_policy,
            execution_template_id,
            status: None,
            cron_job_id: None,
            preset_id: preset_id_update,
            preset_revision: preset_revision_update,
            preset_snapshot: preset_snapshot_update,
            updated_at: Some(now),
        };

        self.conversation_repo.update(parse_conv_id(id)?, &updates).await?;

        if model_changed || workspace_changed || preset_changed || delegation_policy_changed {
            info!(
                model_changed,
                workspace_changed,
                preset_changed,
                delegation_policy_changed,
                "Conversation updated, terminating Agent runtime so the change takes effect on the next message"
            );
            runtime_registry.terminate_and_wait(id, None).await;
        }

        // Re-fetch to return the updated version
        let updated = self
            .conversation_repo
            .get(parse_conv_id(id)?)
            .await?
            .ok_or_else(|| AppError::Internal("Conversation vanished after update".into()))?;

        let mut response = row_to_response(updated, &self.workspace_root)?;
        self.project_execution_relation(user_id, &mut response).await?;

        info!("Conversation updated");
        self.broadcast_list_changed(user_id, id, "updated", response.source.as_ref());

        Ok(response)
    }

    /// Merge backend-owned Agent metadata into `conversation.extra` without
    /// touching the typed conversation fields or terminating its runtime.
    ///
    /// Execution identity and policy are deliberately rejected here: they have
    /// first-class persistence and must not regain a second source of truth via
    /// an internal caller.
    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    pub async fn update_extra(&self, conversation_id: &str, patch: serde_json::Value) -> Result<(), AppError> {
        reject_execution_policy_extra_keys(&patch)?;

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

    fn spawn_conversation_delete_owner(
        &self,
        mut deletion_guard: ConversationDeletionGuard,
        user_id: String,
        conversation_id: String,
        source: Option<ConversationSource>,
        managed_temp_workspace: Option<PathBuf>,
    ) -> oneshot::Receiver<Result<(), AppError>> {
        let (result_tx, result_rx) = oneshot::channel();
        let service = self.clone();
        let hooks: Vec<Arc<dyn OnConversationDelete>> =
            self.delete_hooks.read().map(|guard| guard.clone()).unwrap_or_default();
        tokio::spawn(async move {
            // The detached coordinator owns deletion from stop-join through
            // core persistence. Request cancellation/timeout therefore cannot
            // drop the guard and reopen admission halfway through cleanup.
            let stop_rx = service.spawn_turn_stop_cleanup(
                user_id.clone(),
                conversation_id.clone(),
                Arc::clone(&service.runtime_registry),
                false,
                true,
            );
            match tokio::time::timeout(DELETE_STOP_GRACE, stop_rx).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => warn!(
                    conversation_id,
                    error = %ErrorChain(&error),
                    "Conversation delete stop coordinator reported an error; waiting for its fences"
                ),
                Ok(Err(_)) => warn!(
                    conversation_id,
                    "Conversation delete stop coordinator exited without a result; waiting for its fences"
                ),
                Err(_) => warn!(
                    conversation_id,
                    "Conversation delete stop coordinator exceeded its receive bound; waiting for its fences"
                ),
            }
            if !service
                .runtime_state
                .wait_for_cleanup_fences(&conversation_id, DELETE_STOP_GRACE)
                .await
            {
                let _ = result_tx.send(Err(AppError::Timeout(
                    "conversation delete could not establish quiescence within its hard bound"
                        .to_owned(),
                )));
                return;
            }
            // A deletion-owned stop may have joined a normal-completion fence
            // before that completion left an intentionally reusable idle
            // runtime. Definitive deletion must evict that slot/process and
            // clear registry governor state even when the first stop was only
            // a follower.
            if tokio::time::timeout(
                CANCEL_TEARDOWN_GRACE,
                service.runtime_registry.terminate_and_wait(
                    &conversation_id,
                    Some(AgentKillReason::ConversationDeleted),
                ),
            )
            .await
            .is_err()
            {
                warn!(
                    conversation_id,
                    "Conversation-deleted runtime teardown exceeded its hard bound; deletion remains fenced"
                );
            }

            let delete_timed_out = match tokio::time::timeout(
                DELETE_CORE_GRACE,
                service.conversation_repo.delete(&conversation_id),
            )
            .await
            {
                Ok(Ok(())) => false,
                Ok(Err(error)) => {
                    let _ = result_tx.send(Err(error.into()));
                    return;
                }
                Err(_) => {
                    warn!(
                        conversation_id,
                        "Core conversation delete timed out; verifying the serialized result in background"
                    );
                    true
                }
            };

            if delete_timed_out {
                // SQLite may already have queued a command whose response
                // future timed out. Keep the deletion tombstone owned until a
                // later read, serialized behind that command, establishes
                // whether deletion committed. This prevents a late delete from
                // racing a newly admitted turn.
                let mut retry_delay = Duration::from_millis(100);
                loop {
                    match tokio::time::timeout(
                        DELETE_CLEANUP_ITEM_GRACE,
                        service.conversation_repo.get(&conversation_id),
                    )
                    .await
                    {
                        Ok(Ok(None)) => break,
                        Ok(Ok(Some(_))) => {
                            let _ = result_tx.send(Err(AppError::Timeout(
                                "conversation delete timed out without deleting the row".to_owned(),
                            )));
                            return;
                        }
                        Ok(Err(error)) => warn!(
                            conversation_id,
                            error = %ErrorChain(&error),
                            "Failed to verify timed-out conversation delete"
                        ),
                        Err(_) => warn!(
                            conversation_id,
                            "Timed out verifying timed-out conversation delete; retaining the deletion fence and retrying"
                        ),
                    }
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
                }
            }

            deletion_guard.commit();
            service.runtime_state.clear_knowledge_signature(&conversation_id);
            service.runtime_state.clear_turn_tokens(&conversation_id);
            info!(conversation_id, "Conversation deleted");
            service.broadcast_list_changed(
                &user_id,
                &conversation_id,
                "deleted",
                source.as_ref(),
            );
            // Report authoritative core success before optional cascade work.
            // If the HTTP waiter timed out/disconnected, this send simply
            // fails while the committed tombstone and cleanup remain owned by
            // this detached task.
            let _ = result_tx.send(Ok(()));

            match tokio::time::timeout(
                DELETE_CLEANUP_ITEM_GRACE,
                service.acp_session_repo.delete(&conversation_id),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => warn!(
                    conversation_id,
                    error = %ErrorChain(&err),
                    "Failed to delete acp_session row on conversation delete"
                ),
                Err(_) => warn!(
                    conversation_id,
                    "Timed out deleting acp_session row on conversation delete"
                ),
            }

            for hook in hooks {
                if tokio::time::timeout(
                    DELETE_CLEANUP_ITEM_GRACE,
                    hook.on_conversation_deleted(&user_id, &conversation_id),
                )
                .await
                .is_err()
                {
                    warn!(conversation_id, "Conversation delete hook exceeded its hard bound");
                }
            }

            if let Some(path) = managed_temp_workspace {
                let display_path = path.display().to_string();
                match tokio::time::timeout(
                    DELETE_CLEANUP_ITEM_GRACE,
                    tokio::task::spawn_blocking(move || {
                        if path.exists() {
                            std::fs::remove_dir_all(path)
                        } else {
                            Ok(())
                        }
                    }),
                )
                .await
                {
                    Ok(Ok(Ok(()))) => {}
                    Ok(Ok(Err(err))) => warn!(
                        conversation_id,
                        workspace = %display_path,
                        error = %ErrorChain(&err),
                        "Failed to remove managed temp workspace on conversation delete"
                    ),
                    Ok(Err(err)) => warn!(
                        conversation_id,
                        workspace = %display_path,
                        error = %ErrorChain(&err),
                        "Managed temp workspace cleanup task failed"
                    ),
                    Err(_) => warn!(
                        conversation_id,
                        workspace = %display_path,
                        "Managed temp workspace cleanup exceeded its hard bound"
                    ),
                }
            }
        });
        result_rx
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

        if self
            .is_execution_attempt_conversation(user_id, conv_id)
            .await?
        {
            return Err(AppError::Conflict(
                "Execution attempt conversations are retained as audit history and cannot be deleted directly"
                    .into(),
            ));
        }

        // Deletion is stronger than a stop: block all future admissions in
        // the same synchronous boundary used by turn acquisition, then tear
        // down/force-release the exact current generation before deleting any
        // persisted rows. An uncommitted guard rolls back if deletion fails.
        let deletion_guard = self
            .runtime_state
            .begin_conversation_deletion(id)?;

        let source: Option<ConversationSource> = existing
            .source
            .as_deref()
            .and_then(|s| string_to_enum::<ConversationSource>(s).ok());
        let managed_temp_workspace =
            managed_temp_workspace_path_from_row(&self.workspace_root, &existing);

        let delete_rx = self.spawn_conversation_delete_owner(
            deletion_guard,
            user_id.to_owned(),
            id.to_owned(),
            source,
            managed_temp_workspace,
        );
        match tokio::time::timeout(DELETE_CORE_GRACE, delete_rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => {
                return Err(AppError::Internal(
                    "conversation delete owner exited unexpectedly".to_owned(),
                ));
            }
            Err(_) => {
                return Err(AppError::Timeout(
                    "conversation deletion continues in the background; the deleted event is the authoritative success signal"
                        .to_owned(),
                ));
            }
        }

        // Snapshot the hook list under the read lock, then drop the guard
        // before awaiting — `RwLockReadGuard` is not `Send`, so holding it
        // across `.await` would make this future non-`Send`.

        // Drop the in-memory knowledge signature so the map does not retain
        // entries for deleted conversations across a long-lived process.
        // Likewise drop any accumulated token total — an execution-attempt
        // conversation that errored before the attempt consumed it would otherwise
        // linger here until process restart (a small in-memory leak). No-op for the
        // common chat/companion conversation (which never records a token total).

        Ok(())
    }

    /// Create a conversation from a `CloneConversationRequest`.
    ///
    /// Historically this method supported cloning from a source conversation
    /// (inheriting name / extra / cron binding). That use case has been
    /// removed — the method is retained only because `POST
    /// /api/conversations/clone` has three active callers
    /// (`_AddNewConversation`, Agent runtime registry, legacy repo shim) that
    /// send a pre-built payload shape. New code should prefer `create`.
    pub async fn clone_create(
        &self,
        user_id: &str,
        mut req: CloneConversationRequest,
    ) -> Result<ConversationResponse, AppError> {
        strip_clone_instance_state(&mut req.conversation.extra);
        self.create(user_id, req.conversation).await
    }

    /// Reset a conversation: clear messages and set status back to pending.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn reset(&self, user_id: &str, id: &str) -> Result<(), AppError> {
        let conv_id = parse_conv_id(id)?;
        // Verify existence and ownership
        self.conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;

        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;

        // Delete all messages
        self.conversation_repo
            .delete_messages_by_conversation(conv_id)
            .await?;
        self.conversation_repo
            .delete_artifacts_by_conversation(conv_id)
            .await?;

        // Reset status to pending
        let now = now_ms();
        let updates = ConversationRowUpdate {
            status: Some(enum_to_db(&ConversationStatus::Pending)?),
            updated_at: Some(now),
            ..Default::default()
        };
        self.conversation_repo.update(conv_id, &updates).await?;

        info!("Conversation reset");
        Ok(())
    }

    /// List conversations associated by the same workspace.
    pub async fn list_associated(&self, user_id: &str, id: &str) -> Result<Vec<ConversationResponse>, AppError> {
        let rows = self.conversation_repo.list_associated(user_id, parse_conv_id(id)?).await?;
        let mut responses = Vec::with_capacity(rows.len());
        for row in rows {
            let mut response = row_to_response(row, &self.workspace_root)?;
            self.project_execution_relation(user_id, &mut response).await?;
            responses.push(response);
        }
        Ok(responses)
    }

    /// List conversations spawned by a specific cron job.
    pub async fn list_by_cron_job(
        &self,
        user_id: &str,
        cron_job_id: &str,
    ) -> Result<Vec<ConversationResponse>, AppError> {
        let rows = self.conversation_repo.list_by_cron_job(user_id, cron_job_id).await?;
        let mut responses = Vec::with_capacity(rows.len());
        for row in rows {
            let mut response = row_to_response(row, &self.workspace_root)?;
            self.project_execution_relation(user_id, &mut response).await?;
            responses.push(response);
        }
        Ok(responses)
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
            .get_message(
                parse_conv_id(conversation_id)?,
                parse_message_id(message_id)?,
            )
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
        artifact_id: &str,
        req: UpdateConversationArtifactRequest,
    ) -> Result<ConversationArtifactResponse, AppError> {
        let artifact_id = ConversationArtifactId::try_from(artifact_id)
            .map_err(|_| AppError::BadRequest(format!("invalid artifact id: {artifact_id}")))?;
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
            .update_artifact_status(
                parse_conv_id(conversation_id)?,
                artifact_id.as_ref(),
                &status,
                now_ms(),
            )
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Artifact {artifact_id} not found")))?;

        let response = row_to_artifact_response(row)?;
        self.user_events.send_to_user(
            user_id,
            WebSocketMessage::new(
                "conversation.artifact",
                serde_json::to_value(&response)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize artifact event: {e}")))?,
            ),
        );

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

        let mut items = result
            .items
            .into_iter()
            .map(|row| search_row_to_item(row, &self.workspace_root))
            .collect::<Result<Vec<_>, _>>()?;
        for item in &mut items {
            self.project_execution_relation(user_id, &mut item.conversation)
                .await?;
        }

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
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<ConfirmationListResponse, AppError> {
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let agent = match runtime_registry.get_runtime(conversation_id) {
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
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(), AppError> {
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let agent = runtime_registry
            .get_runtime(conversation_id)
            .ok_or_else(|| AppError::NotFound("No active agent for this conversation".into()))?;

        let confirmations = agent.get_confirmations();
        let conf_id = confirmations
            .iter()
            .find(|c| c.call_id == call_id)
            .map(|c| c.id.clone());

        agent.confirm(&req.msg_id, call_id, req.data, req.always_allow)?;

        if let Some(conf_id) = conf_id {
            let payload = serde_json::json!({
                "conversation_id": conversation_id,
                "id": conf_id,
            });
            let msg = WebSocketMessage::new("confirmation.remove", payload);
            self.user_events.send_to_user(user_id, msg);
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
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<ApprovalCheckResponse, AppError> {
        self.conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        let approved = runtime_registry
            .get_runtime(conversation_id)
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
    /// 3. Acquires the conversation's turn handle in runtime state
    /// 4. Spawns background agent build/send and stream relay work
    /// 5. Returns immediately (202 Accepted semantics)
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn send_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<String, AppError> {
        let lease = self.begin_public_runtime_build(conversation_id, user_id)?;
        self.send_message_inner(
            user_id,
            conversation_id,
            req,
            runtime_registry,
            None,
            Some(lease),
        )
        .await
    }

    pub async fn send_message_with_runtime_build_lease(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        lease: RuntimeBuildLease,
    ) -> Result<String, AppError> {
        self.send_message_inner(
            user_id,
            conversation_id,
            req,
            runtime_registry,
            None,
            Some(lease),
        )
        .await
    }

    /// Trusted at-least-once delivery boundary for durable internal effects.
    /// `operation_id` is a natural idempotency key used to claim one receipt;
    /// the receipt owns a separately minted canonical MessageId. It is deliberately not
    /// part of `SendMessageRequest`, so public callers still receive freshly
    /// server-minted message identities on every accepted request.
    pub(crate) async fn send_message_idempotent(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<IdempotentMessageDelivery, AppError> {
        let operation_id = operation_id.trim();
        if operation_id.is_empty() {
            return Err(AppError::BadRequest(
                "internal message operation id must not be empty".to_owned(),
            ));
        }
        let conversation_key = parse_conv_id(conversation_id)?;
        let runtime_build_lease = self.runtime_state.begin_runtime_build(conversation_key)?;
        let request_payload = serde_json::json!({
            "content": &req.content,
            "files": &req.files,
            "inject_skills": &req.inject_skills,
            "hidden": req.hidden,
            "origin": &req.origin,
            "channel_platform": &req.channel_platform,
        })
        .to_string();
        let receipt = self
            .conversation_repo
            .claim_delivery_receipt(
                user_id,
                conversation_key,
                operation_id,
                "turn",
                &request_payload,
                now_ms(),
            )
            .await?;
        let message_id = receipt.message_id.clone();
        if receipt.status == "completed" {
            return Ok(IdempotentMessageDelivery {
                message_id,
                completed: true,
                result_ok: receipt.result_ok,
                result_text: receipt.result_text,
                result_error: receipt.result_error,
            });
        }
        let operation_guard_key = Self::durable_operation_key(user_id, conversation_key, operation_id);
        let guard_generation = self
            .next_durable_operation_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        {
            let mut operations = self.durable_operations_in_flight.lock().map_err(|_| {
                AppError::Internal("durable operation guard lock poisoned".to_owned())
            })?;
            if let Some(in_flight) = operations.get(&operation_guard_key).cloned() {
                return Ok(IdempotentMessageDelivery {
                    message_id: in_flight.message_id,
                    completed: false,
                    result_ok: None,
                    result_text: None,
                    result_error: None,
                });
            }
            operations.insert(
                operation_guard_key.clone(),
                DurableOperationLease {
                    message_id: message_id.clone(),
                    generation: guard_generation,
                },
            );
        }

        let delivery_lease = DurableDeliveryLease {
            operation_id: operation_id.to_owned(),
            message_id: message_id.clone(),
            guard_key: operation_guard_key.clone(),
            guard_generation,
            receipt_handed_off: Arc::new(AtomicBool::new(false)),
        };

        let accepted_message_id = match self
            .send_message_inner(
                user_id,
                conversation_id,
                req,
                runtime_registry,
                Some(delivery_lease.clone()),
                Some(runtime_build_lease),
            )
            .await
        {
            Ok(message_id) => {
                // A path that only observed an already-running delivery did
                // not take ownership of receipt completion. Do not retain an
                // unowned guard forever; the actual owner already has its own
                // generation-scoped guard (or a later replay may recover it).
                if !delivery_lease.receipt_was_handed_off() {
                    Self::release_durable_operation_guard(
                        &self.durable_operations_in_flight,
                        &operation_guard_key,
                        guard_generation,
                    );
                }
                message_id
            }
            Err(error) => {
                // Once the inner path has attempted or detached durable
                // receipt completion, it exclusively owns this lease.  An
                // unconditional remove here used to reopen redelivery while
                // compensation was still running, duplicating model/tool side
                // effects and creating irreconcilable competing results.
                if !delivery_lease.receipt_was_handed_off() {
                    Self::release_durable_operation_guard(
                        &self.durable_operations_in_flight,
                        &operation_guard_key,
                        guard_generation,
                    );
                }
                return Err(error);
            }
        };
        Ok(IdempotentMessageDelivery {
            message_id: accepted_message_id,
            completed: false,
            result_ok: None,
            result_text: None,
            result_error: None,
        })
    }

    /// Project a finalized assistant result into a Conversation without
    /// starting an Agent turn. This is the sole boundary for durable
    /// execution-result delivery: the repository atomically binds the stable
    /// operation identity to one canonical left-side text row, then this
    /// method publishes the existing final-content realtime contract only
    /// after that transaction commits.
    ///
    /// Replays intentionally rebroadcast the same stable `msg_id`. This closes
    /// the crash window between DB commit and WebSocket publication; clients
    /// already merge stream messages by `msg_id` and `replace=true`.
    /// `stream_complete=true` makes the lifecycle boundary explicit: this is a
    /// self-contained projection, not the beginning of a model stream, so UI
    /// activity indicators must render it without entering a running state.
    pub async fn project_assistant_message_idempotent(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        content: &str,
        origin: &str,
    ) -> Result<String, AppError> {
        let conversation_id = parse_conv_id(conversation_id)?;
        let operation_id = operation_id.trim();
        let content = content.trim();
        let origin = origin.trim();
        if operation_id.is_empty() {
            return Err(AppError::BadRequest(
                "assistant projection operation id must not be empty".to_owned(),
            ));
        }
        if content.is_empty() {
            return Err(AppError::BadRequest(
                "assistant projection content must not be empty".to_owned(),
            ));
        }
        if origin.is_empty() {
            return Err(AppError::BadRequest(
                "assistant projection origin must not be empty".to_owned(),
            ));
        }

        let message_id = MessageId::new().into_string();
        let created_at = now_ms();
        let request_payload = serde_json::json!({
            "content": content,
            "origin": origin,
        })
        .to_string();
        let candidate = MessageRow {
            id: message_id.clone(),
            conversation_id: conversation_id.to_owned(),
            msg_id: Some(message_id),
            r#type: "text".to_owned(),
            content: serde_json::json!({ "content": content }).to_string(),
            position: Some("left".to_owned()),
            status: Some("finish".to_owned()),
            hidden: false,
            created_at,
        };
        let projected = self
            .conversation_repo
            .project_assistant_message_with_receipt(
                user_id,
                conversation_id,
                operation_id,
                "projection",
                &request_payload,
                &candidate,
                created_at,
            )
            .await?;
        let conversation = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .filter(|conversation| conversation.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("conversation {conversation_id}")))?;
        let (companion, companion_id, channel_platform) =
            companion_context_from_extra(&conversation.extra)?;
        let row = projected.message;
        let data = serde_json::from_str::<serde_json::Value>(&row.content)
            .unwrap_or_else(|_| serde_json::json!({ "content": row.content }));
        let stable_message_id = row.msg_id.clone().unwrap_or_else(|| row.id.clone());
        self.user_events.send_to_user(
            user_id,
            WebSocketMessage::new(
                "message.stream",
                serde_json::json!({
                "conversation_id": row.conversation_id,
                "msg_id": stable_message_id,
                "type": "content",
                "data": data,
                "position": row.position,
                "status": row.status,
                "hidden": row.hidden,
                "replace": true,
                "stream_complete": true,
                "origin": origin,
                "companion": companion,
                "companion_id": companion_id,
                "channel_platform": channel_platform,
                "created_at": row.created_at,
                }),
            ),
        );
        Ok(row.id)
    }

    pub(crate) async fn idempotent_delivery_result(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
    ) -> Result<Option<IdempotentMessageDelivery>, AppError> {
        let conversation_id = parse_conv_id(conversation_id)?;
        let receipt = self
            .conversation_repo
            .get_delivery_receipt(user_id, conversation_id, operation_id)
            .await?;
        Ok(receipt.map(|receipt| IdempotentMessageDelivery {
            message_id: receipt.message_id,
            completed: receipt.status == "completed",
            result_ok: receipt.result_ok,
            result_text: receipt.result_text,
            result_error: receipt.result_error,
        }))
    }

    async fn send_message_inner(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        durable_delivery: Option<DurableDeliveryLease>,
        mut runtime_build_lease: Option<RuntimeBuildLease>,
    ) -> Result<String, AppError> {
        let public_cancellable = durable_delivery.is_none();
        // Snapshot before the first await. A stop racing this request advances
        // the epoch even if no turn handle exists yet; admission later fails
        // instead of starting work after stop already returned success.
        let send_cancellation_epoch = runtime_build_lease
            .as_ref()
            .map(RuntimeBuildLease::expected_cancellation_epoch)
            .unwrap_or_else(|| self.runtime_state.cancellation_epoch(conversation_id));
        if let Some(lease) = runtime_build_lease.as_ref() {
            lease.ensure_active()?;
        }
        let durable_operation_id = durable_delivery
            .as_ref()
            .map(|delivery| delivery.operation_id.as_str());
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
        let conversation_key = row.id.clone();
        if let Some(lease) = runtime_build_lease.as_ref() {
            lease.ensure_active()?;
        }

        if !self.execution_authority(user_id).controls_host() {
            if row.r#type != AgentType::Nomi.serde_name() {
                return Err(AppError::Forbidden(
                    "Non-owner conversations are model-only; host runtimes cannot be started"
                        .into(),
                ));
            }
            if !req.files.is_empty() || !req.inject_skills.is_empty() {
                return Err(AppError::Forbidden(
                    "Model-only conversations cannot attach host files or inject installation skills"
                        .into(),
                ));
            }
        }

        // Attempt transcripts are owned by their Agent Execution for their
        // entire retained lifetime, not only while the Attempt link is active.
        // Only the execution infrastructure port supplies a durable operation
        // identity and may start the initial/continuation turn.
        if durable_delivery.is_none() {
            self.ensure_not_retained_execution_attempt(user_id, &conversation_key)
                .await?;
        }

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

        let track_execution_tokens = self
            .is_active_execution_attempt_conversation(user_id, &row.id)
            .await?;

        let user_msg_id = durable_delivery
            .as_ref()
            .map(|delivery| delivery.message_id.clone())
            .unwrap_or_else(Self::mint_msg_id);
        let existing_user_message = self
            .conversation_repo
            .get_message(&row.id, &user_msg_id)
            .await?;
        if let Some(existing) = existing_user_message.as_ref() {
            let expected_content = serde_json::json!({ "content": &req.content }).to_string();
            if existing.position.as_deref() != Some("right")
                || existing.r#type != "text"
                || existing.content != expected_content
            {
                return Err(AppError::Conflict(
                    "internal message operation id was reused with different content".to_owned(),
                ));
            }
            // The first delivery is still owned by this live process.  Returning
            // the stable message identity lets the caller await that turn
            // instead of racing a duplicate model invocation.
            if self.runtime_summary_for(conversation_id).await.is_processing {
                return Ok(user_msg_id);
            }
        }

        let durable_guard = durable_delivery
            .as_ref()
            .map(|delivery| (delivery.guard_key.clone(), delivery.guard_generation));

        // Resolve every fallible/awaiting preflight before turn admission. A
        // stop racing this work advances the epoch, and the admission check
        // below rejects that stale send instead of stranding a turn handle.
        let (companion, companion_id, extra_channel_platform) =
            companion_context_from_extra(&row.extra)?;
        let channel_platform = req
            .channel_platform
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or(extra_channel_platform);
        let origin = req
            .origin
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        let mut runtime_options = match self.build_runtime_options(&row) {
            Ok(opts) => opts,
            Err(err) => {
                error!(
                    error_code = err.error_code(),
                    error = %ErrorChain(&err),
                    "Failed to build runtime options for message send"
                );
                let _ = self.persist_send_failure_tip(conversation_id, &err).await;
                let receipt_error = format!("{}", ErrorChain(&err));
                if let Some(delivery) = durable_delivery.as_ref() {
                    delivery.handoff_receipt();
                }
                Self::complete_delivery_receipt_before_release(
                    &self.conversation_repo,
                    user_id,
                    &conversation_key,
                    durable_operation_id,
                    false,
                    None,
                    Some(&receipt_error),
                    &CancellationToken::new(),
                    durable_guard.as_ref().map(|(key, generation)| {
                        (&self.durable_operations_in_flight, key.as_str(), *generation)
                    }),
                )
                .await;
                return Err(err);
            }
        };
        self.ensure_auto_workspace_skill_links(&row, &runtime_options)
            .await?;
        if let Some(lease) = runtime_build_lease.as_ref() {
            lease.ensure_active()?;
        }
        let preparation_token = runtime_build_lease
            .as_ref()
            .map(RuntimeBuildLease::cancellation_token);
        self.apply_knowledge_mounts(
            &row,
            &mut runtime_options,
            preparation_token.as_ref(),
        )
        .await;
        if let Some(lease) = runtime_build_lease.as_ref() {
            lease.ensure_active()?;
        }
        let stored_workspace = runtime_options.workspace.clone();

        let first_turn_msg_id = Self::mint_msg_id();
        let mut turn_handle = self
            .runtime_state
            .try_acquire_turn_with_wire_context_at_epoch_and_owner(
                conversation_id,
                Some(first_turn_msg_id.clone()),
                TurnWireContext {
                    companion,
                    companion_id: companion_id.clone(),
                    origin: origin.clone(),
                    channel_platform: channel_platform.clone(),
                },
                Some(send_cancellation_epoch),
                Some(user_id.to_owned()),
                public_cancellable,
                preparation_token.as_ref(),
            )?;
        // Exact turn admission takes over cancellation/runtime ownership. A
        // stop either cancelled the preparation lease before this point or now
        // captures the active generation; no unowned build gap remains.
        drop(runtime_build_lease.take());
        let turn_cancellation = turn_handle.turn_cancellation();
        let turn_token = turn_handle.cancellation_token();

        // Store user message. `msg_id` is server-generated so the WebSocket
        // stream, DB row, and client-side message index all agree on the same
        // key. We reuse the same value for `id` (primary key) and `msg_id`
        // to preserve legacy callers that still rely on `id == msg_id`.
        let user_msg = nomifun_db::models::MessageRow {
            id: user_msg_id.clone(),
            conversation_id: conversation_id.to_owned(),
            msg_id: Some(user_msg_id.clone()),
            r#type: "text".into(),
            content: serde_json::json!({ "content": req.content }).to_string(),
            position: Some("right".into()),
            status: Some("finish".into()),
            hidden: req.hidden,
            created_at: now_ms(),
        };
        if existing_user_message.is_none() {
            if let Err(e) = self.conversation_repo.insert_message(&user_msg).await {
                warn!(msg_id = %user_msg_id, error = %ErrorChain(&e), "Failed to insert user message");
                return Err(e.into());
            }

            info!(msg_id = %user_msg_id, "User message persisted");
        }

        // Turn admission is the ownership transfer point for durable
        // delivery. From here either the normal background task or the stop
        // race compensation path owns the receipt lease.
        if let Some(delivery) = durable_delivery.as_ref() {
            delivery.handoff_receipt();
        }
        if existing_user_message.is_none() {
            self.user_events.send_to_user(
                user_id,
                WebSocketMessage::new(
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
                ),
            );
        }

        if turn_token.is_cancelled() {
            Self::complete_delivery_receipt_before_release(
                &self.conversation_repo,
                user_id,
                &conversation_key,
                durable_operation_id,
                false,
                None,
                Some("Agent turn cancelled during admission"),
                &turn_token,
                durable_guard.as_ref().map(|(key, generation)| {
                    (&self.durable_operations_in_flight, key.as_str(), *generation)
                }),
            )
            .await;
            let _ = turn_handle.release();
            return Ok(user_msg_id);
        }

        self.broadcast_turn_started_with_context(
            user_id,
            conversation_id,
            &first_turn_msg_id,
            companion,
            companion_id.clone(),
            origin.clone(),
            channel_platform.clone(),
        )
        .await;

        // A stop may arrive while the asynchronous started-event boundary is
        // yielding. Never start a runtime after the stop worker has already
        // published the cancelled terminal/completion for this generation.
        if turn_token.is_cancelled() {
            Self::complete_delivery_receipt_before_release(
                &self.conversation_repo,
                user_id,
                &conversation_key,
                durable_operation_id,
                false,
                None,
                Some("Agent turn cancelled before runtime startup"),
                &turn_token,
                durable_guard.as_ref().map(|(key, generation)| {
                    (&self.durable_operations_in_flight, key.as_str(), *generation)
                }),
            )
            .await;
            let _ = turn_handle.release();
            return Ok(user_msg_id);
        }

        let conv_id = conversation_id.to_owned();
        let repo = Arc::clone(&self.conversation_repo);
        let user_events = Arc::clone(&self.user_events);
        let cron_service = self.current_cron_service();
        let user_id_owned = user_id.to_owned();
        let service = self.clone();
        let runtime_registry = Arc::clone(runtime_registry);
        let durable_operation_id = durable_operation_id.map(str::to_owned);
        // Only an active attempt relation needs per-turn token accounting. The
        // relation repository is authoritative; Conversation extra carries no
        // execution identity. Ordinary chat/companion turns therefore create no
        // accumulator entry.
        let token_runtime_state = track_execution_tokens.then(|| Arc::clone(&self.runtime_state));
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
        let stable_turn_id = first_turn_msg_id.clone();
        let owner_turn_generation = turn_handle.turn_id();
        let owner_conversation_id = conversation_key.clone();
        let owner_task = tokio::spawn(async move {
            let mut turn_handle = turn_handle;
            let panic_user_id = user_id_owned.clone();
            let panic_conversation_id = conv_id.clone();
            let panic_stable_turn_id = stable_turn_id.clone();
            let panic_runtime_registry = Arc::clone(&runtime_registry);
            let panic_wire_context = TurnWireContext {
                companion,
                companion_id: companion_id.clone(),
                origin: origin.clone(),
                channel_platform: channel_platform.clone(),
            };
            let mut owner_receipt_guard = OwnerReceiptGuard {
                repo: Arc::clone(&repo),
                user_id: user_id_owned.clone(),
                conversation_id: conversation_key.clone(),
                operation_id: durable_operation_id.clone(),
                operation_guard: durable_guard.as_ref().map(|(key, generation)| {
                    (
                        Arc::clone(&service.durable_operations_in_flight),
                        key.clone(),
                        *generation,
                    )
                }),
                armed: true,
            };
            let owner_result = AssertUnwindSafe(async {
            let mut turn_cancellation = turn_cancellation;
            let build_started_at = now_ms();
            info!(conversation_id = %conv_id, "Agent runtime build started");
            let knowledge_extra = runtime_options.extra.clone();
            let mut agent = match runtime_registry
                .get_or_create_runtime_for_turn(
                    &conv_id,
                    turn_cancellation.turn_id(),
                    turn_token.clone(),
                    runtime_options,
                )
                .await
            {
                Ok(agent) => agent,
                Err(err) => {
                    let cancelled = turn_token.is_cancelled();
                    if !cancelled {
                        error!(
                            conversation_id = %conv_id,
                            error_code = err.error_code(),
                            error = %ErrorChain(&err),
                            "Agent runtime build failed"
                        );
                        service
                            .persist_and_broadcast_send_failure_tip(&user_id_owned, &conv_id, &err)
                            .await;
                    }
                    let receipt_error = format!("{}", ErrorChain(&err));
                    Self::complete_delivery_receipt_before_release(
                        &repo,
                        &user_id_owned,
                        &conversation_key,
                        durable_operation_id.as_deref(),
                        false,
                        None,
                        Some(&receipt_error),
                        &turn_token,
                        durable_guard.as_ref().map(|(key, generation)| {
                            (&service.durable_operations_in_flight, key.as_str(), *generation)
                        }),
                    )
                    .await;
                    service
                        .release_and_complete_turn(
                            &mut turn_handle,
                            &user_id_owned,
                            &conv_id,
                            &stable_turn_id,
                            companion,
                            companion_id.clone(),
                            origin.clone(),
                            channel_platform.clone(),
                        )
                        .await;
                    return;
                }
            };

            if turn_token.is_cancelled() {
                Self::complete_delivery_receipt_before_release(
                    &repo,
                    &user_id_owned,
                    &conversation_key,
                    durable_operation_id.as_deref(),
                    false,
                    None,
                    Some("Agent turn cancelled during runtime startup"),
                    &turn_token,
                    durable_guard.as_ref().map(|(key, generation)| {
                        (&service.durable_operations_in_flight, key.as_str(), *generation)
                    }),
                )
                .await;
                let _ = turn_handle.release();
                return;
            }

            // Arm IDMM supervision now that the Agent runtime exists (so the
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
                service
                    .persist_and_broadcast_send_failure_tip(&user_id_owned, &conv_id, &err)
                    .await;
                let receipt_error = format!("{}", ErrorChain(&err));
                Self::complete_delivery_receipt_before_release(
                    &repo,
                    &user_id_owned,
                    &conversation_key,
                    durable_operation_id.as_deref(),
                    false,
                        None,
                        Some(&receipt_error),
                        &turn_token,
                        durable_guard.as_ref().map(|(key, generation)| {
                            (&service.durable_operations_in_flight, key.as_str(), *generation)
                        }),
                    )
                    .await;
                service
                    .release_and_complete_turn(
                        &mut turn_handle,
                        &user_id_owned,
                        &conv_id,
                        &stable_turn_id,
                        companion,
                        companion_id.clone(),
                        origin.clone(),
                        channel_platform.clone(),
                    )
                    .await;
                return;
            }

            info!(
                conversation_id = %conv_id,
                agent_type = ?agent.agent_type(),
                elapsed_ms = now_ms().saturating_sub(build_started_at),
                "Agent runtime ready"
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
            let turn_user_text = pending_send
                .as_ref()
                .map(|(send, _)| send.content.clone())
                .unwrap_or_default();
            let turn_origin = origin.clone();
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
            let mut final_turn_writeback: Option<(
                Arc<nomifun_knowledge::KnowledgeService>,
                nomifun_knowledge::TurnWritebackRequest,
                String,
                String,
            )> = None;
            let mut durable_completion: Option<(bool, Option<String>, Option<String>)> = None;
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
                if turn_token.is_cancelled() {
                    durable_completion = Some((
                        false,
                        None,
                        Some("Agent turn was cancelled before send".to_owned()),
                    ));
                    break;
                }
                if turn_cancellation.terminal_msg_id() != Some(msg_id.as_str()) {
                    match turn_handle.begin_wire_segment(msg_id.clone()) {
                        Ok(segment) => turn_cancellation = segment,
                        Err(_) => {
                            durable_completion = Some((
                                false,
                                None,
                                Some("Agent turn was cancelled before continuation admission".to_owned()),
                            ));
                            final_turn_writeback = None;
                            break;
                        }
                    }
                }
                if continuation_count >= MAX_CRON_CONTINUATIONS_PER_TURN {
                    warn!(
                        conversation_id = %conv_id,
                        max = MAX_CRON_CONTINUATIONS_PER_TURN,
                        "Reached cron continuation limit; ending turn early"
                    );
                    break;
                }

                let turn_msg_id = msg_id.clone();
                let mut relay = StreamRelay::new(
                    conv_id.clone(),
                    msg_id,
                    user_id_owned.clone(),
                    Arc::clone(&repo),
                    Arc::clone(&user_events),
                    cron_service.clone(),
                )
                .with_turn_completion(false)
                .with_cancellation(turn_cancellation.clone())
                .with_companion_context(companion, companion_id.clone())
                .with_origin(origin.clone())
                .with_channel_platform(channel_platform.clone());

                // Execution-attempt turns: let the relay accumulate this turn's
                // token usage into the conversation's running total, consumed by
                // the owning attempt after settle. No-op for every other conversation.
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
                let send_cancellation = turn_token.clone();
                // Phase 3: keep a copy of this turn's send so a pre-response
                // provider fault can resend the SAME content to the next model.
                let resend_payload = current_send.clone();
                let (send_error_tx, send_error_rx) = oneshot::channel();
                // 1. Send the message to the agent and concurrently run the relay to stream events.
                tokio::spawn(async move {
                    if send_cancellation.is_cancelled() {
                        let _ = send_error_tx.send(Ok(()));
                        return;
                    }
                    let send_result = send_agent.send_message(current_send).await;
                    if let Err(e) = send_result.as_ref() {
                        error!(conversation_id = %conv_id_send, error = %ErrorChain(e), "Agent send_message failed");
                    }
                    // Explicit success matters: a dropped sender now denotes
                    // panic/abort and is converted by StreamRelay into a
                    // terminal Error instead of waiting forever.
                    let _ = send_error_tx.send(send_result);
                });
                // 2. Wait for the agent to process the message and complete the turn, while the relay streams events in real time.
                let outcome = relay.consume_with_send_error(rx, send_error_rx).await;
                let post_terminal_deadline =
                    tokio::time::Instant::now() + POST_TERMINAL_TRANSITION_GRACE;

                if turn_token.is_cancelled() || outcome.stop_reason == Some(TurnStopReason::Cancelled) {
                    durable_completion = Some((
                        false,
                        outcome.final_text.clone(),
                        Some("Agent turn was cancelled".to_owned()),
                    ));
                    final_turn_writeback = None;
                    break;
                }

                if outcome.terminal.code()
                    == Some(nomifun_api_types::AgentErrorCode::NomifunStreamBroken)
                {
                    // A permanent manager relay may have exited, or a lagged
                    // broadcast may have skipped the real terminal while the
                    // producer is still running. Remove and await the cached
                    // runtime before releasing exact-turn admission; ordinary
                    // provider errors intentionally do not enter this branch.
                    if tokio::time::timeout(
                        CANCEL_TEARDOWN_GRACE,
                        runtime_registry.terminate_and_wait(
                            &conv_id,
                            Some(AgentKillReason::AgentErrorRecovery),
                        ),
                    )
                    .await
                    .is_err()
                    {
                        warn!(conversation_id = %conv_id, "Broken event-stream runtime teardown exceeded its hard bound");
                    }
                    durable_completion = Some((
                        false,
                        outcome.final_text.clone(),
                        Some("Agent event stream integrity was lost".to_owned()),
                    ));
                    final_turn_writeback = None;
                    break;
                }

                if let Some(session_key) = agent.get_session_key() {
                    if tokio::time::timeout_at(
                        post_terminal_deadline,
                        persist_session_key(&repo, &conv_id, &session_key),
                    )
                    .await
                    .is_err()
                    {
                        warn!(conversation_id = %conv_id, "Post-terminal session persistence exceeded the shared hard bound");
                        durable_completion = Some((
                            false,
                            outcome.final_text.clone(),
                            Some("Post-terminal transition timed out during session persistence".to_owned()),
                        ));
                        final_turn_writeback = None;
                        break;
                    }
                }
                if turn_token.is_cancelled() {
                    durable_completion = Some((
                        false,
                        outcome.final_text.clone(),
                        Some("Agent turn was cancelled during session persistence".to_owned()),
                    ));
                    final_turn_writeback = None;
                    break;
                }

                // Phase 3 (plan D3): model failover. Only fires on a pre-response
                // nomi provider fault, bounded by min(max_switches, queue.len()).
                // On a usable next model we swap `agent` to the rebuilt task and
                // resend the SAME content with a fresh msg_id; on None (queue
                // exhausted / disabled / not eligible) we fall through to the
                // ACP-eviction + error-surfacing path unchanged. This runs BEFORE
                // `evict_acp_task_after_terminal_error` (which only acts on ACP),
                // so a successful nomi failover short-circuits via `continue`.
                let failover_switch = match tokio::time::timeout_at(
                    post_terminal_deadline,
                    service.maybe_failover_in_send_loop(
                        &conv_id,
                        agent.agent_type(),
                        &outcome,
                        failover_switches_done,
                        &failover_tried,
                        &failover_extra_json,
                        &runtime_registry,
                        turn_cancellation.turn_id(),
                        &turn_token,
                    ),
                )
                .await
                {
                    Ok(switch) => switch,
                    Err(_) => {
                        warn!(conversation_id = %conv_id, "Post-terminal model failover exceeded the shared hard bound");
                        durable_completion = Some((
                            false,
                            outcome.final_text.clone(),
                            Some("Post-terminal transition timed out during model failover".to_owned()),
                        ));
                        final_turn_writeback = None;
                        break;
                    }
                };
                if turn_token.is_cancelled() {
                    // Any runtime constructed by failover belongs to the same
                    // still-blocked generation; the stop worker has already
                    // tombstoned it, so never schedule a resend.
                    durable_completion = Some((
                        false,
                        outcome.final_text.clone(),
                        Some("Agent turn was cancelled during model failover".to_owned()),
                    ));
                    final_turn_writeback = None;
                    break;
                }
                if let Some(switch) = failover_switch {
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
                    let rebuilt = match tokio::time::timeout_at(
                        post_terminal_deadline,
                        service.strip_images_and_rebuild(
                            &conv_id,
                            &runtime_registry,
                            turn_cancellation.turn_id(),
                            &turn_token,
                        ),
                    )
                    .await
                    {
                        Ok(rebuilt) => rebuilt,
                        Err(_) => {
                            warn!(conversation_id = %conv_id, "Post-terminal image fallback exceeded the shared hard bound");
                            durable_completion = Some((
                                false,
                                outcome.final_text.clone(),
                                Some("Post-terminal transition timed out during image fallback".to_owned()),
                            ));
                            final_turn_writeback = None;
                            break;
                        }
                    };
                    if let Some(rebuilt) = rebuilt {
                        if turn_token.is_cancelled() {
                            let _ = runtime_registry.cancel_runtime_turn(
                                &conv_id,
                                turn_cancellation.turn_id(),
                                Some(AgentKillReason::UserCancelled),
                            );
                            durable_completion = Some((
                                false,
                                outcome.final_text.clone(),
                                Some("Agent turn was cancelled during image fallback".to_owned()),
                            ));
                            break;
                        }
                        if tokio::time::timeout_at(
                            post_terminal_deadline,
                            service.persist_images_stripped_tip(&conv_id),
                        )
                        .await
                        .is_err()
                        {
                            warn!(conversation_id = %conv_id, "Post-terminal image fallback tip persistence exceeded the shared hard bound");
                            durable_completion = Some((
                                false,
                                outcome.final_text.clone(),
                                Some("Post-terminal transition timed out after image fallback".to_owned()),
                            ));
                            final_turn_writeback = None;
                            break;
                        }
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

                if turn_token.is_cancelled() {
                    durable_completion = Some((
                        false,
                        outcome.final_text.clone(),
                        Some("Agent turn was cancelled during fallback evaluation".to_owned()),
                    ));
                    final_turn_writeback = None;
                    break;
                }

                // review #1/#5: the relay SUPPRESSED a pre-response provider error
                // expecting a failover, but the failover did NOT fire above (picker
                // exhausted at runtime / disabled / rebuild failed). Re-surface the
                // ORIGINAL error so the user is not left with a silently-dropped
                // turn — preserves the "queue-exhausted → original error" invariant.
                if let Some(suppressed) = outcome.suppressed_error.as_ref() {
                    let surface_relay = StreamRelay::new(
                        conv_id.clone(),
                        turn_msg_id.clone(),
                        user_id_owned.clone(),
                        Arc::clone(&repo),
                        Arc::clone(&user_events),
                        cron_service.clone(),
                    )
                    .with_companion_context(companion, companion_id.clone())
                    .with_origin(origin.clone())
                    .with_channel_platform(channel_platform.clone());
                    if tokio::time::timeout_at(
                        post_terminal_deadline,
                        surface_relay.surface_terminal_error(suppressed, &turn_cancellation),
                    )
                    .await
                    .is_err()
                    {
                        warn!(conversation_id = %conv_id, "Post-terminal error surfacing exceeded the shared hard bound");
                        durable_completion = Some((
                            false,
                            outcome.final_text.clone(),
                            Some("Post-terminal transition timed out while surfacing the terminal error".to_owned()),
                        ));
                        final_turn_writeback = None;
                        break;
                    }
                }

                let result_ok = matches!(outcome.terminal, RelayTerminal::Finish)
                    && outcome
                        .final_text
                        .as_deref()
                        .is_some_and(|text| !text.trim().is_empty());
                durable_completion = Some((
                    result_ok,
                    outcome.final_text.clone(),
                    (!matches!(outcome.terminal, RelayTerminal::Finish))
                        .then(|| format!("{:?}", outcome.terminal)),
                ));

                let acp_evicted = match tokio::time::timeout_at(
                    post_terminal_deadline,
                    service.evict_acp_task_after_terminal_error(
                        &conv_id,
                        agent.agent_type(),
                        &outcome,
                        &runtime_registry,
                        turn_cancellation.turn_id(),
                        &turn_token,
                    ),
                )
                .await
                {
                    Ok(evicted) => evicted,
                    Err(_) => {
                        warn!(conversation_id = %conv_id, "Post-terminal ACP eviction exceeded the shared hard bound");
                        durable_completion = Some((
                            false,
                            outcome.final_text.clone(),
                            Some("Post-terminal transition timed out during ACP cleanup".to_owned()),
                        ));
                        final_turn_writeback = None;
                        break;
                    }
                };
                if acp_evicted {
                    break;
                }
                if turn_token.is_cancelled() {
                    durable_completion = Some((
                        false,
                        outcome.final_text.clone(),
                        Some("Agent turn was cancelled during terminal cleanup".to_owned()),
                    ));
                    final_turn_writeback = None;
                    break;
                }

                if outcome.system_responses.is_empty() {
                    if matches!(outcome.terminal, RelayTerminal::Finish)
                        && let Some(final_text) = outcome.final_text.clone()
                        && let Some(final_text_msg_id) = outcome.final_text_msg_id.clone()
                        && let Some((knowledge_service, request)) = service.build_turn_writeback_request(
                            &knowledge_extra,
                            &conv_id,
                            &turn_msg_id,
                            &turn_user_text,
                            turn_origin.as_deref(),
                            agent.agent_type(),
                            companion,
                            channel_platform.as_deref(),
                        )
                    {
                        final_turn_writeback = Some((knowledge_service, request, final_text_msg_id, final_text));
                    }
                    break;
                }
                if turn_token.is_cancelled() {
                    durable_completion = Some((
                        false,
                        outcome.final_text.clone(),
                        Some("Agent turn was cancelled before continuation".to_owned()),
                    ));
                    final_turn_writeback = None;
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

            let (ok, text, error) = durable_completion.unwrap_or_else(|| {
                (
                    false,
                    None,
                    Some("Agent turn ended without a terminal relay outcome".to_owned()),
                )
            });
            Self::complete_delivery_receipt_before_release(
                &repo,
                &user_id_owned,
                &conversation_key,
                durable_operation_id.as_deref(),
                ok,
                text.as_deref(),
                error.as_deref(),
                &turn_token,
                durable_guard.as_ref().map(|(key, generation)| {
                    (&service.durable_operations_in_flight, key.as_str(), *generation)
                }),
            )
            .await;

            // The model turn is finished at this point. Release the exact turn
            // and publish authoritative idle before optional knowledge
            // write-back: post-processing must never keep the conversation
            // visibly busy or block the next user turn.
            service
                .release_and_complete_turn(
                    &mut turn_handle,
                    &user_id_owned,
                    &conv_id,
                    &stable_turn_id,
                    companion,
                    companion_id,
                    origin,
                    channel_platform,
                )
                .await;

            if !turn_token.is_cancelled()
                && let Some((knowledge_service, request, msg_id, final_text)) = final_turn_writeback
            {
                let writeback = AssertUnwindSafe(run_turn_writeback_report(
                    knowledge_service,
                    request,
                    Arc::clone(&repo),
                    Arc::clone(&user_events),
                    // The helper owns Strings because progress callbacks may
                    // outlive individual knowledge-service awaits.
                    // Cloning here keeps the exact turn owner in charge until
                    // the bounded report returns or is cancelled.
                    user_id_owned.clone(),
                    conv_id.clone(),
                    msg_id,
                    final_text,
                ))
                .catch_unwind();
                tokio::select! {
                    biased;
                    _ = turn_token.cancelled() => {
                        info!(conversation_id = %conv_id, "Post-turn write-back cancelled");
                    }
                    result = tokio::time::timeout(TURN_WRITEBACK_GRACE, writeback) => {
                        match result {
                            Ok(Ok(())) => {}
                            Ok(Err(_)) => {
                                warn!(conversation_id = %conv_id, "Post-turn write-back panicked");
                            }
                            Err(_) => {
                                warn!(conversation_id = %conv_id, "Turn write-back exceeded the hard bound");
                            }
                        }
                    }
                }
            }
            })
            .catch_unwind()
            .await;

            if owner_result.is_ok() {
                owner_receipt_guard.disarm();
            } else if turn_handle.is_cancelled() {
                // Explicit AbortHandle cancellation drops this whole outer
                // future and never reaches here. This branch covers a panic
                // racing a user stop; the stop worker owns Cancelled terminal,
                // exact release, and completion.
                warn!(conversation_id = %panic_conversation_id, "Turn owner panicked while cancellation was already active");
            } else {
                error!(conversation_id = %panic_conversation_id, "Turn owner task panicked; running bounded failure finalizer");
                let panic_cancellation = turn_handle.turn_cancellation();
                let terminal_msg_id = panic_cancellation
                    .terminal_msg_id()
                    .or(panic_cancellation.wire_turn_id())
                    .map(str::to_owned)
                    .unwrap_or_else(|| panic_stable_turn_id.clone());
                let panic_event = nomifun_ai_agent::AgentStreamEvent::Error(
                    nomifun_ai_agent::protocol::events::ErrorEventData::legacy(
                        "Agent turn task exited unexpectedly",
                        None,
                    ),
                );
                let relay = StreamRelay::new(
                    panic_conversation_id.clone(),
                    terminal_msg_id,
                    panic_user_id.clone(),
                    Arc::clone(&service.conversation_repo),
                    Arc::clone(&service.user_events),
                    None,
                )
                .with_companion_context(
                    panic_wire_context.companion,
                    panic_wire_context.companion_id.clone(),
                )
                .with_origin(panic_wire_context.origin.clone())
                .with_channel_platform(panic_wire_context.channel_platform.clone());
                relay
                    .surface_terminal_error(&panic_event, &panic_cancellation)
                    .await;

                let _ = panic_runtime_registry.cancel_runtime_turn(
                    &panic_conversation_id,
                    panic_cancellation.turn_id(),
                    Some(AgentKillReason::AgentErrorRecovery),
                );
                if tokio::time::timeout(
                    CANCEL_TEARDOWN_GRACE,
                    panic_runtime_registry.terminate_and_wait(
                        &panic_conversation_id,
                        Some(AgentKillReason::AgentErrorRecovery),
                    ),
                )
                .await
                .is_err()
                {
                    warn!(conversation_id = %panic_conversation_id, "Panicked turn runtime teardown exceeded its hard bound");
                }
                service
                    .release_and_complete_turn(
                        &mut turn_handle,
                        &panic_user_id,
                        &panic_conversation_id,
                        &panic_stable_turn_id,
                        panic_wire_context.companion,
                        panic_wire_context.companion_id,
                        panic_wire_context.origin,
                        panic_wire_context.channel_platform,
                    )
                    .await;
            }
        });
        let owner_abort_handle = owner_task.abort_handle();
        drop(owner_task);
        self.runtime_state.register_turn_owner_abort_handle(
            &owner_conversation_id,
            owner_turn_generation,
            owner_abort_handle,
        );

        info!(
            msg_id = %user_msg_id_ret,
            elapsed_ms = now_ms().saturating_sub(send_started_at),
            "Message accepted, agent work scheduled"
        );
        Ok(user_msg_id_ret)
    }

    /// Trusted idempotent steering boundary for a previously persisted
    /// execution effect.  Unlike the public `steer_message`, this never falls
    /// back to starting a new turn: if the original turn is unavailable the
    /// caller keeps its durable intent pending for recovery/audit.
    pub(crate) async fn steer_message_idempotent(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<String, AppError> {
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }
        if !req.files.is_empty() {
            return Err(AppError::BadRequest(
                "steer_unsupported: attachments must be sent as a new turn".into(),
            ));
        }
        let operation_id = operation_id.trim();
        if operation_id.is_empty() {
            return Err(AppError::BadRequest(
                "internal steer operation id must not be empty".to_owned(),
            ));
        }
        let conv_id = parse_conv_id(conversation_id)?;
        let row = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;
        let request_payload = serde_json::json!({
            "content": &req.content,
            "hidden": req.hidden,
            "origin": &req.origin,
            "channel_platform": &req.channel_platform,
        })
        .to_string();
        let receipt = self
            .conversation_repo
            .claim_delivery_receipt(
                user_id,
                conv_id,
                operation_id,
                "steer",
                &request_payload,
                now_ms(),
            )
            .await?;
        let message_id = receipt.message_id.clone();
        if receipt.status == "completed" {
            return Ok(message_id);
        }
        if let Some(existing) = self.conversation_repo.get_message(conv_id, &message_id).await? {
            let expected = serde_json::json!({ "content": &req.content }).to_string();
            if existing.r#type != "text"
                || existing.position.as_deref() != Some("right")
                || existing.content != expected
            {
                return Err(AppError::Conflict(
                    "internal steer operation id was reused with different content".to_owned(),
                ));
            }
            let completed = self.conversation_repo
                .complete_delivery_receipt(
                    user_id,
                    conv_id,
                    operation_id,
                    true,
                    None,
                    None,
                    now_ms(),
                )
                .await?;
            if !completed {
                return Err(AppError::Internal(
                    "failed to acknowledge idempotent steer receipt".to_owned(),
                ));
            }
            return Ok(message_id);
        }
        let instance = runtime_registry.get_runtime(conversation_id).ok_or_else(|| {
            AppError::Conflict("the Agent turn to steer is no longer active".to_owned())
        })?;
        if instance.status() != Some(ConversationStatus::Running) {
            return Err(AppError::Conflict(
                "the Agent turn to steer is no longer running".to_owned(),
            ));
        }
        match instance.steer(req.content.clone()) {
            Ok(true) => {}
            Ok(false) => {
                return Err(AppError::Conflict(
                    "the Agent turn ended before the steer was delivered".to_owned(),
                ));
            }
            Err(error) => return Err(error),
        }

        let message = nomifun_db::models::MessageRow {
            id: message_id.clone(),
            conversation_id: conv_id.to_owned(),
            msg_id: Some(message_id.clone()),
            r#type: "text".into(),
            content: serde_json::json!({ "content": &req.content }).to_string(),
            position: Some("right".into()),
            status: Some("finish".into()),
            hidden: req.hidden,
            created_at: now_ms(),
        };
        self.conversation_repo.insert_message(&message).await?;
        let completed = self.conversation_repo
            .complete_delivery_receipt(
                user_id,
                conv_id,
                operation_id,
                true,
                None,
                None,
                now_ms(),
            )
            .await?;
        if !completed {
            return Err(AppError::Internal(
                "failed to acknowledge idempotent steer receipt".to_owned(),
            ));
        }
        let (companion, companion_id, extra_channel_platform) =
            companion_context_from_extra(&row.extra)?;
        self.user_events.send_to_user(
            user_id,
            WebSocketMessage::new(
                "message.userCreated",
                serde_json::json!({
                "conversation_id": conv_id,
                "msg_id": &message_id,
                "content": &req.content,
                "position": "right",
                "status": "finish",
                "hidden": req.hidden,
                "origin": req.origin,
                "companion": companion,
                "companion_id": companion_id,
                "channel_platform": req.channel_platform.or(extra_channel_platform),
                "created_at": message.created_at,
                }),
            ),
        );
        Ok(message_id)
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
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<String, AppError> {
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }
        let conv_id = parse_conv_id(conversation_id)?;
        let runtime_build_lease = self.begin_public_runtime_build(conv_id, user_id)?;

        // Verify conversation exists and belongs to user.
        let row = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;
        runtime_build_lease.ensure_active()?;

        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;
        runtime_build_lease.ensure_active()?;

        // No live turn → nothing to steer; send normally (new turn).
        let Some(instance) = runtime_registry.get_runtime(conversation_id) else {
            return self
                .send_message_with_runtime_build_lease(
                    user_id,
                    conversation_id,
                    req,
                    runtime_registry,
                    runtime_build_lease,
                )
                .await;
        };
        if instance.status() != Some(ConversationStatus::Running) {
            return self
                .send_message_with_runtime_build_lease(
                    user_id,
                    conversation_id,
                    req,
                    runtime_registry,
                    runtime_build_lease,
                )
                .await;
        }

        // The steering inbox is text-only. Silently dropping attachments here
        // loses the user's explicit selection (and, for images, defeats the
        // multimodal turn). Surface the existing steer_unsupported marker so
        // the desktop queues the complete payload as the next normal turn.
        if !req.files.is_empty() {
            return Err(AppError::BadRequest(
                "steer_unsupported: attachments must be sent as a new turn".into(),
            ));
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
                return self
                    .send_message_with_runtime_build_lease(
                        user_id,
                        conversation_id,
                        req,
                        runtime_registry,
                        runtime_build_lease,
                    )
                    .await;
            }
            Err(e) => return Err(e),
        }

        // Steered successfully — persist the interjection as a normal user
        // message (transcript shows it at the point it was sent), same shape as
        // `send_message`.
        let user_msg_id = Self::mint_msg_id();
        let user_msg = nomifun_db::models::MessageRow {
            id: user_msg_id.clone(),
            conversation_id: conv_id.to_owned(),
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
        let (companion, companion_id, _) = companion_context_from_extra(&row.extra)?;
        self.user_events.send_to_user(
            user_id,
            WebSocketMessage::new(
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
            ),
        );

        Ok(user_msg_id)
    }

    async fn persist_and_broadcast_send_failure_tip(
        &self,
        user_id: &str,
        conversation_id: &str,
        err: &AppError,
    ) {
        let Some(row) = self.persist_send_failure_tip(conversation_id, err).await else {
            return;
        };

        let msg_id = row.msg_id.clone().unwrap_or_else(|| row.id.clone());
        let content_value: serde_json::Value =
            serde_json::from_str(&row.content).unwrap_or_else(|_| serde_json::Value::String(row.content.clone()));
        self.user_events.send_to_user(
            user_id,
            WebSocketMessage::new(
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
            ),
        );
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
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<String, AppError> {
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }
        let conv_id = parse_conv_id(conversation_id)?;
        let message_id = parse_message_id(message_id)?;
        let runtime_build_lease = self.begin_public_runtime_build(conv_id, user_id)?;

        // 1. 归属校验
        let row = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;
        runtime_build_lease.ensure_active()?;

        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;
        runtime_build_lease.ensure_active()?;

        // 2. 仅 Nomi
        if row.r#type != "nomi" {
            return Err(AppError::BadRequest(
                "Edit & resubmit is only supported for Nomi conversations".into(),
            ));
        }

        // 3. 目标必须是最近一条用户(right/text)消息（保证引擎"回退最后一个 turn"
        //    与 DB"删除该条及其后"对齐）。
        let recent = self.conversation_repo.get_messages_keyset(conv_id, None, 50).await?;
        runtime_build_lease.ensure_active()?;
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
        let agent = self.runtime_handle(conversation_id)?;
        agent.rewind_last_turn().await?;
        runtime_build_lease.ensure_active()?;

        // 5. 截断 DB：删除目标(含)及其后所有消息。
        self.conversation_repo
            .delete_messages_from(conv_id, from_created_at, &from_id)
            .await?;
        runtime_build_lease.ensure_active()?;

        // 6. 复用正常发送：重新插入用户消息行 + 起新 turn。
        self.send_message_with_runtime_build_lease(
            user_id,
            conversation_id,
            req,
            runtime_registry,
            runtime_build_lease,
        )
        .await
    }

    /// Stop the current streaming response for a conversation.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn cancel(
        &self,
        user_id: &str,
        conversation_id: &str,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(), AppError> {
        self.cancel_with_origin(
            user_id,
            conversation_id,
            runtime_registry,
            CancelOrigin::User,
        )
        .await
    }

    /// Cancel an Agent Execution attempt without classifying infrastructure
    /// cleanup (pause/replan/recovery/cancel) as a direct user stop.
    pub async fn cancel_for_execution(
        &self,
        user_id: &str,
        conversation_id: &str,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(), AppError> {
        self.cancel_with_origin(
            user_id,
            conversation_id,
            runtime_registry,
            CancelOrigin::AgentExecution,
        )
        .await
    }

    /// Start an independent, generation-scoped stop worker. Once spawned it
    /// continues even if the HTTP request future is disconnected/dropped.
    /// Admission remains closed until the captured runtime's teardown has
    /// completed or hit the hard bound, then the exact turn generation is
    /// released and (for an ordinary stop) completed on the wire.
    fn spawn_turn_stop_cleanup(
        &self,
        user_id: String,
        conversation_id: String,
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
        publish_completion: bool,
        deletion_owned: bool,
    ) -> oneshot::Receiver<Result<(), AppError>> {
        let (result_tx, result_rx) = oneshot::channel();
        let stop_admission = if deletion_owned {
            self.runtime_state
                .begin_conversation_stop_for_deletion(&conversation_id)
        } else {
            self.runtime_state.begin_conversation_stop(&conversation_id)
        };
        let stop_guard = match stop_admission {
            Ok(Some(guard)) => guard,
            Ok(None) => {
                // Either normal completion or the single-flight stop leader
                // already owns release -> finished -> completed
                // linearization. Followers only join; they never add another
                // fence owner, repeat teardown, or compete to publish idle.
                let runtime_state = Arc::clone(&self.runtime_state);
                tokio::spawn(async move {
                    let completed = runtime_state
                        .wait_for_cleanup_fences(&conversation_id, CANCEL_TEARDOWN_GRACE)
                        .await;
                    let result = if completed {
                        Ok(())
                    } else {
                        Err(AppError::Timeout(
                            "conversation completion did not finish within the stop join window"
                                .to_owned(),
                        ))
                    };
                    let _ = result_tx.send(result);
                });
                return result_rx;
            }
            Err(err) => {
                let _ = result_tx.send(Err(err));
                return result_rx;
            }
        };
        let cancelled_build_ids = stop_guard.cancelled_build_ids().to_vec();
        let turn_cancellation = self.runtime_state.begin_turn_cancellation(&conversation_id);
        // Capture before removing the registry slot. This is the exact old
        // process whose teardown must be awaited before admission reopens.
        let captured_agent = runtime_registry.get_runtime(&conversation_id);
        let service = self.clone();

        tokio::spawn(async move {
            // Keep admission closed for the worker's entire lifecycle,
            // including idle-runtime teardown where no turn release gate exists.
            let mut stop_guard = Some(stop_guard);
            let mut completion_to_publish: Option<(Option<String>, TurnWireContext)> = None;
            let result = async {
                if let Some(turn_cancellation) = turn_cancellation {
                    let mut exact_release_guard = service
                        .runtime_state
                        .arm_cancelled_turn_release(turn_cancellation.clone());
                    // Tombstone/remove the old runtime slot before signalling
                    // the turn token. No later conversation-ID termination is
                    // allowed after exact ownership is released.
                    if let Err(err) = runtime_registry.cancel_runtime_turn(
                        &conversation_id,
                        turn_cancellation.turn_id(),
                        Some(AgentKillReason::UserCancelled),
                    ) {
                        warn!(
                            conversation_id,
                            turn_id = turn_cancellation.turn_id(),
                            error = %ErrorChain(&err),
                            "Failed to initiate cancelled runtime teardown"
                        );
                    }
                    turn_cancellation.cancel();

                    let build_cleanup = async {
                        if !service
                            .runtime_state
                            .wait_for_runtime_builds(
                                &conversation_id,
                                &cancelled_build_ids,
                                CANCEL_TEARDOWN_GRACE,
                            )
                            .await
                        {
                            warn!(conversation_id, "Cancelled runtime preparation leases exceeded the hard bound");
                        }
                        service.runtime_state.forget_cancelled_runtime_builds(
                            &conversation_id,
                            &cancelled_build_ids,
                        );
                    };

                    let terminal_wait = turn_cancellation
                        .wait_for_terminal_observed(CANCEL_RELEASE_GRACE);
                    let owner_wait = turn_cancellation
                        .wait_for_owner_quiesced(CANCEL_TEARDOWN_GRACE);
                    let registry_barrier = runtime_registry.terminate_and_wait(
                        &conversation_id,
                        Some(AgentKillReason::UserCancelled),
                    );
                    let captured_teardown = async {
                        if let Some(agent) = captured_agent {
                            agent
                                .kill_and_wait(Some(AgentKillReason::UserCancelled))
                                .await;
                        }
                    };
                    let teardown = async {
                        tokio::join!(registry_barrier, captured_teardown);
                    };
                    let (_, terminal_seen, owner_quiesced, teardown_result) = tokio::join!(
                        build_cleanup,
                        terminal_wait,
                        owner_wait,
                        tokio::time::timeout(CANCEL_TEARDOWN_GRACE, teardown),
                    );
                    if teardown_result.is_err() {
                        warn!(
                            conversation_id,
                            turn_id = turn_cancellation.turn_id(),
                            "Cancelled runtime teardown exceeded the hard bound; force-kill was already triggered"
                        );
                    }
                    if !owner_quiesced {
                        warn!(
                            conversation_id,
                            turn_id = turn_cancellation.turn_id(),
                            "Cancelled turn owner did not quiesce; aborting the exact owner task"
                        );
                        turn_cancellation.abort_owner_task();
                        if !turn_cancellation
                            .wait_for_owner_quiesced(CANCEL_COMPLETION_GRACE)
                            .await
                        {
                            warn!(
                                conversation_id,
                                turn_id = turn_cancellation.turn_id(),
                                "Cancelled turn owner did not drop after exact-task abort"
                            );
                        }
                    }

                    let wire_turn_id = turn_cancellation.wire_turn_id().map(str::to_owned);
                    let terminal_msg_id = turn_cancellation.terminal_msg_id().map(str::to_owned);
                    let wire_context = turn_cancellation.wire_context().clone();
                    if !terminal_seen
                        && let Some(terminal_msg_id) = terminal_msg_id.as_ref()
                    {
                        let relay = StreamRelay::new(
                            conversation_id.clone(),
                            terminal_msg_id.clone(),
                            user_id.clone(),
                            Arc::clone(&service.conversation_repo),
                            Arc::clone(&service.user_events),
                            None,
                        )
                        .with_companion_context(
                            wire_context.companion,
                            wire_context.companion_id.clone(),
                        )
                        .with_origin(wire_context.origin.clone())
                        .with_channel_platform(wire_context.channel_platform.clone());
                        relay.surface_cancelled_turn(&turn_cancellation);
                    }

                    let released = service
                        .runtime_state
                        .force_release_cancelled_turn(&turn_cancellation);
                    exact_release_guard.disarm();
                    if released
                        && publish_completion
                        && let Some(wire_turn_id) = wire_turn_id.as_deref()
                    {
                        StreamRelay::persist_conversation_finished(
                            &service.conversation_repo,
                            &conversation_id,
                        )
                        .await;
                        completion_to_publish =
                            Some((Some(wire_turn_id.to_owned()), wire_context));
                    }
                } else {
                    // Idle runtime stop is still a real teardown. Removing the
                    // slot first also clears any uninitialized slot left by a
                    // construction that raced without an admitted turn.
                    let _ = runtime_registry.terminate(
                        &conversation_id,
                        Some(AgentKillReason::UserCancelled),
                    );
                    let build_cleanup = async {
                        if !service
                            .runtime_state
                            .wait_for_runtime_builds(
                                &conversation_id,
                                &cancelled_build_ids,
                                CANCEL_TEARDOWN_GRACE,
                            )
                            .await
                        {
                            warn!(conversation_id, "Cancelled runtime preparation leases exceeded the hard bound");
                        }
                        service.runtime_state.forget_cancelled_runtime_builds(
                            &conversation_id,
                            &cancelled_build_ids,
                        );
                    };
                    let registry_barrier = runtime_registry.terminate_and_wait(
                        &conversation_id,
                        Some(AgentKillReason::UserCancelled),
                    );
                    let captured_teardown = async {
                        if let Some(agent) = captured_agent {
                            agent
                                .kill_and_wait(Some(AgentKillReason::UserCancelled))
                                .await;
                        }
                    };
                    let (_, teardown_result) = tokio::join!(
                        build_cleanup,
                        tokio::time::timeout(CANCEL_TEARDOWN_GRACE, async {
                            tokio::join!(registry_barrier, captured_teardown);
                        }),
                    );
                    if teardown_result.is_err()
                    {
                        warn!(
                            conversation_id,
                            "Idle runtime teardown exceeded the hard bound"
                        );
                    }
                    if publish_completion {
                        // A stop can legitimately find no active turn while a
                        // stale/half-built runtime slot still reports Running.
                        // Other clients still need an authoritative idle
                        // transition; completion-fence conflicts were rejected
                        // by stop admission, so this cannot overwrite a live
                        // normal completion.
                        StreamRelay::persist_conversation_finished(
                            &service.conversation_repo,
                            &conversation_id,
                        )
                        .await;
                        completion_to_publish = Some((None, TurnWireContext::default()));
                    }
                }
                Ok(())
            }
            .await;
            if result.is_ok()
                && let Some((wire_turn_id, wire_context)) = completion_to_publish
            {
                let user_events = Arc::clone(&service.user_events);
                let runtime = service.final_completion_runtime(&conversation_id);
                let owned_stop_guard = stop_guard.take();
                let event_user_id = user_id.clone();
                let event_conversation_id = conversation_id.clone();
                if !service
                    .runtime_state
                    .linearize_cleanup_event(
                        &conversation_id,
                        1,
                        0,
                        CANCEL_TEARDOWN_GRACE,
                        move || {
                            StreamRelay::broadcast_turn_completed_with_context(
                                &user_events,
                                &event_user_id,
                                &event_conversation_id,
                                wire_turn_id,
                                Some(runtime),
                                wire_context.companion,
                                wire_context.companion_id,
                                wire_context.origin,
                                wire_context.channel_platform,
                            );
                            drop(owned_stop_guard);
                        },
                    )
                    .await
                {
                    warn!(conversation_id, "Cancelled completion event withheld while another cleanup fence remained active");
                }
            }
            // `send_to_user` above is synchronous enqueue. Drop the exact stop
            // fence immediately afterward so a client reacting to the event
            // cannot race a still-held admission block.
            drop(stop_guard);
            let _ = result_tx.send(result);
        });

        result_rx
    }

    async fn cancel_with_origin(
        &self,
        user_id: &str,
        conversation_id: &str,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        origin: CancelOrigin,
    ) -> Result<(), AppError> {
        let conversation_key = parse_conv_id(conversation_id)?;
        let mut user_cancel_preflight = None;
        let in_memory_authority = if origin == CancelOrigin::User {
            let authorization = self
                .runtime_state
                .authorize_in_memory_user_cancel(conversation_id, user_id)?;
            user_cancel_preflight = authorization.preflight_guard;
            authorization.authority
        } else if self
            .runtime_state
            .active_turn_allows_cancel(conversation_id, user_id, false)
        {
            InMemoryCancelAuthority::ActiveTurn
        } else {
            InMemoryCancelAuthority::None
        };

        if let InMemoryCancelAuthority::PublicBuilds(cancelled_build_ids) =
            &in_memory_authority
        {
                // This authority is intentionally narrower than a conversation
                // stop: an unverified public preparation may cancel only work
                // initiated by the same authenticated requester, never a
                // private/durable build or an unrelated idle runtime.
                self.note_user_cancel(conversation_id);
                self.runtime_state.forget_cancelled_runtime_builds(
                    conversation_id,
                    cancelled_build_ids,
                );
                return Ok(());
        }

        if in_memory_authority != InMemoryCancelAuthority::ActiveTurn {
            // Idle/cold-runtime stops still need repository authorization, but
            // it is hard-bounded. A live turn takes the secure cached-owner
            // fast path above, so a wedged DB actor cannot make its stop button
            // ineffective.
            let conversation = tokio::time::timeout(
                CANCEL_AUTH_PREFLIGHT_GRACE,
                self.conversation_repo.get(conversation_key),
            )
            .await
            .map_err(|_| {
                AppError::Timeout(
                    "conversation stop authorization exceeded its hard bound".to_owned(),
                )
            })??
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

            // A user stops or decides Attempt work through Agent Execution.
            // The internal cleanup path keeps using AgentExecution origin and
            // may cancel retained attempt runtimes.
            if origin == CancelOrigin::User {
                tokio::time::timeout(
                    CANCEL_AUTH_PREFLIGHT_GRACE,
                    self.ensure_not_retained_execution_attempt(user_id, &conversation.id),
                )
                .await
                .map_err(|_| {
                    AppError::Timeout(
                        "conversation stop retention check exceeded its hard bound".to_owned(),
                    )
                })??;
            }
        }

        // Record the user's intent BEFORE touching the agent: even when no
        // Agent is live (turn-acquired-but-not-yet-injected AutoWork window), the
        // stamp tells the owning execution flow this work was deliberately stopped.
        if origin == CancelOrigin::User {
            self.note_user_cancel(conversation_id);
        }

        let result_rx = self.spawn_turn_stop_cleanup(
            user_id.to_owned(),
            conversation_id.to_owned(),
            Arc::clone(runtime_registry),
            true,
            false,
        );
        // `spawn_turn_stop_cleanup` synchronously establishes the stronger
        // conversation stop tombstone before returning. Releasing the
        // requester-scoped preflight here is therefore an atomic fence handoff:
        // same-user public builds saw either the preflight or the stop, never a
        // gap between them.
        drop(user_cancel_preflight);
        let stop_result = match tokio::time::timeout(CANCEL_HANDLER_GRACE, result_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AppError::Internal(
                "conversation stop worker exited before reporting completion".to_owned(),
            )),
            Err(_) => {
                // Admission is already fenced and the detached owner has hard
                // bounds for every teardown phase. Treat the stop as accepted
                // instead of making the UI believe manual termination failed
                // merely because cleanup outlived the HTTP window.
                warn!(conversation_id, "Conversation stop accepted; bounded cleanup continues in the background");
                Ok(())
            }
        };
        stop_result?;
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
    ///     [`AgentRuntimeHandle::clear_context`]; and
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
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(), AppError> {
        let conv_id = parse_conv_id(conversation_id)?;
        // Verify conversation exists and belongs to user.
        self.conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;

        // 1. Reset the live agent's context, if one is running.
        if let Some(agent) = runtime_registry.get_runtime(conversation_id) {
            agent.clear_context().await?;
        } else {
            info!("No active agent; clearing persisted state only");
        }

        // 2. Forget the persisted ACP session so a rebuild does not resume the
        //    old session. Returns false (no-op) for non-ACP conversations.
        if let Err(e) = self.acp_session_repo.clear_session_id(conv_id).await {
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
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(), AppError> {
        let conv_id = parse_conv_id(conversation_id)?;
        // Verify existence and ownership (same pattern as clear_context).
        self.conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;

        // 1. Delete the persisted transcript and artifacts (NOT status).
        self.conversation_repo.delete_messages_by_conversation(conv_id).await?;
        self.conversation_repo.delete_artifacts_by_conversation(conv_id).await?;

        // 2. Reset the live agent's context, if one is running (same as
        //    clear_context: ACP rotates to a fresh session, Nomi empties its
        //    engine, etc.).
        if let Some(agent) = runtime_registry.get_runtime(conversation_id) {
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

    /// Pre-initialize an Agent runtime for a conversation (warmup).
    ///
    /// This builds the Agent runtime without sending a message, so the
    /// first real message can be processed faster.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn warmup(
        &self,
        user_id: &str,
        conversation_id: &str,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(), AppError> {
        let lease = self.begin_public_runtime_build(conversation_id, user_id)?;
        let preparation_token = lease.cancellation_token();
        let row = self
            .conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;
        lease.ensure_active()?;

        self.ensure_not_retained_execution_attempt(user_id, &row.id)
            .await?;
        lease.ensure_active()?;

        let mut runtime_options = self.build_runtime_options(&row)?;
        self.ensure_auto_workspace_skill_links(&row, &runtime_options).await?;
        lease.ensure_active()?;
        self.apply_knowledge_mounts(
            &row,
            &mut runtime_options,
            Some(&preparation_token),
        )
        .await;
        lease.ensure_active()?;
        let stored_workspace = runtime_options.workspace.clone();
        let agent = runtime_registry
            .get_or_create_runtime_for_turn(
                conversation_id,
                lease.id(),
                preparation_token.clone(),
                runtime_options,
            )
            .await?;
        if lease.is_cancelled() {
            let _ = agent.kill(Some(AgentKillReason::UserCancelled));
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} warmup was cancelled"
            )));
        }

        // Persist auto-resolved workspace if factory picked a different path.
        self.maybe_persist_workspace(conversation_id, &stored_workspace, agent.workspace())
            .await?;
        lease.ensure_active()?;

        debug!("Agent warmed up");
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelOrigin {
    User,
    AgentExecution,
}

// ── Internal Helpers ────────────────────────────────────────────────

impl ConversationService {
    /// Build [`AgentRuntimeBuildOptions`] from a conversation database row.
    ///
    /// Provider/model resolution lives in [`crate::runtime_options::provider_model_from_conversation_row`]
    /// so the cron executor can derive identical values for the same row.
    /// Diverging the lookup here historically produced
    /// `Provider '<vendor>' not found` failures under cron when the
    /// interactive path worked fine (Sentry ELECTRON-1HM).
    pub(crate) fn build_runtime_options(&self, row: &nomifun_db::models::ConversationRow) -> Result<AgentRuntimeBuildOptions, AppError> {
        let agent_type = string_to_enum(&row.r#type)?;

        let model = crate::runtime_options::provider_model_from_conversation_row(row)?;
        if agent_type == AgentType::Nomi && model.is_none() {
            return Err(AppError::BadRequest(
                "Nomi conversation has no provider/model configured".to_owned(),
            ));
        }
        let delegation_policy = crate::runtime_options::delegation_policy_from_conversation_row(row)?;

        let mut extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|e| AppError::Internal(format!("Invalid extra JSON: {e}")))?;

        if !self.execution_authority(&row.user_id).controls_host() {
            // Even a row written outside the service cannot smuggle a custom
            // workspace, prompt-side capability config or installation binding
            // into execution. Preserve only the server-minted managed-workspace
            // identity; dropping it would force the factory to invent or reuse
            // an unowned fallback path.
            let temp_workspace_id = require_temp_workspace_id(&extra, &row.id)?;
            extra = serde_json::json!({
                TEMP_WORKSPACE_ID_EXTRA_KEY: temp_workspace_id,
            });
        }

        // A canonical temp-workspace token is the authoritative marker for a
        // backend-managed workspace. Recompute its absolute path beneath the
        // current installation root and ignore a persisted absolute path from a
        // source installation after restore/import. Rows without that marker
        // are explicit custom workspaces and retain their validated path.
        let workspace = if temp_workspace_marker_present(&extra) {
            let workspace =
                auto_workspace_path_for_row(&self.workspace_root, row, &agent_type, &extra)?;
            let workspace = workspace.to_string_lossy().into_owned();
            extra["workspace"] = serde_json::Value::String(workspace.clone());
            workspace
        } else {
            match extra.get("workspace").and_then(|v| v.as_str()) {
                Some(workspace) if !workspace.is_empty() => {
                    let normalized = validate_runtime_workspace_path(workspace)?;
                    if normalized != workspace {
                        extra["workspace"] =
                            serde_json::Value::String(normalized.clone());
                    }
                    normalized
                }
                _ => {
                    return Err(AppError::Internal(format!(
                        "conversation {} has neither a custom workspace nor a canonical temp_workspace_id",
                        row.id
                    )));
                }
            }
        };

        Ok(AgentRuntimeBuildOptions {
            user_id: row.user_id.clone(),
            agent_type,
            workspace,
            model,
            conversation_id: row.id.to_string(),
            delegation_policy,
            extra,
            // Stamp/validate the nomi session against this conversation instance.
            conversation_created_at: Some(row.created_at),
        })
    }

    async fn ensure_auto_workspace_skill_links(
        &self,
        row: &ConversationRow,
        runtime_options: &AgentRuntimeBuildOptions,
    ) -> Result<(), AppError> {
        if !self.execution_authority(&row.user_id).controls_host() {
            return Ok(());
        }
        if !temp_workspace_marker_present(&runtime_options.extra) {
            return Ok(());
        }
        let expected_workspace = auto_workspace_path_for_row(
            &self.workspace_root,
            row,
            &runtime_options.agent_type,
            &runtime_options.extra,
        )?;

        let stored_workspace = runtime_options.workspace.trim();
        let workspace = if stored_workspace.is_empty() {
            expected_workspace
        } else {
            let workspace = PathBuf::from(stored_workspace);
            if workspace != expected_workspace {
                return Err(AppError::Internal(format!(
                    "conversation {} resolved a managed workspace outside the current work root",
                    row.id
                )));
            }
            workspace
        };

        let skill_names = runtime_options
            .extra
            .get("skills")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default();
        if skill_names.is_empty() {
            return Ok(());
        }

        let Some(rel_dirs) = native_skills_dirs(
            &self.agent_metadata_repo,
            &runtime_options.agent_type,
            runtime_options.extra.get("backend"),
        )
        .await
        else {
            return Ok(());
        };
        if rel_dirs.is_empty() {
            return Ok(());
        }

        let resolved = self.skill_resolver.resolve_skills(&skill_names).await;
        if resolved.is_empty() {
            return Ok(());
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
        Ok(())
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
    /// fails Agent runtime creation — mount errors degrade to warnings.
    ///
    /// Binding target selection (spec §3 ruling 6 / §4.5): a conversation
    /// whose `extra.companion_id` is a non-blank string mounts the companion-level
    /// binding `('companion', companion_id)`; everything else keeps the per-conversation
    /// binding `('conversation', conversation_id)`. No merge between the two.
    async fn apply_knowledge_mounts(
        &self,
        row: &ConversationRow,
        runtime_options: &mut AgentRuntimeBuildOptions,
        cancellation: Option<&CancellationToken>,
    ) {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return;
        }
        // Knowledge roots are installation-owned filesystem resources.  A
        // model-only user must never reach workpath/companion binding lookup or
        // create a physical mount before the Agent factory applies its own
        // ceiling.
        if !self.execution_authority(&row.user_id).controls_host() {
            return;
        }
        let service = self.knowledge_service.read().ok().and_then(|guard| guard.clone());
        let Some(service) = service else { return };

        let workspace = PathBuf::from(runtime_options.workspace.trim());

        let (target_kind, target_id) = match knowledge_binding_target(
            &runtime_options.extra,
            &runtime_options.conversation_id,
        ) {
            Ok(target) => target,
            Err(error) => {
                warn!(
                    conversation_id = %runtime_options.conversation_id,
                    error = %error,
                    "refusing to mount knowledge for invalid persisted identity"
                );
                return;
            }
        };
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
        // Companion sessions keep their `('companion', companion_id)` binding unchanged — they
        // are not per-workspace.
        let preset_binding = runtime_options
            .extra
            .get("preset_knowledge_binding")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let outcome = if target_kind == "conversation" && !preset_binding {
            let wp_key = nomifun_knowledge::session_workpath_key(&workspace, &self.workspace_root);
            service
                .ensure_mounts_for_session(&wp_key, target_kind, &target_id, &workspace)
                .await
        } else {
            service
                .ensure_mounts_for_target(target_kind, &target_id, &workspace)
                .await
        };
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return;
        }

        // Recycle the cached agent when the resolved knowledge context changed
        // since it was last built. The agent bakes the retrieval-protocol
        // section into its prompt at build time and is cached per conversation
        // (`get_or_create_runtime` is a per-conversation `OnceCell`), so a
        // `挂载知识库` toggle on an already-warmed/used session would otherwise
        // never reach the running agent — the freshly-resolved mounts here would
        // be discarded by the cache. That silently breaks the UI's promise that
        // a binding change "takes effect on the next message" (the reported bug:
        // KB enabled mid-session → task dispatched → retrieval never triggers).
        // Terminating the in-memory runtime lets the imminent `get_or_create_runtime`
        // rebuild with the new mounts; the conversation and any persisted ACP
        // session are preserved (the rebuilt ACP agent resumes and re-delivers
        // the section via the knowledge prelude hook).
        let conversation_id = runtime_options.conversation_id.clone();
        let new_signature = knowledge_mounts_signature(&outcome);
        let signature_changed =
            self.runtime_state.knowledge_signature(&conversation_id).as_deref() != Some(new_signature.as_str());
        if signature_changed {
            match self.runtime_registry.get_runtime(&conversation_id) {
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
                    if cancellation.is_some_and(CancellationToken::is_cancelled) {
                        return;
                    }
                    info!(
                        conversation_id = %conversation_id,
                        "knowledge binding changed; recycling cached agent so the new mounts take effect on the next message"
                    );
                    if let Err(e) = self
                        .runtime_registry
                        .terminate(&conversation_id, Some(AgentKillReason::KnowledgeBindingChanged))
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

        let Some(obj) = runtime_options.extra.as_object_mut() else { return };
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

    fn build_turn_writeback_request(
        &self,
        extra: &serde_json::Value,
        conversation_id: &str,
        msg_id: &str,
        user_text: &str,
        origin: Option<&str>,
        agent_type: AgentType,
        companion: bool,
        channel_platform: Option<&str>,
    ) -> Option<(
        Arc<nomifun_knowledge::KnowledgeService>,
        nomifun_knowledge::TurnWritebackRequest,
    )> {
        if origin.map(str::trim).filter(|s| !s.is_empty()).is_some() {
            return None;
        }
        if user_text.trim().is_empty() {
            return None;
        }

        let service = self.knowledge_service.read().ok().and_then(|guard| guard.clone())?;
        let mounts: Vec<KnowledgeMountInfo> =
            serde_json::from_value(extra.get("knowledge_mounts")?.clone()).ok()?;
        if mounts.is_empty() {
            return None;
        }

        let writeback = extra
            .get("knowledge_writeback")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !writeback {
            return None;
        }

        let writeback_mode = extra
            .get("knowledge_writeback_mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("staged")
            .to_owned();
        let writeback_eagerness = extra
            .get("knowledge_writeback_eagerness")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("conservative")
            .to_owned();
        let channel_write_enabled = extra
            .get("knowledge_channel_write_enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let surface = if companion {
            nomifun_knowledge::WriteSurface::Companion
        } else if channel_platform.map(str::trim).filter(|s| !s.is_empty()).is_some() {
            nomifun_knowledge::WriteSurface::ExternalChannel
        } else if agent_type == AgentType::Acp {
            nomifun_knowledge::WriteSurface::TerminalAcp
        } else {
            nomifun_knowledge::WriteSurface::RegularChat
        };
        let conversation_scope = conversation_id.trim_matches('/');
        let turn_scope = msg_id.trim_matches('/');
        let scope = if turn_scope.is_empty() {
            conversation_scope.to_owned()
        } else {
            format!("{conversation_scope}/{turn_scope}")
        };
        let request = nomifun_knowledge::TurnWritebackRequest {
            mounts: mounts.clone(),
            binding: nomifun_knowledge::KnowledgeBinding {
                enabled: true,
                writeback,
                writeback_mode,
                writeback_eagerness,
                channel_write_enabled,
                kb_ids: mounts.iter().map(|m| m.id.clone()).collect(),
                ..Default::default()
            },
            surface,
            scope,
            user_text: user_text.to_owned(),
            assistant_text: String::new(),
        };

        Some((service, request))
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
        user_id: &str,
        conversation_id: &str,
        action: &str,
        source: Option<&ConversationSource>,
    ) {
        let payload = serde_json::json!({
            "conversation_id": conversation_id,
            "action": action,
            "source": source,
        });
        let event = WebSocketMessage::new("conversation.listChanged", payload);
        self.user_events.send_to_user(user_id, event);
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
    async fn backfill_extra_inplace(&self, conversation_id: &str, extra: &mut serde_json::Value) {
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

fn allocate_temp_workspace_id(workspace_root: &Path, label: &str) -> (String, PathBuf) {
    loop {
        let temp_workspace_id = generate_prefixed_id("ws");
        let path = auto_temp_workspace_path(workspace_root, label, &temp_workspace_id);
        if !path.exists() {
            return (temp_workspace_id, path);
        }
    }
}

fn auto_temp_workspace_path(workspace_root: &Path, label: &str, temp_workspace_id: &str) -> PathBuf {
    workspace_root
        .join("conversations")
        .join(format!("{label}-temp-{temp_workspace_id}"))
}

fn temp_workspace_id_from_extra(extra: &serde_json::Value) -> Option<&str> {
    extra.get(TEMP_WORKSPACE_ID_EXTRA_KEY)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn temp_workspace_marker_present(extra: &serde_json::Value) -> bool {
    extra
        .as_object()
        .is_some_and(|object| object.contains_key(TEMP_WORKSPACE_ID_EXTRA_KEY))
}

fn require_temp_workspace_id<'a>(
    extra: &'a serde_json::Value,
    conversation_id: &str,
) -> Result<&'a str, AppError> {
    let value = temp_workspace_id_from_extra(extra).ok_or_else(|| {
        AppError::Internal(format!(
            "conversation {conversation_id} has no canonical temp_workspace_id for its managed workspace"
        ))
    })?;
    validate_prefixed_id(value, "ws").map_err(|error| {
        AppError::Internal(format!(
            "conversation {conversation_id} has invalid temp_workspace_id '{value}': {error}"
        ))
    })?;
    Ok(value)
}

fn auto_workspace_path_for_row(
    workspace_root: &Path,
    row: &ConversationRow,
    agent_type: &AgentType,
    extra: &serde_json::Value,
) -> Result<PathBuf, AppError> {
    let label = conversation_label(agent_type, extra.get("backend"));
    let temp_workspace_id = require_temp_workspace_id(extra, &row.id)?;
    Ok(auto_temp_workspace_path(
        workspace_root,
        &label,
        temp_workspace_id,
    ))
}

fn managed_temp_workspace_path_from_row(workspace_root: &Path, row: &ConversationRow) -> Option<PathBuf> {
    let extra: serde_json::Value = serde_json::from_str(&row.extra).ok()?;
    let temp_workspace_id = require_temp_workspace_id(&extra, &row.id).ok()?;
    let agent_type: AgentType = string_to_enum(&row.r#type).ok()?;
    let label = conversation_label(&agent_type, extra.get("backend"));
    let expected = auto_temp_workspace_path(workspace_root, &label, temp_workspace_id);
    Some(expected)
}

fn rebase_managed_workspace_in_row(
    row: &mut ConversationRow,
    workspace_root: &Path,
) -> Result<(), AppError> {
    let mut extra: serde_json::Value = serde_json::from_str(&row.extra)
        .map_err(|error| AppError::Internal(format!("Invalid extra JSON: {error}")))?;
    if !temp_workspace_marker_present(&extra) {
        return Ok(());
    }
    let agent_type: AgentType = string_to_enum(&row.r#type)?;
    let workspace =
        auto_workspace_path_for_row(workspace_root, row, &agent_type, &extra)?;
    extra["workspace"] =
        serde_json::Value::String(workspace.to_string_lossy().into_owned());
    row.extra = serde_json::to_string(&extra)
        .map_err(|error| AppError::Internal(format!("Failed to serialize extra: {error}")))?;
    Ok(())
}

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

/// Execution preferences and execution identity are typed columns/relations,
/// never open-ended Agent factory data. Rejecting these keys prevents clients
/// from recreating the retired dual-source contract after migration 037 has
/// removed it from every existing row.
fn reject_execution_policy_extra_keys(extra: &serde_json::Value) -> Result<(), AppError> {
    let Some(object) = extra.as_object() else {
        return Ok(());
    };
    let forbidden = object.keys().find(|key| {
        matches!(
            key.as_str(),
            "delegation_policy"
                | "execution_model_pool"
                | "decision_policy"
                | "execution_template_id"
                | "agent_cluster_mode"
                | "team_id"
                | "teamId"
        ) || key.starts_with("orchestrator_")
    });

    match forbidden {
        Some(key) => Err(AppError::BadRequest(format!(
            "`extra.{key}` is retired; use the typed conversation execution fields"
        ))),
        None => Ok(()),
    }
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
    let id = parse_message_id(id)
        .map_err(|_| AppError::BadRequest(format!("invalid message cursor id: {cursor}")))?;
    Ok((created_at, id.to_owned()))
}

/// Parse the companion-companion wire markers from a conversation row's `extra`
/// JSON string. Present identifiers must satisfy their canonical contracts;
/// malformed persisted values are never reinterpreted as absence.
///
/// These markers ride on `message.userCreated` / `message.stream` /
/// `turn.completed` broadcasts so downstream consumers (the companion memory
/// collector, the companion window's remote-turn bubble) can recognize companion
/// conversations — including Channel Agent sessions that never register in
/// the companion-side thread table — straight off the wire.
fn companion_context_from_extra(
    extra: &str,
) -> Result<(bool, Option<CompanionId>, Option<String>), AppError> {
    let value: serde_json::Value = serde_json::from_str(extra)
        .map_err(|error| AppError::Internal(format!("invalid conversation extra JSON: {error}")))?;
    let companion = value
        .get("companion_session")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let companion_id = match value.get("companion_id") {
        None => None,
        Some(value) => {
            let raw = value.as_str().ok_or_else(|| {
                AppError::Internal("persisted companion_id must be a string".to_owned())
            })?;
            Some(
                CompanionId::parse(raw)
                    .map_err(|error| {
                        AppError::Internal(format!("invalid persisted companion_id: {error}"))
                    })?,
            )
        }
    };
    let channel_platform = match value.get("channel_platform") {
        None => None,
        Some(value) => {
            let raw = value.as_str().ok_or_else(|| {
                AppError::Internal("persisted channel_platform must be a string".to_owned())
            })?;
            if raw.is_empty() || raw.trim() != raw {
                return Err(AppError::Internal(
                    "persisted channel_platform must be a non-empty trimmed natural key"
                        .to_owned(),
                ));
            }
            Some(raw.to_owned())
        }
    };
    Ok((companion, companion_id, channel_platform))
}

/// Decide which knowledge-binding target a conversation mounts from
/// (spec §3 ruling 6 / §4.5).
///
/// A conversation whose `extra.companion_id` is present routes to the
/// companion-level binding `("companion", companion_id)` — companion sessions and channel
/// master sessions of a companion share its knowledge. Missing means the
/// per-conversation binding `("conversation", conversation_id)`; malformed
/// identity data is rejected rather than reinterpreted as absence.
/// No merge semantics: exactly one target applies.
fn knowledge_binding_target<'a>(
    extra: &'a serde_json::Value,
    conversation_id: &'a str,
) -> Result<(&'static str, &'a str), AppError> {
    ConversationId::parse(conversation_id)
        .map_err(|error| AppError::BadRequest(format!("invalid conversation_id: {error}")))?;
    match extra.get("companion_id") {
        Some(value) => {
            let companion_id = value.as_str().ok_or_else(|| {
                AppError::BadRequest("companion_id must be a canonical string ID".to_owned())
            })?;
            CompanionId::parse(companion_id)
                .map_err(|error| AppError::BadRequest(format!("invalid companion_id: {error}")))?;
            Ok(("companion", companion_id))
        }
        None => Ok(("conversation", conversation_id)),
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
struct PresetLineage<'a> {
    agent_type: &'a str,
    preset_id: &'a str,
    custom_agent_id: &'a str,
    agent_id: &'a str,
    agent_name: &'a str,
    backend: &'a str,
    current_model_id: &'a str,
    session_mode: &'a str,
}

impl<'a> PresetLineage<'a> {
    fn from_response_and_extra(response: &'a ConversationResponse, extra: &'a serde_json::Value) -> Self {
        fn s<'a>(extra: &'a serde_json::Value, key: &str) -> &'a str {
            extra.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
        }
        Self {
            agent_type: response.r#type.serde_name(),
            preset_id: s(extra, "preset_id"),
            custom_agent_id: s(extra, "custom_agent_id"),
            agent_id: s(extra, "agent_id"),
            agent_name: s(extra, "agent_name"),
            backend: s(extra, "backend"),
            current_model_id: s(extra, "current_model_id"),
            session_mode: s(extra, "session_mode"),
        }
    }

    fn has_any_identity(&self) -> bool {
        !self.preset_id.is_empty()
            || !self.custom_agent_id.is_empty()
            || !self.agent_id.is_empty()
            || !self.agent_name.is_empty()
    }
}

fn log_conversation_created(response: &ConversationResponse, extra: &serde_json::Value) {
    let lineage = PresetLineage::from_response_and_extra(response, extra);
    if lineage.has_any_identity() {
        info!(
            conversation_id = %response.id,
            agent_type = lineage.agent_type,
            preset_id = lineage.preset_id,
            custom_agent_id = lineage.custom_agent_id,
            agent_id = lineage.agent_id,
            agent_name = lineage.agent_name,
            backend = lineage.backend,
            current_model_id = lineage.current_model_id,
            session_mode = lineage.session_mode,
            "Conversation created from preset"
        );
    } else {
        info!(
            conversation_id = %response.id,
            agent_type = lineage.agent_type,
            "Conversation created (no preset)"
        );
    }
}

fn is_tool_message_type(message_type: MessageType) -> bool {
    matches!(
        message_type,
        MessageType::ToolCall | MessageType::ToolGroup | MessageType::AcpToolCall
    )
}

/// Parse the optional per-conversation MCP-server selection. Invalid input
/// is rejected instead of silently becoming an empty selection.
fn parse_selected_mcp_server_ids(
    obj: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<Option<Vec<McpServerId>>, AppError> {
    let Some(value) = obj.remove("selected_mcp_server_ids") else {
        return Ok(None);
    };
    let serde_json::Value::Array(items) = value else {
        return Err(AppError::BadRequest(
            "selected_mcp_server_ids must be an array of canonical MCP server IDs".into(),
        ));
    };
    let ids = items
        .into_iter()
        .enumerate()
        .map(|(index, value)| match value {
            serde_json::Value::String(id) => McpServerId::parse(id).map_err(|error| {
                AppError::BadRequest(format!(
                    "selected_mcp_server_ids[{index}] is invalid: {error}"
                ))
            }),
            _ => Err(AppError::BadRequest(format!(
                "selected_mcp_server_ids[{index}] must be a string"
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PROVIDER_ID_1: &str = "prov_0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_ID_2: &str = "prov_0190f5fe-7c00-7a00-8000-000000000002";

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
    fn finite_conversation_model_pool_must_contain_the_lead() {
        let model = ProviderWithModel {
            provider_id: PROVIDER_ID_1.to_owned(),
            model: "model-1".to_owned(),
            use_model: Some("model-1".to_owned()),
        };
        let matching = ExecutionModelPool::Single {
            model: ExecutionModelRef {
                provider_id: PROVIDER_ID_1.to_owned(),
                model: "model-1".to_owned(),
            },
        };
        assert!(validate_conversation_model_authority(Some(&model), Some(&matching)).is_ok());

        let mismatched = ExecutionModelPool::Single {
            model: ExecutionModelRef {
                provider_id: PROVIDER_ID_2.to_owned(),
                model: "model-2".to_owned(),
            },
        };
        assert!(matches!(
            validate_conversation_model_authority(Some(&model), Some(&mismatched)),
            Err(AppError::BadRequest(_))
        ));
        assert!(
            validate_conversation_model_authority(
                Some(&model),
                Some(&ExecutionModelPool::Automatic),
            )
            .is_ok()
        );

        for blank_override in ["", "   "] {
            let fallback_model = ProviderWithModel {
                provider_id: PROVIDER_ID_1.to_owned(),
                model: "model-1".to_owned(),
                use_model: Some(blank_override.to_owned()),
            };
            assert!(matches!(
                validate_conversation_model_authority(
                    Some(&fallback_model),
                    Some(&matching),
                ),
                Err(AppError::BadRequest(_))
            ));
        }
    }

    #[test]
    fn parse_selected_mcp_ids_accepts_canonical_string_array() {
        let first = McpServerId::new();
        let second = McpServerId::new();
        let mut obj = json!({ "selected_mcp_server_ids": [first, second] })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            parse_selected_mcp_server_ids(&mut obj).unwrap(),
            Some(vec![first, second])
        );
    }

    #[test]
    fn parse_selected_mcp_ids_rejects_numbers_and_invalid_strings() {
        for value in [json!([1]), json!(["4"]), json!("not-an-array")] {
            let mut obj = json!({ "selected_mcp_server_ids": value })
                .as_object()
                .unwrap()
                .clone();
            assert!(matches!(
                parse_selected_mcp_server_ids(&mut obj),
                Err(AppError::BadRequest(_))
            ));
        }
    }

    #[test]
    fn parse_selected_mcp_ids_absent_is_none() {
        let mut obj = json!({ "workspace": "/p" }).as_object().unwrap().clone();
        assert_eq!(parse_selected_mcp_server_ids(&mut obj).unwrap(), None);
    }

    #[test]
    fn parse_selected_mcp_ids_empty_is_explicit_none_selected() {
        let mut obj = json!({ "selected_mcp_server_ids": [] }).as_object().unwrap().clone();
        assert_eq!(parse_selected_mcp_server_ids(&mut obj).unwrap(), Some(vec![]));
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
        let companion_id = "companion_0190f5fe-7c00-7a00-8abc-012345678901";
        let conversation_id = "conv_0190f5fe-7c00-7a00-8abc-012345678902";
        let extra = json!({"companion_id": companion_id});
        assert_eq!(
            knowledge_binding_target(&extra, conversation_id).unwrap(),
            ("companion", companion_id)
        );
    }

    #[test]
    fn knowledge_binding_target_rejects_untrimmed_companion_id() {
        let extra = json!({"companion_id": "  companion_0190f5fe-7c00-7a00-8abc-012345678901  "});
        assert!(knowledge_binding_target(
            &extra,
            "conv_0190f5fe-7c00-7a00-8abc-012345678902"
        )
        .is_err());
    }

    #[test]
    fn knowledge_binding_target_rejects_empty_companion_id() {
        let extra = json!({"companion_id": ""});
        assert!(knowledge_binding_target(
            &extra,
            "conv_0190f5fe-7c00-7a00-8abc-012345678902"
        )
        .is_err());
    }

    #[test]
    fn knowledge_binding_target_rejects_blank_companion_id() {
        let extra = json!({"companion_id": "   \t "});
        assert!(knowledge_binding_target(
            &extra,
            "conv_0190f5fe-7c00-7a00-8abc-012345678902"
        )
        .is_err());
    }

    #[test]
    fn knowledge_binding_target_missing_companion_id_falls_back() {
        let extra = json!({"workspace": "/tmp/ws"});
        let conversation_id = "conv_0190f5fe-7c00-7a00-8abc-012345678902";
        assert_eq!(
            knowledge_binding_target(&extra, conversation_id).unwrap(),
            ("conversation", conversation_id)
        );
    }

    #[test]
    fn knowledge_binding_target_non_object_extra_falls_back() {
        // build_runtime_options can yield a non-object extra only in degenerate
        // cases, but the helper must still not panic on them.
        let extra = serde_json::Value::Null;
        let conversation_id = "conv_0190f5fe-7c00-7a00-8abc-012345678902";
        assert_eq!(
            knowledge_binding_target(&extra, conversation_id).unwrap(),
            ("conversation", conversation_id)
        );
    }

    #[test]
    fn knowledge_binding_target_rejects_non_string_companion_id() {
        let extra = json!({"companion_id": 42});
        assert!(knowledge_binding_target(
            &extra,
            "conv_0190f5fe-7c00-7a00-8abc-012345678902"
        )
        .is_err());
    }

    fn response_with_type(agent_type: nomifun_common::AgentType) -> ConversationResponse {
        ConversationResponse {
            id: nomifun_common::ConversationId::new().into_string(),
            name: "test".into(),
            r#type: agent_type,
            model: None,
            status: ConversationStatus::Pending,
            runtime: None,
            source: None,
            pinned: false,
            pinned_at: None,
            channel_chat_id: None,
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            delegation_policy: Default::default(),
            execution_model_pool: None,
            decision_policy: Default::default(),
            execution_template_id: None,
            linked_execution_id: None,
            execution_step_id: None,
            execution_attempt_id: None,
            created_at: 0,
            modified_at: 0,
            extra: json!({}),
        }
    }

    #[test]
    fn preset_lineage_extracts_acp_builtin_fields() {
        use nomifun_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({
            "agent_id": "abc-123",
            "agent_name": "Claude Code",
            "backend": "claude",
            "current_model_id": "opus",
            "session_mode": "default",
        });
        let lineage = PresetLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "acp");
        assert_eq!(lineage.agent_id, "abc-123");
        assert_eq!(lineage.agent_name, "Claude Code");
        assert_eq!(lineage.backend, "claude");
        assert_eq!(lineage.current_model_id, "opus");
        assert_eq!(lineage.session_mode, "default");
        assert_eq!(lineage.preset_id, "");
        assert_eq!(lineage.custom_agent_id, "");
        assert!(lineage.has_any_identity());
    }

    #[test]
    fn preset_lineage_extracts_nomi_preset_id() {
        use nomifun_common::AgentType;
        let response = response_with_type(AgentType::Nomi);
        let extra = json!({ "preset_id": "preset-xyz" });
        let lineage = PresetLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "nomi");
        assert_eq!(lineage.preset_id, "preset-xyz");
        assert!(lineage.has_any_identity());
    }

    #[test]
    fn preset_lineage_extracts_acp_custom_agent_id() {
        use nomifun_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({
            "custom_agent_id": "custom-1",
            "backend": "openrouter",
        });
        let lineage = PresetLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "acp");
        assert_eq!(lineage.custom_agent_id, "custom-1");
        assert_eq!(lineage.backend, "openrouter");
        assert!(lineage.has_any_identity());
    }

    #[test]
    fn preset_lineage_no_identity_when_extra_lacks_assistant_fields() {
        use nomifun_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({ "workspace": "/project" });
        let lineage = PresetLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "acp");
        assert!(!lineage.has_any_identity());
    }

    #[test]
    fn preset_lineage_treats_non_string_fields_as_missing() {
        use nomifun_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({
            "agent_id": 42,
            "agent_name": null,
        });
        let lineage = PresetLineage::from_response_and_extra(&response, &extra);
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

    #[test]
    fn execution_policy_extra_keys_are_rejected() {
        for key in [
            "delegation_policy",
            "execution_model_pool",
            "decision_policy",
            "agent_cluster_mode",
            "orchestrator_legacy_identity",
            "orchestrator_role",
        ] {
            let mut object = serde_json::Map::new();
            object.insert(key.to_owned(), json!("value"));
            let extra = serde_json::Value::Object(object);
            assert!(
                reject_execution_policy_extra_keys(&extra).is_err(),
                "{key} must not recreate the retired extra contract"
            );
        }
    }

    #[test]
    fn ordinary_agent_extra_remains_allowed() {
        assert!(
            reject_execution_policy_extra_keys(&json!({
                "workspace": "/project",
                "skills": ["pdf"]
            }))
            .is_ok()
        );
    }
}
