use std::path::{Path, PathBuf};
use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use std::time::Duration;

use nomifun_ai_agent::artifact_store::ArtifactStore;
use nomifun_ai_agent::protocol::events::AgentStreamEvent;
use nomifun_ai_agent::types::{AgentRuntimeBuildOptions, SendMessageData};
use nomifun_ai_agent::{AgentRuntimeHandle, AgentRuntimeRegistry, TurnStopReason};
use futures_util::FutureExt;
use sha2::{Digest, Sha256};
use std::panic::AssertUnwindSafe;

use crate::response_middleware::ICronService;
use crate::runtime_state::{
    AgentTurnCancellation, AgentTurnHandle, ConversationDeletionGuard,
    ConversationPreparationGuard, ConversationRuntimeStateService, InMemoryCancelAuthority,
    RuntimeBuildLease, TurnWireContext,
};
use crate::ExecutionConversationBoundary;
use crate::orphan_recovery::{
    RunningOrphanDisposition, running_orphan_disposition,
};
use nomifun_api_types::{
    ApprovalCheckResponse, CloneConversationRequest, ConfirmRequest, ConfirmationListResponse,
    ConversationArtifactListResponse, ConversationArtifactResponse, ConversationListResponse,
    ConversationMcpStatus, ConversationMcpStatusKind,
    ConversationResponse, ConversationRuntimeSummary, CreateConversationRequest, KnowledgeMountInfo, ListConversationsQuery,
    ListMessagesQuery, McpServerId, MessageListResponse, MessageResponse, MessageSearchResponse, SearchMessagesQuery,
    ExecutionModelPool, ExecutionModelRef, ResolvedPresetSnapshot, SendMessageRequest, SessionMcpServer, SessionMcpTransport, UpdateConversationArtifactRequest,
    UpdateConversationRequest, WebSocketMessage,
};
use nomifun_common::{
    AgentExecutionTemplateId, AgentKillReason, AgentType, AppError, CompanionId, ConversationId, ConversationSource,
    ConversationStatus, CronJobId, DecisionPolicy, DelegationPolicy, ErrorChain, ExecutionAuthority, MessageId, MessageType, OnConversationDelete, PaginatedResult, ProviderId, ProviderWithModel,
    generate_id, now_ms, validate_uuidv7, workspace_path_has_edge_whitespace_segment,
};
use nomifun_db::models::{AgentMetadataRow, ConversationRow, MessageRow};
use nomifun_db::{
    AgentExecutionTurnAuthority, ConversationFilters, ConversationRowUpdate, CreateAcpSessionParams, IAcpSessionRepository,
    IAgentMetadataRepository, IConversationRepository, IMcpServerRepository, SaveRuntimeStateParams,
    ConversationTurnAdmissionState, RequirementConversationTurnAuthority, SortOrder,
    TurnLifecycleTransition, TurnReceiptCompletion,
};
use nomifun_mcp::{AcpMcpCapabilities, parse_acp_mcp_capabilities};
use nomifun_realtime::UserEventSink;
use nomifun_runtime::resolve_command_path;
use std::collections::{HashMap, HashSet};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::convert::{
    TOOL_CONTENT_COMPACT_THRESHOLD_BYTES, message_needs_artifact_history_audit,
    parse_provider_with_model, project_historical_artifact_integrity,
    row_to_artifact_response, row_to_message_response, row_to_message_response_compact,
    row_to_response, row_to_response_with_extra, search_row_to_item, string_to_enum,
};
use crate::skill_resolver::SkillResolver;
use crate::skill_snapshot::compute_initial_skills;
use crate::stream_relay::{
    RelayTerminal, StreamRelay, TurnWritebackAttempt, await_turn_writeback_quiesced,
    finish_turn_writeback_failure, reconcile_quiesced_writebacks_until_resolved,
    run_turn_writeback_report,
};
use std::sync::RwLock;

const MAX_CRON_CONTINUATIONS_PER_TURN: usize = 4;
const TEMP_WORKSPACE_ID_EXTRA_KEY: &str = "temp_workspace_id";
pub(crate) const PUBLIC_IDEMPOTENCY_KEY_MAX_BYTES: usize = 128;
const RECEIPT_FOREGROUND_BUDGET: Duration = Duration::from_secs(2);
/// Stop waits for relay/receipt cleanup, then generation-safely releases the
/// exact cancelled turn so the endpoint itself always remains bounded.
const CANCEL_RELEASE_GRACE: Duration = Duration::from_secs(3);
const CANCEL_TEARDOWN_GRACE: Duration = Duration::from_secs(7);
const CANCEL_HANDLER_GRACE: Duration = Duration::from_secs(11);
const CANCEL_AUTH_PREFLIGHT_GRACE: Duration = Duration::from_secs(2);
const KNOWLEDGE_AUTOGEN_MODEL_PREF_KEY: &str = "knowledge.autogenModel";
const DELETE_CORE_GRACE: Duration = Duration::from_secs(5);
const DELETE_CLEANUP_ITEM_GRACE: Duration = Duration::from_secs(5);
const TURN_WRITEBACK_CANCEL_GRACE: Duration = Duration::from_secs(10);

tokio::task_local! {
    static DELETED_CRON_JOB_IDS: Arc<[String]>;
}

/// Cron IDs deleted atomically with the Conversation currently being
/// dispatched to post-commit lifecycle hooks.
///
/// This is intentionally task-scoped: hooks cannot query the deleted database
/// rows, and a global handoff would permit cross-delete races or leaks.
pub fn current_deleted_cron_job_ids() -> Option<Arc<[String]>> {
    DELETED_CRON_JOB_IDS.try_with(Arc::clone).ok()
}

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
    /// `false` only for the atomic INSERT winner that was admitted to execute.
    /// Existing accepted/completed receipts and same-boot in-flight followers
    /// are absorbing replays and must not be awaited as newly-started work.
    pub replayed: bool,
    pub completed: bool,
    pub result_ok: Option<bool>,
    pub result_text: Option<String>,
    pub result_error: Option<String>,
}

/// Read-only durable state of one public keyed Conversation turn.
///
/// Background schedulers use this typed boundary after restart, when they
/// still own the opaque public key but no longer have (and must not recreate)
/// the historical request payload. The public key is namespaced internally;
/// callers never inspect repository status strings or operation-id formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicTurnDeliveryState {
    Missing,
    Accepted { message_id: String },
    Completed(IdempotentMessageDelivery),
}

/// Trusted runtime preparation applied only after a durable keyed turn has
/// won receiver-side admission.
///
/// Cron and AutoWork must not construct or mutate a cached runtime before
/// receipt + Running authority exists. They pass their fully resolved options
/// here so ConversationService can keep one preparation gate from exact claim
/// through local owner handoff and perform these mutations inside that owner.
#[async_trait::async_trait]
pub trait BackgroundTurnPreSendHook: Send + Sync {
    async fn prepare(&self) -> Result<(), AppError>;
}

pub struct BackgroundTurnRuntimePreparation {
    pub runtime_options: AgentRuntimeBuildOptions,
    pub desired_mode: Option<String>,
    pub clear_context: bool,
    pub pre_send_hook: Option<Arc<dyn BackgroundTurnPreSendHook>>,
}

/// Background keyed delivery plus an event subscription installed before the
/// owner sends the model prompt. Replays carry no runtime/subscription because
/// they never execute again.
pub struct ObservedIdempotentMessageDelivery {
    pub delivery: IdempotentMessageDelivery,
    pub runtime: Option<AgentRuntimeHandle>,
    pub events: Option<broadcast::Receiver<AgentStreamEvent>>,
}

/// Result of the process-start orphan sweep for one persisted Conversation.
///
/// Retained Agent Execution transcripts have a separate engine-owned recovery
/// protocol and are deliberately reported as a non-error skip. Every current
/// backend without durable, queryable proof that the exact prior process tree
/// is empty fails closed with [`AppError::Conflict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuiescentOrphanReconciliation {
    Reconciled,
    AlreadyTerminal,
    RetainedExecutionSkipped,
}

/// Typed outcome for an accepted background-delivery replay.
///
/// Consumers may wait for the durable receipt only for the first two outcomes.
/// The latter outcomes are durable quarantine decisions and must never be
/// converted into a retry that sends the prompt again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTurnReconciliationDisposition {
    LiveExactOwnerWait,
    ReconciledOrTerminalReRead,
    ExternalProofRequiredFailClosed,
    StaleConflict,
}

/// Stable identity of the exact live turn observed by IDMM.
///
/// The durable IDMM action reservation stores this scope and must present it
/// again when delivering a continuation or confirmation. A newer turn on the
/// same Conversation has a different generation and/or root wire identity, so
/// a delayed action can never be misdelivered to that replacement.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdmmTurnScope {
    pub wire_turn_id: String,
    pub generation: u64,
}

struct IdmmActiveTurnAuthority {
    _lease: RuntimeBuildLease,
    _preparation_guard: ConversationPreparationGuard,
    row: ConversationRow,
    active_turn: AgentTurnCancellation,
    runtime: AgentRuntimeHandle,
    scope: IdmmTurnScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExactActiveTurnAccess {
    OrdinaryConversation,
    AgentExecution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageSendAuthority {
    /// An owner-driven interactive send. It remains cancellable by that owner
    /// and may never mutate a retained Agent Execution transcript.
    OwnerInteractive,
    /// Trusted Agent Execution delivery. Its durable receipt is also its
    /// authority to address an execution-attempt transcript.
    TrustedInternal,
    /// The atomic edit/resubmit receipt owns a destructive rewind/truncate
    /// workflow and its replacement turn.
    EditResubmit,
}

impl MessageSendAuthority {
    fn public_cancellable(self) -> bool {
        matches!(self, Self::OwnerInteractive | Self::EditResubmit)
    }

    fn may_address_retained_execution(self) -> bool {
        matches!(self, Self::TrustedInternal)
    }
}

/// Validate the opaque public HTTP idempotency token without normalizing it.
///
/// Trimming would make two distinct wire values alias. Limiting keys to
/// non-whitespace visible ASCII keeps the durable operation namespace bounded,
/// log-safe, and identical across HTTP implementations on every platform.
pub(crate) fn validate_public_idempotency_key(value: &str) -> Result<(), AppError> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err(AppError::BadRequest(
            "Idempotency-Key must not be empty".to_owned(),
        ));
    }
    if bytes.len() > PUBLIC_IDEMPOTENCY_KEY_MAX_BYTES {
        return Err(AppError::BadRequest(format!(
            "Idempotency-Key must be at most {PUBLIC_IDEMPOTENCY_KEY_MAX_BYTES} bytes"
        )));
    }
    if !bytes.iter().all(|byte| (0x21..=0x7e).contains(byte)) {
        return Err(AppError::BadRequest(
            "Idempotency-Key must contain visible ASCII characters only".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct DurableOperationLease {
    message_id: String,
    generation: u64,
}

type DurableOperationGuards =
    Arc<std::sync::Mutex<std::collections::HashMap<String, DurableOperationLease>>>;
struct TurnWritebackRun {
    generation: u64,
    cancellation: CancellationToken,
}

static TURN_WRITEBACKS_IN_FLIGHT: LazyLock<
    std::sync::Mutex<HashMap<String, TurnWritebackRun>>,
> = LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
static NEXT_TURN_WRITEBACK_RUN_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Process-local ownership for one detached write-back attempt. It deliberately
/// has no durable queue semantics: after a process exit, history projection
/// marks the orphaned running state retryable and the user can start one fresh
/// attempt explicitly.
struct TurnWritebackRunGuard {
    key: String,
    generation: u64,
    cancellation: CancellationToken,
}

impl TurnWritebackRunGuard {
    fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

impl Drop for TurnWritebackRunGuard {
    fn drop(&mut self) {
        let mut in_flight = TURN_WRITEBACKS_IN_FLIGHT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if in_flight
            .get(&self.key)
            .is_some_and(|run| run.generation == self.generation)
        {
            in_flight.remove(&self.key);
        }
    }
}

const ADMISSION_CUSTODIAN_REQUEST_OWNER: u8 = 0;
const ADMISSION_CUSTODIAN_DETACHED_TURN_OWNER: u8 = 1;
const ADMISSION_CUSTODIAN_STOP_OWNER: u8 = 2;
const ADMISSION_CUSTODIAN_BACKGROUND_FINALIZER: u8 = 3;
const ADMISSION_CUSTODIAN_TERMINAL: u8 = 4;
const ADMISSION_CUSTODIAN_REPLAY_LOSER: u8 = 5;
const ADMISSION_CUSTODIAN_RUNNING: u8 = 6;
const ADMISSION_CUSTODIAN_UNCOMMITTED_CLAIM: u8 = 7;
const EDIT_CUSTODIAN_RESERVED: u8 = 0;
const EDIT_CUSTODIAN_ADMITTED: u8 = 1;
const EDIT_CUSTODIAN_DESTRUCTIVE_RUNTIME_MUTATION: u8 = 2;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicAdmissionCutpoint {
    AfterClaimCommit,
    BeforeOwnerSpawn,
    AfterWritebackRetryPreparationLease,
    AfterPublicPreparationCancelCaptured,
}

#[cfg(test)]
#[derive(Clone)]
struct PublicAdmissionCutpointControl {
    stage: PublicAdmissionCutpoint,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    panic: bool,
}

/// Cancellation-safe owner armed before the public atomic claim is awaited.
///
/// Its candidate message ID distinguishes the INSERT winner even when the
/// request future is dropped after SQLite committed but before the async return
/// value was delivered. Until a detached turn/stop/finalizer has genuinely
/// taken ownership, dropping this guard starts an independent exact abandon
/// loop and never guesses from aggregate status alone.
struct PublicTurnAdmissionCustodian {
    repo: Arc<dyn IConversationRepository>,
    user_id: String,
    conversation_id: String,
    operation_id: String,
    candidate_message_id: String,
    request_payload: String,
    expected_admitted_epoch: i64,
    operation_guards: DurableOperationGuards,
    guard_key: String,
    guard_generation: u64,
    owner: Arc<AtomicU8>,
}

impl PublicTurnAdmissionCustodian {
    fn owner(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.owner)
    }

    fn disarm_replay_loser(&self) {
        let _ = self.owner.compare_exchange(
            ADMISSION_CUSTODIAN_REQUEST_OWNER,
            ADMISSION_CUSTODIAN_REPLAY_LOSER,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn disarm_uncommitted_claim(&self) {
        let _ = self.owner.compare_exchange(
            ADMISSION_CUSTODIAN_REQUEST_OWNER,
            ADMISSION_CUSTODIAN_UNCOMMITTED_CLAIM,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

impl Drop for PublicTurnAdmissionCustodian {
    fn drop(&mut self) {
        if self
            .owner
            .compare_exchange(
                ADMISSION_CUSTODIAN_REQUEST_OWNER,
                ADMISSION_CUSTODIAN_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        ConversationService::continue_abandoned_public_turn_admission(
            Arc::clone(&self.repo),
            self.user_id.clone(),
            self.conversation_id.clone(),
            self.operation_id.clone(),
            self.candidate_message_id.clone(),
            self.request_payload.clone(),
            self.expected_admitted_epoch,
            (
                Arc::clone(&self.operation_guards),
                self.guard_key.clone(),
                self.guard_generation,
            ),
        );
    }
}

/// Cancellation-safe owner for the Agent Execution receipt/admission boundary.
///
/// Agent Execution has an additional typed attempt authority, so it cannot use
/// the public repository custodian. The same caller-minted candidate message ID
/// nevertheless proves whether a dropped claim future committed, and the
/// boundary's exact abandon command also validates the retained attempt lease.
struct ExecutionTurnAdmissionCustodian {
    boundary: Arc<dyn ExecutionConversationBoundary>,
    user_id: String,
    conversation_id: String,
    operation_id: String,
    candidate_message_id: String,
    request_payload: String,
    authority: AgentExecutionTurnAuthority,
    expected_admitted_epoch: i64,
    operation_guards: DurableOperationGuards,
    guard_key: String,
    guard_generation: u64,
    owner: Arc<AtomicU8>,
}

impl ExecutionTurnAdmissionCustodian {
    fn owner(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.owner)
    }

    fn disarm_replay_loser(&self) {
        let _ = self.owner.compare_exchange(
            ADMISSION_CUSTODIAN_REQUEST_OWNER,
            ADMISSION_CUSTODIAN_REPLAY_LOSER,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn disarm_uncommitted_claim(&self) {
        let _ = self.owner.compare_exchange(
            ADMISSION_CUSTODIAN_REQUEST_OWNER,
            ADMISSION_CUSTODIAN_UNCOMMITTED_CLAIM,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

impl Drop for ExecutionTurnAdmissionCustodian {
    fn drop(&mut self) {
        if self
            .owner
            .compare_exchange(
                ADMISSION_CUSTODIAN_REQUEST_OWNER,
                ADMISSION_CUSTODIAN_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        ConversationService::continue_abandoned_execution_turn_admission(
            Arc::clone(&self.boundary),
            self.user_id.clone(),
            self.conversation_id.clone(),
            self.operation_id.clone(),
            self.candidate_message_id.clone(),
            self.request_payload.clone(),
            self.authority.clone(),
            self.expected_admitted_epoch,
            (
                Arc::clone(&self.operation_guards),
                self.guard_key.clone(),
                self.guard_generation,
            ),
        );
    }
}

/// Cancellation-safe owner for the two-stage edit/resubmit admission.
///
/// Reservation is durable but not execution authority. A dropped reserve/admit
/// await therefore first attempts the exact reservation recovery and, if that
/// reports Stale, falls through to exact admitted-generation abandon. Once
/// rewind is invoked, runtime teardown plus persisted Nomi-session erasure must
/// be proven before the receipt/fence can be released.
struct EditResubmitAdmissionCustodian {
    repo: Arc<dyn IConversationRepository>,
    runtime_registry: Arc<dyn AgentRuntimeRegistry>,
    runtime_state: Arc<ConversationRuntimeStateService>,
    user_id: String,
    conversation_id: String,
    operation_id: String,
    candidate_message_id: String,
    request_payload: String,
    reserved_admission_epoch: i64,
    admitted_admission_epoch: i64,
    conversation_created_at: i64,
    operation_guards: DurableOperationGuards,
    guard_key: String,
    guard_generation: u64,
    owner: Arc<AtomicU8>,
    phase: Arc<AtomicU8>,
}

impl EditResubmitAdmissionCustodian {
    fn owner(&self) -> Arc<AtomicU8> {
        Arc::clone(&self.owner)
    }

    fn disarm_replay_loser(&self) {
        let _ = self.owner.compare_exchange(
            ADMISSION_CUSTODIAN_REQUEST_OWNER,
            ADMISSION_CUSTODIAN_REPLAY_LOSER,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn disarm_uncommitted_claim(&self) {
        let _ = self.owner.compare_exchange(
            ADMISSION_CUSTODIAN_REQUEST_OWNER,
            ADMISSION_CUSTODIAN_UNCOMMITTED_CLAIM,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn mark_admitted(&self) -> Result<(), AppError> {
        self.phase
            .compare_exchange(
                EDIT_CUSTODIAN_RESERVED,
                EDIT_CUSTODIAN_ADMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| {
                AppError::Conflict(
                    "edit/resubmit admission custodian lost reservation ownership".to_owned(),
                )
            })
    }

    fn mark_destructive_runtime_mutation(&self) -> Result<(), AppError> {
        self.phase
            .compare_exchange(
                EDIT_CUSTODIAN_ADMITTED,
                EDIT_CUSTODIAN_DESTRUCTIVE_RUNTIME_MUTATION,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| {
                AppError::Conflict(
                    "edit/resubmit admission custodian lost destructive preparation ownership"
                        .to_owned(),
                )
            })
    }

    fn destructive_runtime_mutation_started(&self) -> bool {
        self.phase.load(Ordering::Acquire) == EDIT_CUSTODIAN_DESTRUCTIVE_RUNTIME_MUTATION
    }
}

impl Drop for EditResubmitAdmissionCustodian {
    fn drop(&mut self) {
        if self
            .owner
            .compare_exchange(
                ADMISSION_CUSTODIAN_REQUEST_OWNER,
                ADMISSION_CUSTODIAN_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        ConversationService::continue_abandoned_edit_resubmit_admission(
            Arc::clone(&self.repo),
            Arc::clone(&self.runtime_registry),
            Arc::clone(&self.runtime_state),
            self.user_id.clone(),
            self.conversation_id.clone(),
            self.operation_id.clone(),
            self.candidate_message_id.clone(),
            self.request_payload.clone(),
            self.reserved_admission_epoch,
            self.admitted_admission_epoch,
            self.conversation_created_at,
            self.phase.load(Ordering::Acquire),
            (
                Arc::clone(&self.operation_guards),
                self.guard_key.clone(),
                self.guard_generation,
            ),
        );
    }
}

/// Ownership token passed from the synchronous idempotent admission path into
/// whichever path is responsible for completing the durable receipt.  The
/// explicit handoff prevents an outer `Err` handler from opening redelivery
/// while a bounded receipt retry was already detached in the background.
#[derive(Debug, Clone)]
struct DurableDeliveryLease {
    operation_id: String,
    message_id: String,
    kind: String,
    request_payload: String,
    execution_authority: Option<AgentExecutionTurnAuthority>,
    /// The receipt INSERT winner already atomically advanced the Conversation
    /// lifecycle to Running in the same SQLite writer transaction.
    durable_admitted: bool,
    /// Exact persistent Conversation generation created by the atomic
    /// receipt/admission transaction. Process-local handles may act only while
    /// this epoch and `operation_id` still identify the active generation.
    admission_epoch: Option<i64>,
    guard_key: String,
    guard_generation: u64,
    receipt_handed_off: Arc<AtomicBool>,
    admission_custodian_owner: Option<Arc<AtomicU8>>,
}

impl DurableDeliveryLease {
    fn handoff_receipt(&self) {
        self.receipt_handed_off.store(true, Ordering::Release);
    }

    fn receipt_was_handed_off(&self) -> bool {
        self.receipt_handed_off.load(Ordering::Acquire)
    }

    fn transfer_admission_custodian(&self, owner: u8) {
        if let Some(custodian_owner) = self.admission_custodian_owner.as_ref() {
            let _ = custodian_owner.compare_exchange(
                ADMISSION_CUSTODIAN_REQUEST_OWNER,
                owner,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    fn transfer_to_detached_turn_owner(&self) {
        self.transfer_admission_custodian(ADMISSION_CUSTODIAN_DETACHED_TURN_OWNER);
    }

    fn transfer_to_stop_owner(&self) {
        self.transfer_admission_custodian(ADMISSION_CUSTODIAN_STOP_OWNER);
    }

    fn transfer_to_background_finalizer(&self) {
        self.transfer_admission_custodian(ADMISSION_CUSTODIAN_BACKGROUND_FINALIZER);
    }

    fn mark_terminal_finalized(&self) {
        self.transfer_admission_custodian(ADMISSION_CUSTODIAN_TERMINAL);
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

/// Reconcile a finite model authority after a backend-resolved preset changes
/// the Nomi lead selected by the caller.
///
/// The incoming request still has to be internally valid before the preset is
/// allowed to replace its lead. This prevents preset resolution from becoming
/// a repair path for malformed client authority. `None` keeps inheriting the
/// final lead and `Automatic` remains catalog-based. A finite pool may replace
/// the caller's original lead, preserving collaborators; when the request has
/// no lead, the resolved preset lead must already be authorized by that pool.
fn reconcile_preset_conversation_model_pool(
    pool: Option<ExecutionModelPool>,
    requested_model: Option<&ProviderWithModel>,
    resolved_lead: &ExecutionModelRef,
) -> Result<Option<ExecutionModelPool>, AppError> {
    ExecutionModelPool::Single {
        model: resolved_lead.clone(),
    }
    .validate()
    .map_err(AppError::BadRequest)?;

    let requested_lead = requested_model.map(conversation_lead_model).transpose()?;
    let Some(pool) = pool else {
        return Ok(None);
    };

    pool.validate().map_err(AppError::BadRequest)?;
    validate_conversation_model_authority(requested_model, Some(&pool))?;

    if requested_lead.is_none() && !pool.contains(resolved_lead) {
        return Err(AppError::BadRequest(format!(
            "preset-resolved lead model {}/{} must belong to execution_model_pool when the request model is omitted",
            resolved_lead.provider_id, resolved_lead.model
        )));
    }
    let reconciled = match pool {
        ExecutionModelPool::Automatic => ExecutionModelPool::Automatic,
        ExecutionModelPool::Single { .. } => ExecutionModelPool::Single {
            model: resolved_lead.clone(),
        },
        ExecutionModelPool::Range { models } => {
            let mut reconciled = Vec::with_capacity(models.len());
            reconciled.push(resolved_lead.clone());
            reconciled.extend(models.into_iter().filter(|model| {
                model != resolved_lead && requested_lead.as_ref() != Some(model)
            }));
            ExecutionModelPool::Range { models: reconciled }
        }
    };
    reconciled.validate().map_err(AppError::BadRequest)?;
    Ok(Some(reconciled))
}

fn required_trimmed_extra_string<'a>(
    extra: &'a serde_json::Value,
    key: &str,
    context: &str,
) -> Result<&'a str, AppError> {
    let value = extra
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| AppError::BadRequest(format!("{context} requires extra.{key}")))?;
    if value.is_empty() || value.trim() != value {
        return Err(AppError::BadRequest(format!(
            "{context} extra.{key} must be a non-empty trimmed string"
        )));
    }
    Ok(value)
}

fn optional_trimmed_extra_string<'a>(
    extra: &'a serde_json::Value,
    key: &str,
    context: &str,
) -> Result<Option<&'a str>, AppError> {
    match extra.get(key) {
        None => Ok(None),
        Some(serde_json::Value::String(value))
            if !value.is_empty() && value.trim() == value =>
        {
            Ok(Some(value))
        }
        Some(_) => Err(AppError::BadRequest(format!(
            "{context} extra.{key} must be a non-empty trimmed string"
        ))),
    }
}

fn validate_acp_agent_metadata_row(
    row: &AgentMetadataRow,
    extra: &serde_json::Value,
) -> Result<(), AppError> {
    if row.agent_type != AgentType::Acp.serde_name() {
        return Err(AppError::BadRequest(format!(
            "ACP extra.agent_id '{}' resolves to agent type '{}'",
            row.agent_id, row.agent_type
        )));
    }
    if !row.enabled {
        return Err(AppError::BadRequest(format!(
            "ACP extra.agent_id '{}' is disabled",
            row.agent_id
        )));
    }
    if !matches!(row.agent_source.as_str(), "builtin" | "extension" | "custom") {
        return Err(AppError::BadRequest(format!(
            "ACP extra.agent_id '{}' has unsupported agent_source '{}'",
            row.agent_id, row.agent_source
        )));
    }
    if let Some(backend) = optional_trimmed_extra_string(extra, "backend", "ACP conversation")?
        && row.backend.as_deref() != Some(backend)
    {
        return Err(AppError::BadRequest(format!(
            "ACP extra.backend '{backend}' does not match agent '{}'",
            row.agent_id
        )));
    }
    if let Some(agent_source) =
        optional_trimmed_extra_string(extra, "agent_source", "ACP conversation")?
        && row.agent_source != agent_source
    {
        return Err(AppError::BadRequest(format!(
            "ACP extra.agent_source '{agent_source}' does not match agent '{}'",
            row.agent_id
        )));
    }
    Ok(())
}

fn reject_acp_identity_patch(extra: &serde_json::Value) -> Result<(), AppError> {
    let Some(object) = extra.as_object() else {
        return Ok(());
    };
    const IDENTITY_KEYS: [&str; 3] = ["agent_id", "backend", "agent_source"];
    if let Some(key) = IDENTITY_KEYS
        .into_iter()
        .find(|key| object.contains_key(*key))
    {
        return Err(AppError::BadRequest(format!(
            "ACP extra.{key} is immutable after creation; create a new conversation to change the agent"
        )));
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
    /// Arm IDMM for this exact admitted turn. The scope is captured at
    /// admission, never reconstructed from a later queued event.
    fn on_turn_start(&self, conversation_id: &str, admitted_scope: IdmmTurnScope);
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
    #[cfg(test)]
    public_admission_cutpoint:
        Arc<std::sync::Mutex<Option<PublicAdmissionCutpointControl>>>,

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
    /// Wait until every runtime-build generation captured by a lifecycle
    /// owner has actually released its lease.
    ///
    /// `CANCEL_TEARDOWN_GRACE` is deliberately only a warning interval.  A
    /// cancelled build may still own a factory future, tool setup, or child
    /// process; elapsed time is not proof that those side effects are gone.
    /// The caller must retain its reset/stop/preparation fence for the entire
    /// wait and may forget the bookkeeping entries only after this returns.
    async fn await_cancelled_runtime_builds_quiesced(
        &self,
        conversation_id: &str,
        cancelled_build_ids: &[u64],
        lifecycle_action: &'static str,
    ) {
        while !self
            .runtime_state
            .wait_for_runtime_builds(
                conversation_id,
                cancelled_build_ids,
                CANCEL_TEARDOWN_GRACE,
            )
            .await
        {
            warn!(
                conversation_id,
                lifecycle_action,
                cancelled_build_count = cancelled_build_ids.len(),
                "Cancelled runtime preparation is still active; retaining lifecycle ownership"
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn install_public_admission_cutpoint(
        &self,
        stage: PublicAdmissionCutpoint,
        panic: bool,
    ) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .public_admission_cutpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(PublicAdmissionCutpointControl {
                stage,
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                panic,
            });
        (entered, release)
    }

    #[cfg(test)]
    pub(crate) fn has_durable_operation_guard(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
    ) -> bool {
        let key = Self::durable_operation_key(user_id, conversation_id, operation_id);
        self.durable_operations_in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key)
    }

    #[cfg(test)]
    async fn reach_public_admission_cutpoint(&self, stage: PublicAdmissionCutpoint) {
        let control = {
            let mut slot = self
                .public_admission_cutpoint
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slot.as_ref().is_some_and(|control| control.stage == stage) {
                slot.take()
            } else {
                None
            }
        };
        let Some(control) = control else {
            return;
        };
        control.entered.notify_one();
        assert!(
            !control.panic,
            "injected public admission custodian cutpoint panic"
        );
        control.release.notified().await;
    }

    async fn adopt_completed_turn_receipt_if_still_active(
        &self,
        user_id: &str,
        conversation_id: &str,
        receipt: &nomifun_db::models::ConversationDeliveryReceiptRow,
    ) -> Result<(), AppError> {
        if receipt.status != "completed" || receipt.kind != "turn" {
            return Ok(());
        }
        let result_ok = receipt.result_ok.ok_or_else(|| {
            AppError::Conflict(
                "completed turn receipt is missing its authoritative result".to_owned(),
            )
        })?;
        let completion = TurnReceiptCompletion {
            operation_id: receipt.operation_id.clone(),
            kind: receipt.kind.clone(),
            request_payload: receipt.request_payload.clone(),
            result_ok,
            result_text: receipt.result_text.clone(),
            result_error: receipt.result_error.clone(),
        };
        match self
            .conversation_repo
            .finalize_exact_turn_operation(
                user_id,
                conversation_id,
                &completion,
                now_ms(),
            )
            .await?
        {
            TurnLifecycleTransition::Committed
            | TurnLifecycleTransition::AlreadyApplied => Ok(()),
            TurnLifecycleTransition::Stale => {
                // A concurrent exact stop/replacement may have won after the
                // read. Inspect exact persisted authority rather than the
                // Running-only validator: a historical partial commit can be
                // `finished` while still carrying this operation, and treating
                // that row as inactive would leave it permanently fenced.
                let admission = self
                    .conversation_repo
                    .get_turn_admission_state(user_id, conversation_id)
                    .await?;
                if admission.active_operation_id.as_deref()
                    != Some(receipt.operation_id.as_str())
                {
                    Ok(())
                } else {
                    Err(AppError::Conflict(
                        "completed turn receipt remains attached to its exact Conversation generation"
                            .to_owned(),
                    ))
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn continue_abandoned_public_turn_admission(
        repo: Arc<dyn IConversationRepository>,
        user_id: String,
        conversation_id: String,
        operation_id: String,
        candidate_message_id: String,
        request_payload: String,
        expected_admitted_epoch: i64,
        operation_guard: (
            DurableOperationGuards,
            String,
            u64,
        ),
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // This guard is armed and dropped only inside an async service
            // request. If an embedding violates that contract, retaining the
            // process-local operation guard is safer than pretending the
            // durable admission was released.
            error!(
                conversation_id,
                operation_id,
                "No Tokio runtime is available for abandoned turn admission recovery"
            );
            return;
        };
        runtime.spawn(async move {
            let mut retry_delay = Duration::from_millis(25);
            loop {
                match repo
                    .abandon_exact_turn_admission(
                        &user_id,
                        &conversation_id,
                        &operation_id,
                        &candidate_message_id,
                        &request_payload,
                        expected_admitted_epoch,
                        "Public turn request was dropped before detached execution ownership",
                        now_ms(),
                    )
                    .await
                {
                    Ok(
                        TurnLifecycleTransition::Committed
                        | TurnLifecycleTransition::AlreadyApplied
                        | TurnLifecycleTransition::Stale,
                    ) => {
                        Self::release_durable_operation_guard(
                            &operation_guard.0,
                            &operation_guard.1,
                            operation_guard.2,
                        );
                        info!(
                            conversation_id,
                            operation_id,
                            "Abandoned public turn admission reached exact durable terminal proof"
                        );
                        return;
                    }
                    Err(error) => {
                        // Missing/corrupt receipt identity and epoch mismatch are
                        // ambiguous commit windows, never evidence that the
                        // operation was not admitted. Keep retrying without a
                        // total timeout; a later exact receipt or explicit
                        // recovery can make the state provable.
                        warn!(
                            conversation_id,
                            operation_id,
                            error = %ErrorChain(&error),
                            "Abandoned public turn admission remains quarantined"
                        );
                    }
                }
                tokio::time::sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn continue_abandoned_execution_turn_admission(
        boundary: Arc<dyn ExecutionConversationBoundary>,
        user_id: String,
        conversation_id: String,
        operation_id: String,
        candidate_message_id: String,
        request_payload: String,
        authority: AgentExecutionTurnAuthority,
        expected_admitted_epoch: i64,
        operation_guard: (
            DurableOperationGuards,
            String,
            u64,
        ),
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            error!(
                conversation_id,
                operation_id,
                "No Tokio runtime is available for abandoned Agent Execution turn recovery"
            );
            return;
        };
        runtime.spawn(async move {
            let mut retry_delay = Duration::from_millis(25);
            loop {
                match boundary
                    .abandon_exact_attempt_turn_admission(
                        &user_id,
                        &conversation_id,
                        &operation_id,
                        &candidate_message_id,
                        &request_payload,
                        &authority,
                        expected_admitted_epoch,
                        "Agent Execution turn request was dropped before detached execution ownership",
                        now_ms(),
                    )
                    .await
                {
                    Ok(
                        TurnLifecycleTransition::Committed
                        | TurnLifecycleTransition::AlreadyApplied
                        | TurnLifecycleTransition::Stale,
                    ) => {
                        Self::release_durable_operation_guard(
                            &operation_guard.0,
                            &operation_guard.1,
                            operation_guard.2,
                        );
                        info!(
                            conversation_id,
                            operation_id,
                            "Abandoned Agent Execution turn admission reached exact durable terminal proof"
                        );
                        return;
                    }
                    Err(error) => {
                        // An exact active attempt with a missing/corrupt
                        // candidate receipt is quarantined by the boundary.
                        // Retain this guard and retry until durable authority is
                        // provably committed, displaced, or explicitly reset.
                        warn!(
                            conversation_id,
                            operation_id,
                            error = %ErrorChain(&error),
                            "Abandoned Agent Execution turn admission remains quarantined"
                        );
                    }
                }
                tokio::time::sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
            }
        });
    }

    async fn quarantine_edit_runtime_until_confirmed(
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        runtime_state: &Arc<ConversationRuntimeStateService>,
        conversation_id: &str,
        conversation_created_at: i64,
    ) {
        Self::terminate_runtime_until_confirmed(
            runtime_registry,
            conversation_id,
            AgentKillReason::AgentErrorRecovery,
            "failed edit/resubmit destructive preparation",
        )
        .await;

        let mut retry_delay = Duration::from_millis(25);
        loop {
            match runtime_registry
                .reset_persisted_nomi_session(conversation_id, conversation_created_at)
                .await
            {
                Ok(_) => break,
                Err(error) => {
                    error!(
                        conversation_id,
                        error = %ErrorChain(&error),
                        "Failed to erase persisted Nomi recovery authority after edit/resubmit mutation; retaining durable fence"
                    );
                }
            }
            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
        }
        runtime_state.clear_knowledge_signature(conversation_id);
        runtime_state.clear_turn_tokens(conversation_id);
    }

    #[allow(clippy::too_many_arguments)]
    fn continue_abandoned_edit_resubmit_admission(
        repo: Arc<dyn IConversationRepository>,
        runtime_registry: Arc<dyn AgentRuntimeRegistry>,
        runtime_state: Arc<ConversationRuntimeStateService>,
        user_id: String,
        conversation_id: String,
        operation_id: String,
        candidate_message_id: String,
        request_payload: String,
        reserved_admission_epoch: i64,
        admitted_admission_epoch: i64,
        conversation_created_at: i64,
        phase: u8,
        operation_guard: (
            DurableOperationGuards,
            String,
            u64,
        ),
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            error!(
                conversation_id,
                operation_id,
                "No Tokio runtime is available for abandoned edit/resubmit recovery"
            );
            return;
        };
        runtime.spawn(async move {
            // The dropped request released its original guard. Reacquire the
            // same gate before interpreting a reservation as abandoned, and
            // retain it through runtime quarantine and exact durable cleanup.
            let cancellation = CancellationToken::new();
            let _preparation_guard = match runtime_state
                .acquire_preparation_gate(&conversation_id, &cancellation)
                .await
            {
                Ok(guard) => guard,
                Err(error) => {
                    error!(
                        conversation_id,
                        operation_id,
                        error = %ErrorChain(&error),
                        "Abandoned edit/resubmit could not reacquire its preparation gate; durable fence remains quarantined"
                    );
                    return;
                }
            };

            let mut retry_delay = Duration::from_millis(25);
            if phase == EDIT_CUSTODIAN_RESERVED {
                loop {
                    match repo
                        .recover_unadmitted_edit_resubmit_reservation(
                            &user_id,
                            &conversation_id,
                            &operation_id,
                            &candidate_message_id,
                            &request_payload,
                            reserved_admission_epoch,
                            "Edit/resubmit request was dropped before durable turn admission",
                            now_ms(),
                        )
                        .await
                    {
                        Ok(
                            TurnLifecycleTransition::Committed
                            | TurnLifecycleTransition::AlreadyApplied,
                        ) => {
                            Self::release_durable_operation_guard(
                                &operation_guard.0,
                                &operation_guard.1,
                                operation_guard.2,
                            );
                            return;
                        }
                        Ok(TurnLifecycleTransition::Stale) => {
                            // The admit transaction may have committed while
                            // its future was being dropped. Fall through to the
                            // candidate+epoch exact admitted-generation command.
                            break;
                        }
                        Err(error) => {
                            warn!(
                                conversation_id,
                                operation_id,
                                error = %ErrorChain(&error),
                                "Abandoned edit/resubmit reservation remains quarantined"
                            );
                        }
                    }
                    tokio::time::sleep(retry_delay).await;
                    retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
                }
            }

            if phase != EDIT_CUSTODIAN_RESERVED && phase != EDIT_CUSTODIAN_ADMITTED {
                Self::quarantine_edit_runtime_until_confirmed(
                    &runtime_registry,
                    &runtime_state,
                    &conversation_id,
                    conversation_created_at,
                )
                .await;
            }

            retry_delay = Duration::from_millis(25);
            loop {
                match repo
                    .abandon_exact_turn_admission(
                        &user_id,
                        &conversation_id,
                        &operation_id,
                        &candidate_message_id,
                        &request_payload,
                        admitted_admission_epoch,
                        "Edit/resubmit request was dropped before detached execution ownership",
                        now_ms(),
                    )
                    .await
                {
                    Ok(
                        TurnLifecycleTransition::Committed
                        | TurnLifecycleTransition::AlreadyApplied
                        | TurnLifecycleTransition::Stale,
                    ) => {
                        Self::release_durable_operation_guard(
                            &operation_guard.0,
                            &operation_guard.1,
                            operation_guard.2,
                        );
                        return;
                    }
                    Err(error) => {
                        warn!(
                            conversation_id,
                            operation_id,
                            error = %ErrorChain(&error),
                            "Abandoned admitted edit/resubmit remains quarantined"
                        );
                    }
                }
                tokio::time::sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
            }
        });
    }

    fn continue_atomic_turn_finalization_in_background(
        repo: Arc<dyn IConversationRepository>,
        user_id: String,
        conversation_id: String,
        completion: TurnReceiptCompletion,
        operation_guard: (
            DurableOperationGuards,
            String,
            u64,
        ),
    ) {
        tokio::spawn(async move {
            let mut retry_delay = Duration::from_millis(50);
            loop {
                let receipt = repo
                    .get_delivery_receipt(
                        &user_id,
                        &conversation_id,
                        &completion.operation_id,
                    )
                    .await;
                match receipt {
                    Ok(Some(receipt))
                        if receipt.user_id == user_id
                            && receipt.conversation_id == conversation_id
                            && receipt.kind == completion.kind
                            && receipt.request_payload == completion.request_payload
                            && matches!(receipt.status.as_str(), "accepted" | "completed") =>
                    {}
                    // Missing/corrupt identity is not proof that execution
                    // never crossed its external boundary. Retain the guard
                    // and fail closed until durable state becomes provable.
                    Ok(None) | Ok(Some(_)) | Err(_) => {
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
                        continue;
                    }
                };

                match repo
                    .finalize_exact_turn_operation(
                        &user_id,
                        &conversation_id,
                        &completion,
                        now_ms(),
                    )
                    .await
                {
                    Ok(
                        TurnLifecycleTransition::Committed
                        | TurnLifecycleTransition::AlreadyApplied,
                    ) => {
                        Self::release_durable_operation_guard(
                            &operation_guard.0,
                            &operation_guard.1,
                            operation_guard.2,
                        );
                        return;
                    }
                    Ok(TurnLifecycleTransition::Stale) | Err(_) => {
                        let displaced_and_absorbed =
                            matches!(
                                repo.get_delivery_receipt(
                                    &user_id,
                                    &conversation_id,
                                    &completion.operation_id,
                                )
                                .await,
                                Ok(Some(receipt))
                                    if receipt.user_id == user_id
                                        && receipt.conversation_id == conversation_id
                                        && receipt.kind == completion.kind
                                        && receipt.request_payload == completion.request_payload
                                        && receipt.status == "completed"
                            ) && matches!(
                                repo.get_turn_admission_state(
                                    &user_id,
                                    &conversation_id,
                                )
                                .await,
                                Ok(state)
                                    if state.active_operation_id.as_deref()
                                        != Some(completion.operation_id.as_str())
                            );
                        if displaced_and_absorbed {
                            Self::release_durable_operation_guard(
                                &operation_guard.0,
                                &operation_guard.1,
                                operation_guard.2,
                            );
                            return;
                        }
                    }
                }
                tokio::time::sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
            }
        });
    }

    async fn finalize_durable_admission_after_error(
        &self,
        user_id: &str,
        conversation_id: &str,
        delivery: &DurableDeliveryLease,
        fallback_error: &str,
    ) {
        // The exact repository command below preserves an already-completed
        // receipt's authoritative result. If that receipt is still attached to
        // active Running A, it adopts the result and closes A; if A was
        // displaced, it cannot touch the replacement generation.
        let completion = TurnReceiptCompletion {
            operation_id: delivery.operation_id.clone(),
            kind: delivery.kind.clone(),
            request_payload: delivery.request_payload.clone(),
            result_ok: false,
            result_text: None,
            result_error: Some(fallback_error.to_owned()),
        };
        match self
            .conversation_repo
            .finalize_exact_turn_operation(
                user_id,
                conversation_id,
                &completion,
                now_ms(),
            )
            .await
        {
            Ok(TurnLifecycleTransition::Committed | TurnLifecycleTransition::AlreadyApplied) => {
                delivery.mark_terminal_finalized();
            }
            Ok(TurnLifecycleTransition::Stale) | Err(_) => {
                delivery.handoff_receipt();
                delivery.transfer_to_background_finalizer();
                Self::continue_atomic_turn_finalization_in_background(
                    Arc::clone(&self.conversation_repo),
                    user_id.to_owned(),
                    conversation_id.to_owned(),
                    completion,
                    (
                        Arc::clone(&self.durable_operations_in_flight),
                        delivery.guard_key.clone(),
                        delivery.guard_generation,
                    ),
                );
            }
        }
    }

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
                if matches!(
                    repo.get_delivery_receipt(user_id, conversation_id, operation_id)
                        .await,
                    Ok(Some(receipt))
                        if receipt.user_id == user_id
                            && receipt.conversation_id == conversation_id
                            && receipt.operation_id == operation_id
                            && receipt.status == "completed"
                ) {
                    // A stop/error race may have committed a different
                    // authoritative result. Exact completed identity is
                    // absorbing; compensation must not retry forever or
                    // overwrite that result.
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

    pub(crate) fn public_turn_operation_id(
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
    ) -> String {
        // `conversation_delivery_receipts.operation_id` is globally unique.
        // Public client tokens are only scoped to one owner and Conversation,
        // so the receiver must namespace them before they reach that table.
        format!("public-turn:v1:{user_id}:{conversation_id}:{idempotency_key}")
    }

    fn public_steer_operation_id(
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
    ) -> String {
        format!("public-steer:v1:{user_id}:{conversation_id}:{idempotency_key}")
    }

    fn public_steer_operation_prefix(
        user_id: &str,
        conversation_id: &str,
    ) -> String {
        format!("public-steer:v1:{user_id}:{conversation_id}:")
    }

    fn public_edit_resubmit_operation_id(
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
    ) -> String {
        format!("public-edit-resubmit:v1:{user_id}:{conversation_id}:{idempotency_key}")
    }

    fn public_edit_resubmit_operation_prefix(
        user_id: &str,
        conversation_id: &str,
    ) -> String {
        format!("public-edit-resubmit:v1:{user_id}:{conversation_id}:")
    }

    fn edit_resubmit_request_payload(
        target_message_id: &str,
        req: &SendMessageRequest,
    ) -> String {
        serde_json::json!({
            "workflow": "edit-resubmit",
            "target_message_id": target_message_id,
            "content": &req.content,
            "files": &req.files,
            "inject_skills": &req.inject_skills,
            "hidden": req.hidden,
            "origin": &req.origin,
            "channel_platform": &req.channel_platform,
        })
        .to_string()
    }

    fn steer_delivery_request_fields(req: &SendMessageRequest) -> serde_json::Value {
        serde_json::json!({
            "content": &req.content,
            "files": &req.files,
            "inject_skills": &req.inject_skills,
            "hidden": req.hidden,
            "origin": &req.origin,
            "channel_platform": &req.channel_platform,
        })
    }

    fn steer_delivery_request_payload(
        req: &SendMessageRequest,
        scope: &IdmmTurnScope,
    ) -> String {
        let mut payload = Self::steer_delivery_request_fields(req);
        payload
            .as_object_mut()
            .expect("steer request payload is an object")
            .insert(
                "turn_scope".to_owned(),
                serde_json::to_value(scope).expect("turn scope is serializable"),
            );
        payload.to_string()
    }

    fn existing_steer_receipt_scope(
        request_payload: &str,
        req: &SendMessageRequest,
    ) -> Option<IdmmTurnScope> {
        let Ok(mut stored) = serde_json::from_str::<serde_json::Value>(request_payload) else {
            return None;
        };
        let Some(stored_object) = stored.as_object_mut() else {
            return None;
        };
        let scope = serde_json::from_value::<IdmmTurnScope>(
            stored_object
                .remove("turn_scope")
                .unwrap_or(serde_json::Value::Null),
        )
        .ok()?;
        (stored == Self::steer_delivery_request_fields(req)).then_some(scope)
    }

    fn turn_delivery_request_payload(req: &SendMessageRequest) -> String {
        serde_json::json!({
            "content": &req.content,
            "files": &req.files,
            "inject_skills": &req.inject_skills,
            "hidden": req.hidden,
            "origin": &req.origin,
            "channel_platform": &req.channel_platform,
        })
        .to_string()
    }

    fn agent_execution_turn_delivery_request_payload(
        req: &SendMessageRequest,
        authority: &AgentExecutionTurnAuthority,
    ) -> String {
        serde_json::json!({
            "delivery": {
                "content": &req.content,
                "files": &req.files,
                "inject_skills": &req.inject_skills,
                "hidden": req.hidden,
                "origin": &req.origin,
                "channel_platform": &req.channel_platform,
            },
            "agent_execution_authority": authority,
        })
        .to_string()
    }

    fn autowork_turn_delivery_request_payload(
        req: &SendMessageRequest,
        authority: &RequirementConversationTurnAuthority,
    ) -> String {
        // Never persist the opaque claim capability. The receiver validates it
        // directly against `requirements` in the admission transaction; the
        // durable payload binds its one-way digest so a rotated same-generation
        // capability cannot replay another claim's receipt.
        let claim_token_sha256 =
            format!("{:x}", Sha256::digest(authority.claim_token.as_bytes()));
        serde_json::json!({
            "delivery": {
                "content": &req.content,
                "files": &req.files,
                "inject_skills": &req.inject_skills,
                "hidden": req.hidden,
                "origin": &req.origin,
                "channel_platform": &req.channel_platform,
            },
            "autowork_authority": {
                "requirement_id": &authority.requirement_id,
                "claim_generation": authority.claim_generation,
                "claim_token_sha256": claim_token_sha256,
            },
        })
        .to_string()
    }

    fn turn_receipt_completion(
        operation_id: Option<&str>,
        kind: Option<&str>,
        request_payload: Option<&str>,
        result_ok: bool,
        result_text: Option<&str>,
        result_error: Option<&str>,
    ) -> Option<TurnReceiptCompletion> {
        operation_id
            .zip(kind)
            .zip(request_payload)
            .map(|((operation_id, kind), request_payload)| TurnReceiptCompletion {
                operation_id: operation_id.to_owned(),
                kind: kind.to_owned(),
                request_payload: request_payload.to_owned(),
                result_ok,
                result_text: result_text.map(str::to_owned),
                result_error: result_error.map(str::to_owned),
            })
    }

    fn release_durable_operation_guard(
        guard: &DurableOperationGuards,
        key: &str,
        generation: u64,
    ) {
        // Poisoning records a past panic; it must not strand a durable
        // accepted receipt after the database admission already committed.
        // Recover ownership of the map and remove only the exact generation.
        let mut operations = guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if operations
            .get(key)
            .is_some_and(|lease| lease.generation == generation)
        {
            operations.remove(key);
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
            #[cfg(test)]
            public_admission_cutpoint: Arc::new(std::sync::Mutex::new(None)),

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
    /// `delete()`. Used by `nomifun-app` to wire up process/filesystem cleanup
    /// that is independent from repository-owned logical-reference deletion.
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
        MessageId::new().into_string()
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

    /// Resolve the model for one knowledge write-back. A valid explicit
    /// knowledge preference wins; otherwise the model that actually completed
    /// the conversation turn is used, including its provider-specific
    /// `use_model` override.
    pub(crate) async fn resolve_turn_writeback_model(
        &self,
        session_model: Option<&ProviderWithModel>,
    ) -> Result<Option<ProviderWithModel>, String> {
        let fallback = session_model.cloned();
        let Some((provider_repo, client_prefs)) = self.failover_deps() else {
            return Ok(fallback);
        };
        let preferences = match client_prefs
            .get_by_keys(&[KNOWLEDGE_AUTOGEN_MODEL_PREF_KEY])
            .await
        {
            Ok(preferences) => preferences,
            Err(error) => {
                warn!(
                    error = %ErrorChain(&error),
                    "Failed to read explicit knowledge write-back model"
                );
                return Err(
                    "Could not read the configured knowledge write-back model; retry"
                        .to_owned(),
                );
            }
        };
        let Some(raw) = preferences
            .into_iter()
            .find(|preference| preference.key == KNOWLEDGE_AUTOGEN_MODEL_PREF_KEY)
            .map(|preference| preference.value)
        else {
            return Ok(fallback);
        };
        let mut selected = match serde_json::from_str::<ProviderWithModel>(&raw) {
            Ok(selected) if selected.validate().is_ok() => selected,
            Ok(_) | Err(_) => {
                warn!(
                    preference = KNOWLEDGE_AUTOGEN_MODEL_PREF_KEY,
                    "Invalid explicit knowledge write-back model preference"
                );
                return Err(
                    "The configured knowledge write-back model is invalid; update it and retry"
                        .to_owned(),
                );
            }
        };
        // The knowledge preference UI selects exactly provider_id + model.
        // `use_model` is a per-turn session failover override and must not be
        // accepted from stale/manually imported preference JSON, where it
        // could bypass the availability check below and call another model.
        selected.use_model = None;
        let provider = match provider_repo.find_by_id(&selected.provider_id).await {
            Ok(Some(provider)) => provider,
            Ok(None) => {
                warn!(
                    provider_id = %selected.provider_id,
                    "Knowledge write-back model provider no longer exists"
                );
                return Err(
                    "The configured knowledge write-back provider no longer exists; choose another model and retry"
                        .to_owned(),
                );
            }
            Err(error) => {
                warn!(
                    provider_id = %selected.provider_id,
                    error = %ErrorChain(&error),
                    "Failed to validate explicit knowledge write-back model"
                );
                return Err(
                    "Could not validate the configured knowledge write-back model; retry"
                        .to_owned(),
                );
            }
        };
        let models = serde_json::from_str::<Vec<String>>(&provider.models).unwrap_or_default();
        let model_enabled = provider
            .model_enabled
            .as_deref()
            .and_then(|raw| serde_json::from_str::<HashMap<String, bool>>(raw).ok())
            .unwrap_or_default();
        if !provider.enabled
            || !models.iter().any(|model| model == &selected.model)
            || model_enabled.get(&selected.model) == Some(&false)
        {
            warn!(
                provider_id = %selected.provider_id,
                model = %selected.model,
                "Knowledge write-back model is unavailable"
            );
            return Err(
                "The configured knowledge write-back model is unavailable; choose another model and retry"
                    .to_owned(),
            );
        }
        Ok(Some(selected))
    }

    fn turn_writeback_key(conversation_id: &str, message_id: &str) -> String {
        format!("{conversation_id}\0{message_id}")
    }

    fn try_start_turn_writeback(
        &self,
        conversation_id: &str,
        message_id: &str,
    ) -> Option<TurnWritebackRunGuard> {
        let key = Self::turn_writeback_key(conversation_id, message_id);
        let mut in_flight = TURN_WRITEBACKS_IN_FLIGHT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if in_flight.contains_key(&key) {
            return None;
        }
        let generation =
            NEXT_TURN_WRITEBACK_RUN_GENERATION.fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        in_flight.insert(
            key.clone(),
            TurnWritebackRun {
                generation,
                cancellation: cancellation.clone(),
            },
        );
        Some(TurnWritebackRunGuard {
            key,
            generation,
            cancellation,
        })
    }

    #[cfg(test)]
    pub(crate) fn install_blocking_turn_writeback_run_for_test(
        &self,
        conversation_id: &str,
        message_id: &str,
    ) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let guard = self
            .try_start_turn_writeback(conversation_id, message_id)
            .expect("test write-back key must not already be running");
        let cancellation = guard.cancellation_token();
        let cancelled = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let cancelled_for_task = Arc::clone(&cancelled);
        let release_for_task = Arc::clone(&release);
        tokio::spawn(async move {
            cancellation.cancelled().await;
            cancelled_for_task.notify_one();
            release_for_task.notified().await;
            drop(guard);
        });
        (cancelled, release)
    }

    fn turn_writeback_is_running(&self, conversation_id: &str, message_id: &str) -> bool {
        TURN_WRITEBACKS_IN_FLIGHT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&Self::turn_writeback_key(conversation_id, message_id))
    }

    fn cancel_turn_writebacks_for_conversation(&self, conversation_id: &str) {
        let prefix = format!("{conversation_id}\0");
        let cancellations: Vec<CancellationToken> = TURN_WRITEBACKS_IN_FLIGHT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(_, run)| run.cancellation.clone())
            .collect();
        for cancellation in cancellations {
            cancellation.cancel();
        }
    }

    /// Cancel every process-local attempt for a conversation and wait until
    /// each owner has reached a cancellation-safe boundary. Message
    /// reset/delete must not race a late atomic knowledge-file publication or
    /// let a detached state update recreate a row after the transcript is
    /// gone.
    async fn cancel_and_wait_for_turn_writebacks(
        &self,
        conversation_id: &str,
    ) -> Result<(), AppError> {
        self.cancel_turn_writebacks_for_conversation(conversation_id);
        let prefix = format!("{conversation_id}\0");
        let mut warning_interval = tokio::time::interval(TURN_WRITEBACK_CANCEL_GRACE);
        warning_interval.tick().await;
        loop {
            let any_running = TURN_WRITEBACKS_IN_FLIGHT
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .keys()
                .any(|key| key.starts_with(&prefix));
            if !any_running {
                return Ok(());
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                _ = warning_interval.tick() => {
                    warn!(
                        conversation_id,
                        "Knowledge write-back is still quiescing; retaining lifecycle ownership"
                    );
                }
            }
        }
    }

    fn spawn_turn_writeback(
        &self,
        knowledge_service: Arc<nomifun_knowledge::KnowledgeService>,
        mut request: nomifun_knowledge::TurnWritebackRequest,
        session_model: Option<ProviderWithModel>,
        final_text: String,
        attempt: TurnWritebackAttempt,
        guard: TurnWritebackRunGuard,
        workspace_binding_lease: Option<nomifun_knowledge::WorkspaceBindingLease>,
    ) {
        let resolver = self.clone();
        let cancellation = guard.cancellation_token();
        tokio::spawn(async move {
            let _guard = guard;
            let _workspace_binding_lease = workspace_binding_lease;
            let panic_attempt = attempt.clone();
            let worker = async {
                if cancellation.is_cancelled() {
                    attempt
                        .interrupt("Knowledge write-back was cancelled because the conversation changed")
                        .await;
                    return;
                }
                request.cancellation = Some(cancellation);
                request.model = match resolver
                    .resolve_turn_writeback_model(session_model.as_ref())
                    .await
                {
                    Ok(model) => model,
                    Err(error) => {
                        if let Err(persist_error) =
                            finish_turn_writeback_failure(attempt, error).await
                        {
                            error!(
                                error = %ErrorChain(&persist_error),
                                "Knowledge write-back model failure did not reach a durable terminal state"
                            );
                        }
                        return;
                    }
                };
                if let Err(error) =
                    run_turn_writeback_report(knowledge_service, request, final_text, attempt)
                        .await
                {
                    error!(
                        error = %ErrorChain(&error),
                        "Knowledge write-back worker did not report a durable terminal result"
                    );
                }
            };
            let result = AssertUnwindSafe(worker).catch_unwind().await;
            match result {
                Err(_) => {
                    warn!("Detached knowledge write-back panicked");
                    panic_attempt
                        .interrupt("Knowledge write-back stopped unexpectedly")
                        .await;
                }
                Ok(()) => {}
            }
        });
    }

    fn project_orphaned_turn_writeback(
        &self,
        mut response: MessageResponse,
    ) -> MessageResponse {
        let Some(writeback) = response
            .content
            .as_object_mut()
            .and_then(|content| content.get_mut("knowledge_writeback"))
        else {
            return response;
        };
        if !writeback
            .get("status")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|status| matches!(status, "started" | "extracting" | "writing"))
            || self.turn_writeback_is_running(&response.conversation_id, &response.message_id)
        {
            return response;
        }
        let interrupted_at = now_ms();
        if let Some(state) = writeback.as_object_mut() {
            state.insert("status".to_owned(), serde_json::json!("interrupted"));
            state.insert("retryable".to_owned(), serde_json::json!(true));
            state.insert("updated_at".to_owned(), serde_json::json!(interrupted_at));
            state.insert("finished_at".to_owned(), serde_json::json!(interrupted_at));
            state.insert("interrupted_at".to_owned(), serde_json::json!(interrupted_at));
            state.insert(
                "failures".to_owned(),
                serde_json::json!([{
                    "kb_id": serde_json::Value::Null,
                    "rel_path": serde_json::Value::Null,
                    "error": "Knowledge write-back was interrupted when the application stopped"
                }]),
            );
        }
        response
    }

    pub fn runtime_state(&self) -> Arc<ConversationRuntimeStateService> {
        self.runtime_state.clone()
    }

    async fn compensate_failed_creation(
        &self,
        conversation_id: &str,
        managed_workspace: Option<&Path>,
        original_error: AppError,
    ) -> AppError {
        let mut cleanup_errors = Vec::new();
        let aggregate_deleted = match self.conversation_repo.delete(conversation_id).await {
            Ok(()) | Err(nomifun_db::DbError::NotFound(_)) => true,
            Err(error) => {
                warn!(
                    conversation_id,
                    error = %ErrorChain(&error),
                    original_error = %original_error,
                    "failed to roll back conversation row after creation failed; retained aggregate remains addressable"
                );
                cleanup_errors.push(format!("database rollback failed: {error}"));
                false
            }
        };

        if aggregate_deleted {
            // SQLite's Conversation repository owns this logical child
            // cleanup. Keep the explicit repository call for alternate
            // repository implementations and tests; deleting a missing row is
            // intentionally a no-op.
            if let Err(error) = self.acp_session_repo.delete(conversation_id).await {
                warn!(
                    conversation_id,
                    error = %ErrorChain(&error),
                    "conversation rollback committed, but ACP session cleanup failed; session row may be orphaned"
                );
                cleanup_errors.push(format!("ACP session cleanup failed: {error}"));
            }

            if let Some(path) = managed_workspace
                && path.exists()
                && let Err(error) = std::fs::remove_dir_all(path)
            {
                warn!(
                    conversation_id,
                    workspace = %path.display(),
                    error = %ErrorChain(&error),
                    "conversation rollback committed, but managed workspace cleanup failed; directory remains as an orphan"
                );
                cleanup_errors.push(format!(
                    "managed workspace '{}' cleanup failed: {error}",
                    path.display()
                ));
            }
        }

        if cleanup_errors.is_empty() {
            original_error
        } else {
            AppError::Internal(format!(
                "conversation creation failed ({original_error}); compensation was incomplete: {}",
                cleanup_errors.join("; ")
            ))
        }
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
            .projection(user_id, &response.conversation_id)
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

    async fn ensure_retained_execution_attempt(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<(), AppError> {
        if !self
            .is_execution_attempt_conversation(user_id, conversation_id)
            .await?
        {
            return Err(AppError::Conflict(
                "Agent Execution control requires a retained Execution Attempt Conversation"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Fail closed while a destructive edit/resubmit receipt remains accepted.
    /// This observer never performs recovery: before the shared preparation
    /// gate is held, `Finished + phase=accepted` may still belong to a live
    /// request paused between reservation and admission.
    pub async fn ensure_no_ambiguous_edit_resubmit(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<(), AppError> {
        let conversation_id = parse_conv_id(conversation_id)?;
        let prefix =
            Self::public_edit_resubmit_operation_prefix(user_id, conversation_id);
        if self
            .conversation_repo
            .has_accepted_delivery_receipt_operation_prefix(
                user_id,
                conversation_id,
                &prefix,
            )
            .await?
        {
            return Err(AppError::Conflict(
                "an edit/resubmit workflow has an ambiguous durable outcome; explicit Conversation reset is required"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// Recover only the pre-admission edit/resubmit crash cutpoint.
    ///
    /// The caller must hold the per-Conversation preparation gate. That gate
    /// proves no live local request remains paused between reservation and
    /// admission; the repository transaction then proves the persisted
    /// Finished/epoch/operation/phase identity before settling anything.
    async fn recover_unadmitted_edit_resubmit_reservation_under_gate(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<(), AppError> {
        let conversation_id = parse_conv_id(conversation_id)?;
        let prefix =
            Self::public_edit_resubmit_operation_prefix(user_id, conversation_id);
        if !self
            .conversation_repo
            .has_accepted_delivery_receipt_operation_prefix(
                user_id,
                conversation_id,
                &prefix,
            )
            .await?
        {
            return Ok(());
        }
        let row = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        let admission = self
            .conversation_repo
            .get_turn_admission_state(user_id, conversation_id)
            .await?;
        let marker: Option<(String, String)> =
            serde_json::from_str::<serde_json::Value>(&row.extra)
                .ok()
                .and_then(|extra| {
                    let marker = extra.get("_edit_resubmit_fence")?;
                    Some((
                        marker.get("operation_id")?.as_str()?.to_owned(),
                        marker.get("phase")?.as_str()?.to_owned(),
                    ))
                });
        if row.status.as_deref() == Some("finished")
            && admission.active_operation_id.is_none()
            && let Some((operation_id, phase)) = marker
            && phase == "accepted"
            && operation_id.starts_with(&prefix)
        {
            let receipt = self
                .conversation_repo
                .get_delivery_receipt(user_id, conversation_id, &operation_id)
                .await?
                .ok_or_else(|| {
                    AppError::Conflict(
                        "edit/resubmit reservation lost its exact receipt".to_owned(),
                    )
                })?;
            if receipt.user_id != user_id
                || receipt.conversation_id != conversation_id
                || receipt.operation_id != operation_id
                || receipt.kind != "turn"
            {
                return Err(AppError::Conflict(
                    "edit/resubmit reservation receipt identity is invalid".to_owned(),
                ));
            }
            if self
                .conversation_repo
                .recover_unadmitted_edit_resubmit_reservation(
                    user_id,
                    conversation_id,
                    &operation_id,
                    &receipt.message_id,
                    &receipt.request_payload,
                    admission.epoch,
                    "Application stopped before edit/resubmit admission",
                    now_ms(),
                )
                .await?
                == TurnLifecycleTransition::Committed
            {
                info!(
                    conversation_id,
                    operation_id,
                    "Recovered an interrupted pre-admission edit/resubmit reservation"
                );
            }
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
            .await?;
        self.ensure_no_ambiguous_edit_resubmit(user_id, conversation_id)
            .await
    }

    /// Initial-owner preflight for public/background work that already holds
    /// a runtime preparation lease.
    ///
    /// Unlike [`Self::ensure_public_mutation_allowed`], this method enters the
    /// per-Conversation preparation gate and may repair only the exact
    /// `Finished + no active operation + accepted edit reservation` crash
    /// cutpoint. Cron/AutoWork use it before staging files or constructing a
    /// runtime. Mid-turn observers and failover probes must keep using the
    /// observer-only method above.
    pub async fn ensure_initial_public_mutation_allowed(
        &self,
        user_id: &str,
        conversation_id: &str,
        build_lease: &RuntimeBuildLease,
    ) -> Result<(), AppError> {
        let conversation_id = parse_conv_id(conversation_id)?;
        build_lease.ensure_active()?;
        let cancellation = build_lease.cancellation_token();
        let _preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(conversation_id, &cancellation)
            .await?;
        build_lease.ensure_active()?;
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|conversation| conversation.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        self.ensure_not_retained_execution_attempt(user_id, conversation_id)
            .await?;
        self.recover_unadmitted_edit_resubmit_reservation_under_gate(
            user_id,
            conversation_id,
        )
        .await?;
        self.ensure_no_ambiguous_edit_resubmit(user_id, conversation_id)
            .await?;
        build_lease.ensure_active()?;

        // Do not let background preparation treat a persisted Running row as
        // permission to build. The keyed send seam owns exact orphan
        // reconciliation with runtime-exit proof.
        let row = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .filter(|conversation| conversation.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        if row.status.as_deref() == Some("running") {
            return Err(AppError::Conflict(
                "Conversation already has a durable running turn".to_owned(),
            ));
        }
        Ok(())
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

    pub fn begin_public_runtime_preparation(
        &self,
        conversation_id: &str,
        requester_user_id: &str,
    ) -> Result<RuntimeBuildLease, AppError> {
        self.runtime_state.begin_runtime_preparation_for_requester(
            conversation_id,
            Some(requester_user_id.to_owned()),
            true,
        )
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
            active_turn_id: None,
            processing_started_at: None,
        }
    }

    /// Attempt one result-bearing runtime teardown.
    ///
    /// This is the request-facing form: it reports failure immediately while
    /// the production registry retains its quarantined slot. Callers must not
    /// mutate mounts/config-dependent runtime state, delete the slot, or build
    /// a replacement after an error.
    pub(crate) async fn terminate_runtime_with_proof(
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        conversation_id: &str,
        reason: AgentKillReason,
        operation: &'static str,
    ) -> Result<(), AppError> {
        match runtime_registry
            .terminate_and_wait_result(conversation_id, Some(reason))
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => {
                error!(
                    conversation_id,
                    operation,
                    error = %ErrorChain(&error),
                    "Agent runtime teardown was not proven; registry slot remains quarantined"
                );
                Err(error)
            }
        }
    }

    /// Await proof that the registered process tree for one Conversation has
    /// exited. This retrying form is only for detached lifecycle owners that
    /// must retain a stop/completion/deletion fence until proof arrives. A
    /// failed teardown leaves the registry slot quarantined; retrying has no
    /// total business timeout because releasing a durable Running turn while
    /// that process may still execute would reopen the duplicate-execution
    /// window this lifecycle fence closes.
    pub(crate) async fn terminate_runtime_until_confirmed(
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        conversation_id: &str,
        reason: AgentKillReason,
        operation: &'static str,
    ) {
        let mut retry_delay = Duration::from_millis(25);
        loop {
            match Self::terminate_runtime_with_proof(
                runtime_registry,
                conversation_id,
                reason,
                operation,
            )
            .await
            {
                Ok(()) => return,
                Err(_) => {}
            }
            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
        }
    }

    async fn release_runtime_turn_until_confirmed(
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        conversation_id: &str,
        turn_generation: u64,
    ) {
        let mut retry_delay = Duration::from_millis(25);
        loop {
            match runtime_registry
                .release_runtime_turn(conversation_id, turn_generation)
                .await
            {
                Ok(()) => return,
                Err(error) => {
                    error!(
                        conversation_id,
                        turn_generation,
                        error = %ErrorChain(&error),
                        "Exact runtime turn admission release failed; retaining lifecycle fences"
                    );
                }
            }
            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
        }
    }

    /// Reject one durable Running generation that has no process-local turn
    /// owner and no durable, queryable terminal proof.
    ///
    /// Parent-death configuration and holding the new server lock prove neither
    /// that the exact prior authority observed the cancellation nor that its
    /// complete descendant tree is empty. Consequently this seam deliberately
    /// contains no runtime teardown, writeback settlement, lifecycle CAS, or
    /// completion broadcast. A future recovery implementation must introduce
    /// a persisted exact terminal-proof protocol rather than enabling a backend
    /// in a static allow-list.
    fn unproven_running_generation_error(&self, row: &ConversationRow) -> AppError {
        let conversation_id = row.conversation_id.as_str();
        if self.runtime_state.has_active_turn(conversation_id) {
            return AppError::Conflict(
                "Conversation already has an authoritative local turn owner".to_owned(),
            );
        }
        match running_orphan_disposition(&row.r#type) {
            Ok(RunningOrphanDisposition::ExternalTerminalProofRequired) => AppError::Conflict(
                "Conversation has a durable running turn whose exact Agent terminal state is not proven"
                    .to_owned(),
            ),
            Err(error) => error,
        }
    }

    /// Settle one exact accepted public background delivery after a process
    /// restart proves that no local turn owner survived.
    ///
    /// This is a recovery seam, not a send path: it never builds a runtime or
    /// sends a prompt. The public idempotency key is namespaced internally,
    /// then its immutable receipt and the aggregate's exact active operation
    /// are re-read while the per-Conversation preparation gate is held. A
    /// live exact owner is still authoritative and must be awaited. Without
    /// one, every current backend remains quarantined: parent-death teardown is
    /// not durable, queryable proof that the prior descendant tree is empty.
    pub async fn reconcile_quiescent_running_turn_for_background(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
        _runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<BackgroundTurnReconciliationDisposition, AppError> {
        let conversation_id = parse_conv_id(conversation_id)?;
        validate_public_idempotency_key(idempotency_key)?;
        let operation_id =
            Self::public_turn_operation_id(user_id, conversation_id, idempotency_key);
        let lease = self.begin_public_runtime_preparation(conversation_id, user_id)?;
        let cancellation = lease.cancellation_token();
        let _preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(conversation_id, &cancellation)
            .await?;
        lease.ensure_active()?;
        self.ensure_not_retained_execution_attempt(user_id, conversation_id)
            .await?;

        let receipt = self
            .conversation_repo
            .get_delivery_receipt(user_id, conversation_id, &operation_id)
            .await?
            .ok_or_else(|| {
                AppError::Conflict(
                    "background turn recovery lost its exact durable receipt".to_owned(),
                )
            })?;
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_id
            || receipt.operation_id != operation_id
            || receipt.kind != "turn"
        {
            return Err(AppError::Conflict(
                "background turn recovery receipt identity is invalid".to_owned(),
            ));
        }
        match receipt.status.as_str() {
            "completed" => {
                // This also repairs the crash cutpoint where the receipt became
                // completed but Finished still retained active_operation=A.
                self.adopt_completed_turn_receipt_if_still_active(
                    user_id,
                    conversation_id,
                    &receipt,
                )
                .await?;
                return Ok(
                    BackgroundTurnReconciliationDisposition::ReconciledOrTerminalReRead,
                );
            }
            "accepted" => {}
            status => {
                return Err(AppError::Conflict(format!(
                    "background turn recovery receipt has unsupported status '{status}'"
                )));
            }
        }

        let row = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        let admission = self
            .conversation_repo
            .get_turn_admission_state(user_id, conversation_id)
            .await?;
        if !matches!(row.status.as_deref(), Some("running" | "finished"))
            || admission.active_operation_id.as_deref() != Some(operation_id.as_str())
        {
            return Ok(BackgroundTurnReconciliationDisposition::StaleConflict);
        }
        if self.runtime_state.has_active_turn(conversation_id) {
            return Ok(BackgroundTurnReconciliationDisposition::LiveExactOwnerWait);
        }
        let _ = running_orphan_disposition(&row.r#type);
        Ok(BackgroundTurnReconciliationDisposition::ExternalProofRequiredFailClosed)
    }

    /// Reconcile one row selected by the exclusive process-start orphan sweep.
    ///
    /// The method deliberately re-reads all authority under the same
    /// per-Conversation preparation gate; the caller's enumeration is only a
    /// hint. Retained Agent Execution transcripts are skipped for their
    /// engine-owned recovery path and exact terminal rows are harmless no-ops.
    /// Every unresolved `Running`, or `Finished + active_operation=A`, row is
    /// quarantined until a durable terminal proof exists.
    pub async fn reconcile_locally_quiescent_orphan_on_boot(
        &self,
        user_id: &str,
        conversation_id: &str,
        _runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<QuiescentOrphanReconciliation, AppError> {
        let conversation_id = parse_conv_id(conversation_id)?;
        let lease = self.begin_public_runtime_preparation(conversation_id, user_id)?;
        let cancellation = lease.cancellation_token();
        let _preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(conversation_id, &cancellation)
            .await?;
        lease.ensure_active()?;

        let row = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        if self
            .is_execution_attempt_conversation(user_id, conversation_id)
            .await?
        {
            return Ok(QuiescentOrphanReconciliation::RetainedExecutionSkipped);
        }
        let admission = self
            .conversation_repo
            .get_turn_admission_state(user_id, conversation_id)
            .await?;
        if row.status.as_deref() != Some("running")
            && admission.active_operation_id.is_none()
        {
            return Ok(QuiescentOrphanReconciliation::AlreadyTerminal);
        }
        if !matches!(row.status.as_deref(), Some("running" | "finished")) {
            return Err(AppError::Conflict(
                "Conversation has active turn authority in a non-terminal lifecycle state"
                    .to_owned(),
            ));
        }
        lease.ensure_active()?;
        Err(self.unproven_running_generation_error(&row))
    }

    async fn release_and_complete_turn(
        &self,
        turn_handle: &mut AgentTurnHandle,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
        receipt_completion: Option<TurnReceiptCompletion>,
        durable_guard: Option<(String, u64)>,
        companion: bool,
        companion_id: Option<CompanionId>,
        origin: Option<String>,
        channel_platform: Option<String>,
    ) {
        // Keep both lifecycle fences across durable finalize -> exact release
        // -> turn.completed.  A replacement send/warmup must not observe the
        // old turn as idle between those steps, and Finished must never be
        // projected before its optional delivery receipt is settled.
        let completion_fence = match self
            .runtime_state
            .begin_turn_completion(conversation_id, turn_handle.turn_id())
        {
            Ok(Some(guard)) => guard,
            Ok(None) => {
                // Stop/delete/reset won the shared admission ordering.  Its
                // worker owns teardown and orphan finalization.  Returning
                // without an explicit release lets AgentTurnHandle::drop mark
                // the blocked owner quiesced without reopening admission.
                return;
            }
            Err(error) => {
                warn!(
                    conversation_id,
                    error = %ErrorChain(&error),
                    "Failed to acquire completion admission fence; retaining exact turn ownership"
                );
                // A poisoned lifecycle fence cannot be bypassed safely.  Keep
                // the exact turn owned forever (or until process shutdown)
                // rather than exposing an Idle state backed by a Running row.
                std::future::pending::<()>().await;
                unreachable!("pending completion fence wait returned");
            }
        };

        let preparation_token = CancellationToken::new();
        let preparation_fence = loop {
            match self
                .runtime_state
                .acquire_preparation_gate(conversation_id, &preparation_token)
                .await
            {
                Ok(guard) => break guard,
                Err(error) => {
                    error!(
                        conversation_id,
                        error = %ErrorChain(&error),
                        "Failed to acquire terminal preparation fence; retaining exact turn ownership"
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };

        // A model turn is not durably complete while one of its detached
        // knowledge write-back workers can still publish to the filesystem or
        // persist a terminal message state.  The exact completion/preparation
        // fences above exclude successor admission while this activity fence
        // drains.  Reconcile every quiesced attempt without a total timeout
        // before committing Conversation Finished or its delivery receipt.
        await_turn_writeback_quiesced(conversation_id).await;
        reconcile_quiesced_writebacks_until_resolved(
            Arc::clone(&self.conversation_repo),
            Some(Arc::clone(&self.user_events)),
            user_id,
            conversation_id,
        )
        .await;

        // There is deliberately no total business timeout here.  Every
        // individual database attempt is bounded by the repository/SQLite
        // configuration, while retry delay is capped.  Releasing on a failed
        // finalize is exactly the split-brain that allowed a completed turn to
        // be admitted and executed again.
        let mut retry_delay = Duration::from_millis(25);
        loop {
            let finalization = if let Some(completion) = receipt_completion.as_ref() {
                self.conversation_repo
                    .finalize_exact_turn_operation(
                        user_id,
                        conversation_id,
                        completion,
                        now_ms(),
                    )
                    .await
            } else {
                self.conversation_repo
                    .finalize_turn(user_id, conversation_id, None, now_ms())
                    .await
            };
            match finalization {
                Ok(TurnLifecycleTransition::Committed | TurnLifecycleTransition::AlreadyApplied) => {
                    break;
                }
                Ok(TurnLifecycleTransition::Stale) => {
                    error!(
                        conversation_id,
                        turn_id,
                        "Durable turn finalization was stale; retaining exact turn ownership"
                    );
                }
                Err(error) => {
                    error!(
                        conversation_id,
                        turn_id,
                        error = %ErrorChain(&error),
                        "Durable turn finalization failed; retaining exact turn ownership and retrying"
                    );
                }
            }

            // A stop/orphan owner may have won after this local completion
            // fence was acquired (for example from a successor process). Do
            // not retry an old finalizer against a replacement Running
            // generation forever. Re-read durable authority on every failed
            // attempt and retire this exact local handle once its receipt is
            // terminal or its active operation no longer matches.
            let no_longer_owns_generation = if let Some(completion) =
                receipt_completion.as_ref()
            {
                match self
                    .conversation_repo
                    .get_delivery_receipt(
                        user_id,
                        conversation_id,
                        &completion.operation_id,
                    )
                    .await
                {
                    Ok(Some(receipt))
                        if receipt.user_id == user_id
                            && receipt.conversation_id == conversation_id
                            && receipt.kind == completion.kind
                            && receipt.request_payload == completion.request_payload
                            && receipt.status == "completed" =>
                    {
                        matches!(
                            self.conversation_repo
                                .get_turn_admission_state(
                                    user_id,
                                    conversation_id,
                                )
                                .await,
                            Ok(state)
                                if state.active_operation_id.as_deref()
                                    != Some(completion.operation_id.as_str())
                        )
                    }
                    Ok(Some(receipt))
                        if receipt.user_id == user_id
                            && receipt.conversation_id == conversation_id
                            && receipt.kind == completion.kind
                            && receipt.request_payload == completion.request_payload =>
                    {
                        // Accepted but displaced is not sufficient to release:
                        // the exact repository command must first absorb that
                        // receipt without touching the replacement row.
                        false
                    }
                    // A missing receipt is an ambiguous legacy crash window,
                    // never evidence that no external effect occurred.
                    Ok(None) => false,
                    // Identity mismatch is corruption, not proof that it is
                    // safe to release the current execution fence.
                    Ok(Some(_)) | Err(_) => false,
                }
            } else {
                match self
                    .conversation_repo
                    .get_turn_admission_state(user_id, conversation_id)
                    .await
                {
                    // An unkeyed legacy finalizer owns only the legacy
                    // active-null generation. Never let it touch a keyed
                    // replacement turn.
                    Ok(state) => state.active_operation_id.is_some(),
                    Err(_) => false,
                }
            };
            if no_longer_owns_generation {
                if let Some((guard_key, generation)) = durable_guard.as_ref() {
                    Self::release_durable_operation_guard(
                        &self.durable_operations_in_flight,
                        guard_key,
                        *generation,
                    );
                }
                // Do not publish another terminal event: the durable winner
                // (stop/orphan or a successor generation) owns that event.
                let turn_generation = turn_handle.turn_id();
                if turn_handle.release() {
                    Self::release_runtime_turn_until_confirmed(
                        runtime_registry,
                        conversation_id,
                        turn_generation,
                    )
                    .await;
                }
                drop(preparation_fence);
                drop(completion_fence);
                return;
            }
            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(Duration::from_secs(2));
        }

        if let Some((guard_key, generation)) = durable_guard.as_ref() {
            Self::release_durable_operation_guard(
                &self.durable_operations_in_flight,
                guard_key,
                *generation,
            );
        }
        let turn_generation = turn_handle.turn_id();
        if !turn_handle.release() {
            // Durable Finished is already authoritative.  A concurrent
            // cleanup owner may have closed the exact release gate; it alone
            // publishes the matching terminal event.
            drop(preparation_fence);
            drop(completion_fence);
            return;
        }
        Self::release_runtime_turn_until_confirmed(
            runtime_registry,
            conversation_id,
            turn_generation,
        )
        .await;
        let allowed_completion_owners = 1;
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
                    drop(preparation_fence);
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
    /// Generates a canonical bare UUIDv7 ID, sets status to `pending`, defaults
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
        self.delete(user_id, &conversation.conversation_id).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, agent_type = ?req.r#type))]
    async fn create_inner(
        &self,
        user_id: &str,
        mut req: CreateConversationRequest,
        trusted_snapshot: Option<nomifun_api_types::ResolvedPresetSnapshot>,
        creation_key: Option<&str>,
    ) -> Result<ConversationResponse, AppError> {
        reject_backend_owned_lifecycle_extra_keys(&req.extra)?;
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

        // Type-aware v3 rule: top-level `model` is nomi-only. Other agent types
        // carry model/mode via `extra` (see spec 2026-05-12). Reject the
        // noncanonical wire shape instead of silently writing an unused column.
        if req.r#type != AgentType::Nomi && req.model.is_some() {
            return Err(AppError::BadRequest(format!(
                "top-level `model` is only accepted for nomi conversations; pass model via `extra` for {}",
                req.r#type.serde_name()
            )));
        }
        let requested_model = req.model.clone();
        let requested_model_pool = req.execution_model_pool.clone();

        let mut extra = req.extra;
        reject_execution_policy_extra_keys(&extra)?;
        reject_retired_skill_extra_keys(&extra)?;
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
            if let Some(agent_id) = snapshot.resolved_agent_id.as_ref() {
                let agent = self
                    .agent_metadata_repo
                    .get(agent_id)
                    .await?
                    .ok_or_else(|| {
                        AppError::BadRequest(format!(
                            "preset resolved missing agent '{agent_id}'"
                        ))
                    })?;
                req.r#type = serde_json::from_value(serde_json::Value::String(agent.agent_type.clone()))
                    .map_err(|_| AppError::BadRequest(format!("preset resolved unknown agent type '{}'", agent.agent_type)))?;
                if let Some(obj) = extra.as_object_mut() {
                    obj.insert("agent_id".into(), serde_json::Value::String(agent.agent_id));
                    if let Some(backend) = agent.backend {
                        obj.insert("backend".into(), serde_json::Value::String(backend));
                    }
                    obj.insert(
                        "agent_source".into(),
                        serde_json::Value::String(agent.agent_source),
                    );
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
                    let resolved_lead = ExecutionModelRef {
                        provider_id: provider_id.clone(),
                        model: model.model.clone(),
                    };
                    req.execution_model_pool = reconcile_preset_conversation_model_pool(
                        requested_model_pool.clone(),
                        requested_model.as_ref(),
                        &resolved_lead,
                    )?;
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
                // The immutable first-class `preset_snapshot` column is the
                // only authority for runtime instructions.  Do not persist a
                // second copy in this open JSON bag: build_runtime_options
                // projects the snapshot into the adapter-specific prompt
                // field on every fresh runtime build.
                obj.remove("preset_rules");
                obj.remove("preset_context");
                obj.insert("preset_enabled_skills".into(), serde_json::to_value(&snapshot.included_skills).unwrap_or_default());
                obj.insert("exclude_auto_inject_skills".into(), serde_json::to_value(&snapshot.excluded_auto_skills).unwrap_or_default());
                obj.insert("preset_knowledge_binding".into(), serde_json::Value::Bool(true));
            }
        }

        // Nomi has exactly one model source: the typed top-level `model`.
        // Reject a second representation instead of normalizing an obsolete
        // payload shape.
        if req.r#type == AgentType::Nomi
            && extra
                .as_object()
                .is_some_and(|object| object.contains_key("model"))
        {
            return Err(AppError::BadRequest(
                "extra.model is not part of the v3 Nomi contract; use top-level model".to_owned(),
            ));
        }

        // V3 ACP identity is row-scoped and explicit. A backend label is
        // descriptive metadata, never a lookup key or a substitute for the
        // catalog business ID. Validate the logical parent before creating the
        // Conversation row so an invalid agent cannot leave a half-created
        // aggregate behind.
        let acp_agent = if req.r#type == AgentType::Acp {
            let agent_id =
                required_trimmed_extra_string(&extra, "agent_id", "ACP conversation")?;
            let agent = self
                .agent_metadata_repo
                .get(agent_id)
                .await
                .map_err(|error| AppError::Internal(format!("agent_metadata lookup: {error}")))?
                .ok_or_else(|| {
                    AppError::BadRequest(format!(
                        "ACP extra.agent_id '{agent_id}' does not exist"
                    ))
                })?;
            validate_acp_agent_metadata_row(&agent, &extra)?;
            if let Some(object) = extra.as_object_mut() {
                match agent.backend.as_ref() {
                    Some(backend) => {
                        object.insert(
                            "backend".to_owned(),
                            serde_json::Value::String(backend.clone()),
                        );
                    }
                    None => {
                        object.remove("backend");
                    }
                }
                object.insert(
                    "agent_source".to_owned(),
                    serde_json::Value::String(agent.agent_source.clone()),
                );
            }
            Some(agent)
        } else {
            None
        };

        // Determine whether the user chose this workspace ("custom") or we
        // auto-provision one under `{work_dir}/conversations/{uuidv7}/`.
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
            let (temp_workspace_id, probe_path) =
                allocate_temp_workspace_id(&self.workspace_root);
            if workspace_path_has_edge_whitespace_segment(&probe_path) {
                return Err(AppError::WorkspacePathEdgeWhitespace(probe_path.display().to_string()));
            }
            Some(temp_workspace_id)
        } else {
            None
        };

        // Strip request-only / derived workspace flags and then stamp the
        // backend-owned temp workspace token for auto-provisioned sessions.
        if let Some(obj) = extra.as_object_mut() {
            obj.remove("custom_workspace");
            obj.remove("is_temporary_workspace");
            obj.remove(TEMP_WORKSPACE_ID_EXTRA_KEY);
            if let Some(temp_workspace_id) = auto_workspace.as_ref() {
                obj.insert(
                    TEMP_WORKSPACE_ID_EXTRA_KEY.to_owned(),
                    serde_json::Value::String(temp_workspace_id.clone()),
                );
            }
        }

        // Consume the canonical transient skill-shaping inputs and freeze the
        // initial immutable snapshot into `extra.skills`. No historical aliases
        // are accepted or normalized in the v3 contract.
        let (preset_enabled, exclude_auto_inject) = match extra.as_object_mut() {
            Some(obj) => {
                let preset = take_string_array(obj, "preset_enabled_skills")?;
                let exclude = take_string_array(obj, "exclude_auto_inject_skills")?;
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
        // junction after the row exists so the logical parent is already addressable.
        let selected_mcp_server_ids: Option<Vec<String>> = match extra.as_object_mut() {
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
        let mut resolved_mcp_server_ids: Vec<String> = Vec::new();
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
                    Some(ids) => ids.iter().any(|id| id == &row.mcp_server_id),
                    None => row.enabled,
                })
                .collect::<Vec<_>>();
            resolved_mcp_server_ids = selected_rows
                .iter()
                .map(|row| row.mcp_server_id.clone())
                .collect();
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
            // reads stable UUIDv7 business IDs from `extra.mcp_server_ids`.
            obj.insert(
                "mcp_server_ids".to_owned(),
                serde_json::Value::Array(
                    resolved_mcp_server_ids
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
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

        // `cron_job_id` is a request-only logical reference. Consume the one
        // canonical snake_case field and persist it only in the typed column;
        // aliases and non-canonical IDs are rejected rather than creating a
        // second durable representation in `extra`.
        let cron_job_id = match extra.as_object_mut() {
            Some(object) => {
                if object.contains_key("cronJobId") {
                    return Err(AppError::BadRequest(
                        "cronJobId is not supported; use cron_job_id".to_owned(),
                    ));
                }
                match object.remove("cron_job_id") {
                    None => None,
                    Some(value) => {
                        let value = value.as_str().ok_or_else(|| {
                            AppError::BadRequest(
                                "cron_job_id must be a canonical UUIDv7 string".to_owned(),
                            )
                        })?;
                        Some(
                            CronJobId::parse(value)
                                .map_err(|error| {
                                    AppError::BadRequest(format!(
                                        "cron_job_id must be a canonical UUIDv7 string: {error}"
                                    ))
                                })?
                                .into_string(),
                        )
                    }
                }
            }
            None => None,
        };
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

        let conversation_id = ConversationId::new().into_string();
        let row = nomifun_db::models::ConversationRow {
            id: 0,
            conversation_id: conversation_id.clone(),
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

        let managed_workspace = auto_workspace
            .as_ref()
            .map(|temp_workspace_id| auto_temp_workspace_path(&self.workspace_root, temp_workspace_id));
        let materialized = async {
            // Now that the row exists, provision the auto (temp) workspace:
            // create the tokenized directory, wire skill symlinks (temp
            // workspaces only), record the path in `extra`, and persist the
            // updated `extra` back. User-supplied workspaces are left
            // untouched.
            if let Some(ws_path) = managed_workspace.as_ref() {
                std::fs::create_dir_all(ws_path)
                    .map_err(|e| AppError::Internal(format!("Failed to create workspace: {e}")))?;

                if !is_custom_workspace
                    && !skills_for_links.is_empty()
                    && let Some(rel_dirs) =
                        native_skills_dirs(&req.r#type, acp_agent.as_ref())
                {
                    let resolved = self.skill_resolver.resolve_skills(&skills_for_links).await;
                    if !resolved.is_empty() {
                        let rel_dirs_refs: Vec<&str> =
                            rel_dirs.iter().map(String::as_str).collect();
                        let n = self
                            .skill_resolver
                            .link_workspace_skills(ws_path, &rel_dirs_refs, &resolved)
                            .await;
                        debug!(
                            conversation_id = new_id,
                            workspace = %ws_path.display(),
                            links = n,
                            "wired skill symlinks into workspace"
                        );
                    }
                }

                extra["workspace"] =
                    serde_json::Value::String(ws_path.to_string_lossy().into_owned());
                let extra_json = serde_json::to_string(&extra)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?;
                let workspace_update = ConversationRowUpdate {
                    extra: Some(extra_json),
                    updated_at: Some(now),
                    ..Default::default()
                };
                self.conversation_repo
                    .update(&new_id, &workspace_update)
                    .await?;
            }

            // The v3 junction is the only durable MCP selection store. A
            // failed write invalidates the aggregate; warning-and-continuing
            // would return a Conversation whose runtime snapshot and database
            // selection disagree.
            if selected_mcp_server_ids.is_some() {
                self.conversation_repo
                    .set_mcp_server_ids(&new_id, &resolved_mcp_server_ids)
                    .await?;
            }

            // ACP conversations own one logical 1:1 acp_session child.
            if let Some(agent) = acp_agent.as_ref() {
                self.create_acp_session_row(&new_id, &extra, agent).await?;
            }

            // Build the response before the final cross-domain binding write
            // so no fallible Conversation operation remains after a
            // companion-scoped preset binding is updated.
            let response_row = nomifun_db::models::ConversationRow {
                id: row.id,
                conversation_id: new_id.clone(),
                extra: serde_json::to_string(&extra)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?,
                ..row
            };
            let mut response = row_to_response(response_row, &self.workspace_root)?;
            self.project_execution_relation(user_id, &mut response).await?;

            // Materialize the preset's knowledge policy last. This
            // deliberately bypasses workpath inheritance at runtime:
            // selecting a preset must reproduce its KB scope without silently
            // sharing the user's general workspace binding.
            if let Some(snapshot) = resolved_preset_snapshot.as_ref()
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
                                .clone()
                                .unwrap_or_else(|| "conservative".to_owned()),
                            // Presets never self-authorize unattended channel writes.
                            channel_write_enabled: false,
                            kb_ids: snapshot.knowledge_base_ids.clone(),
                        },
                    )
                    .await?;
            }

            Ok(response)
        }
        .await;
        let response = match materialized {
            Ok(response) => response,
            Err(error) => {
                return Err(
                    self.compensate_failed_creation(
                        &new_id,
                        managed_workspace.as_deref(),
                        error,
                    )
                    .await,
                );
            }
        };

        if let Some(snapshot) = resolved_preset_snapshot.as_ref()
            && let Some(service) = self
                .preset_service
                .read()
                .ok()
                .and_then(|guard| guard.as_ref().cloned())
            && let Err(error) = service.mark_used(&snapshot.preset_id, now).await
        {
            // Usage ordering is secondary catalog metadata. The conversation
            // and its immutable snapshot are already fully materialized, so a
            // transient state-write failure must not invalidate the aggregate.
            warn!(
                conversation_id = %new_id,
                preset_id = %snapshot.preset_id,
                error = %ErrorChain(&error),
                "failed to update preset last_used_at"
            );
        }

        self.broadcast_list_changed(user_id, &new_id, "created", response.source.as_ref());

        log_conversation_created(&response, &extra);

        Ok(response)
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    async fn create_acp_session_row(
        &self,
        conversation_id: &str,
        extra: &serde_json::Value,
        agent: &AgentMetadataRow,
    ) -> Result<(), AppError> {
        debug!("Creating acp_session row");

        let conv_id = parse_conv_id(conversation_id)?;
        validate_acp_agent_metadata_row(agent, extra)?;

        let params = CreateAcpSessionParams {
            conversation_id: conv_id,
            agent_backend: agent.backend.as_deref().unwrap_or_default(),
            agent_source: &agent.agent_source,
            agent_id: &agent.agent_id,
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

        let extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|e| AppError::Internal(format!("Invalid extra JSON: {e}")))?;
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

        // V3 is reset-only and has no historical-row compatibility path.
        // Persisted corruption fails the request instead of silently dropping
        // rows while still reporting the repository's original total.
        let mut items = Vec::with_capacity(result.items.len());
        for mut row in result.items {
            rebase_managed_workspace_in_row(&mut row, &self.workspace_root)?;
            let conversation_id = row.conversation_id.clone();
            let extra: serde_json::Value = serde_json::from_str(&row.extra).map_err(|error| {
                AppError::Internal(format!(
                    "Conversation {conversation_id} has invalid extra JSON: {error}"
                ))
            })?;
            let mut response =
                row_to_response_with_extra(row, extra, &self.workspace_root)?;
            self.project_execution_relation(user_id, &mut response).await?;
            items.push(response);
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
        if let Some(incoming) = req.extra.as_ref() {
            reject_backend_owned_lifecycle_extra_keys(incoming)?;
        }

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
        self.ensure_not_retained_execution_attempt(user_id, &existing.conversation_id)
            .await?;
        self.ensure_no_ambiguous_edit_resubmit(
            user_id,
            &existing.conversation_id,
        )
        .await?;

        // Snapshot invariant: once written at create time, `extra.skills`
        // must not be re-shaped by PATCH. The frontend must clone the
        // conversation to produce a new snapshot.
        if let Some(incoming) = &req.extra
            && (incoming.get("skills").is_some()
                || incoming.get("preset_id").is_some()
                || incoming.get("preset_revision").is_some()
                || incoming.get("preset_snapshot").is_some()
                || incoming.get("preset_enabled_skills").is_some()
                || incoming.get("exclude_auto_inject_skills").is_some()
                || incoming.get("preset_rules").is_some()
                || incoming.get("preset_context").is_some()
                || incoming.get("preset_knowledge_binding").is_some()
                || incoming.get("preset_instructions_embedded").is_some()
                || incoming.get("mcp_server_ids").is_some()
                || incoming.get("mcp_servers").is_some()
                || incoming.get("mcp_statuses").is_some()
                || incoming.get("session_mcp_servers").is_some())
        {
            return Err(AppError::BadRequest(
                "preset, skill and MCP snapshots are immutable post-creation".into(),
            ));
        }
        if let Some(incoming) = &req.extra {
            reject_execution_policy_extra_keys(incoming)?;
            reject_retired_skill_extra_keys(incoming)?;
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
        if existing_type == AgentType::Acp
            && let Some(incoming) = req.extra.as_ref()
        {
            // The conversation row and its 1:1 acp_session row must always
            // point at the same logical agent parent. Agent replacement is an
            // aggregate replacement, not a JSON patch.
            reject_acp_identity_patch(incoming)?;
        }

        let now = now_ms();

        if existing_type == AgentType::Nomi
            && req
                .extra
                .as_ref()
                .and_then(serde_json::Value::as_object)
                .is_some_and(|object| object.contains_key("model"))
        {
            return Err(AppError::BadRequest(
                "extra.model is not part of the v3 Nomi contract; use top-level model".to_owned(),
            ));
        }

        // Merge canonical extra fields. Nomi model selection is never carried
        // in this open JSON object.
        let merged_extra = if let Some(new_extra) = &req.extra {
            let mut existing_extra: serde_json::Value =
                serde_json::from_str(&existing.extra).map_err(|error| {
                    AppError::Internal(format!(
                        "Conversation {id} has invalid extra JSON: {error}"
                    ))
                })?;
            if !existing_extra.is_object() {
                return Err(AppError::Internal(format!(
                    "Conversation {id} extra must be a JSON object"
                )));
            }
            merge_json(&mut existing_extra, new_extra);
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

        let requested_model_json = req
            .model
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                AppError::Internal(format!("Failed to serialize model: {error}"))
            })?;
        let model_changed = requested_model_json
            .as_deref()
            .is_some_and(|new_json| existing.model.as_deref() != Some(new_json));

        // A workspace repoint (e.g. binding a temporary session to a real
        // project directory) changes the agent's cwd — and, via the surface
        // scope, its native/gateway file authority. The cached agent baked the
        // old cwd at build time, so it must be recycled for the change to take
        // effect on the next message (same rationale as the model-change termination
        // below). Detected by comparing the pre/post merged `extra.workspace`.
        let existing_extra_value: serde_json::Value =
            serde_json::from_str(&existing.extra).map_err(|error| {
                AppError::Internal(format!(
                    "Conversation {id} has invalid extra JSON: {error}"
                ))
            })?;
        let merged_extra_value = merged_extra
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()
            .map_err(|error| {
                AppError::Internal(format!(
                    "Conversation {id} merged extra is invalid JSON: {error}"
                ))
            })?;
        let workspace_changed = req
            .extra
            .as_ref()
            .is_some_and(|extra| extra.get("workspace").is_some())
            && {
                let ws_of = |value: &serde_json::Value| {
                    value
                        .get("workspace")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                };
                ws_of(&existing_extra_value)
                    != merged_extra_value.as_ref().and_then(ws_of)
            };
        let model_json = requested_model_json.map(Some);

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
            // Preset lineage is a frozen creation-time contract. Applying a
            // different preset means creating/cloning a new conversation.
            preset_id: None,
            preset_revision: None,
            preset_snapshot: None,
            updated_at: Some(now),
        };

        self.conversation_repo.update(parse_conv_id(id)?, &updates).await?;

        if model_changed || workspace_changed || delegation_policy_changed {
            info!(
                model_changed,
                workspace_changed,
                delegation_policy_changed,
                "Conversation updated, terminating Agent runtime so the change takes effect on the next message"
            );
            Self::terminate_runtime_with_proof(
                runtime_registry,
                id,
                AgentKillReason::AgentErrorRecovery,
                "conversation configuration update",
            )
            .await?;
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
        reject_backend_owned_lifecycle_extra_keys(&patch)?;
        reject_execution_policy_extra_keys(&patch)?;
        reject_retired_skill_extra_keys(&patch)?;

        let existing = self
            .conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;
        if string_to_enum::<AgentType>(&existing.r#type)? == AgentType::Acp {
            reject_acp_identity_patch(&patch)?;
        }

        let mut merged: serde_json::Value =
            serde_json::from_str(&existing.extra).map_err(|error| {
                AppError::Internal(format!(
                    "Conversation {conversation_id} has invalid extra JSON: {error}"
                ))
            })?;
        if !merged.is_object() {
            return Err(AppError::Internal(format!(
                "Conversation {conversation_id} extra must be a JSON object"
            )));
        }
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
        let deletion_cancelled_build_ids = deletion_guard.cancelled_build_ids().to_vec();
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
            // The deletion tombstone's first synchronous boundary is the only
            // lossless capture point. A pre-existing completion owner can make
            // the stop coordinator a follower, and completion does not own
            // runtime-preparation leases. Always await the first capture in
            // parallel with stop so neither that follower path nor a later
            // recapture can mistake cleared cleanup fences for build
            // quiescence.
            let first_capture_cleanup = async {
                service
                    .await_cancelled_runtime_builds_quiesced(
                        &conversation_id,
                        &deletion_cancelled_build_ids,
                        "conversation deletion",
                    )
                    .await;
                service.runtime_state.forget_cancelled_runtime_builds(
                    &conversation_id,
                    &deletion_cancelled_build_ids,
                );
            };
            let stop_cleanup = async {
                match stop_rx.await {
                    Ok(result) => result,
                    Err(_) => Err(AppError::Internal(
                        "conversation delete stop coordinator exited without proving quiescence"
                            .to_owned(),
                    )),
                }
            };
            let ((), stop_result) = tokio::join!(first_capture_cleanup, stop_cleanup);
            if let Err(error) = stop_result {
                let _ = result_tx.send(Err(error));
                return;
            }
            // A deletion-owned stop may have joined a normal-completion fence
            // before that completion left an intentionally reusable idle
            // runtime. Definitive deletion must evict that slot/process and
            // clear registry governor state even when the first stop was only
            // a follower.
            Self::terminate_runtime_until_confirmed(
                &service.runtime_registry,
                &conversation_id,
                AgentKillReason::ConversationDeleted,
                "conversation deletion",
            )
            .await;

            // The request waiter has its own hard bound, but this detached
            // owner must let the explicit database transaction finish. Dropping
            // the repository future on an inner timeout would lose the
            // captured Cron IDs needed for post-commit scheduler/file cleanup.
            let delete_cleanup = match service
                .conversation_repo
                .delete_with_cleanup(&conversation_id)
                .await
            {
                Ok(cleanup) => cleanup,
                Err(error) => {
                    let _ = result_tx.send(Err(error.into()));
                    return;
                }
            };

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

            // A backend-managed workspace is part of the conversation's
            // durable lifecycle, not an optional after-effect. Do not
            // acknowledge deletion while its files are still locatable: that
            // race could make stale artifacts appear to survive a successful
            // delete. Optional repository/hooks remain detached below.
            let mut managed_workspace_cleanup_error = None;
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
                    Ok(Ok(Err(err))) => {
                        warn!(
                            conversation_id,
                            workspace = %display_path,
                            error = %ErrorChain(&err),
                            "Failed to remove managed temp workspace on conversation delete"
                        );
                        managed_workspace_cleanup_error = Some(format!(
                            "conversation was deleted, but managed workspace '{display_path}' could not be removed: {err}"
                        ));
                    }
                    Ok(Err(err)) => {
                        warn!(
                            conversation_id,
                            workspace = %display_path,
                            error = %ErrorChain(&err),
                            "Managed temp workspace cleanup task failed"
                        );
                        managed_workspace_cleanup_error = Some(format!(
                            "conversation was deleted, but managed workspace '{display_path}' cleanup failed: {err}"
                        ));
                    }
                    Err(_) => {
                        warn!(
                            conversation_id,
                            workspace = %display_path,
                            "Managed temp workspace cleanup exceeded its hard bound"
                        );
                        managed_workspace_cleanup_error = Some(format!(
                            "conversation was deleted, but managed workspace '{display_path}' cleanup timed out"
                        ));
                    }
                }
            }

            // If the HTTP waiter timed out/disconnected, this send simply
            // fails while the committed tombstone and optional cleanup remain
            // owned by this detached task.
            let _ = result_tx.send(match managed_workspace_cleanup_error {
                Some(error) => Err(AppError::Internal(error)),
                None => Ok(()),
            });

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

            let deleted_cron_job_ids: Arc<[String]> = delete_cleanup.into();
            for hook in hooks {
                let hook_timed_out = DELETED_CRON_JOB_IDS
                    .scope(Arc::clone(&deleted_cron_job_ids), async {
                        tokio::time::timeout(
                            DELETE_CLEANUP_ITEM_GRACE,
                            hook.on_conversation_deleted(&user_id, &conversation_id),
                        )
                        .await
                        .is_err()
                    })
                    .await;
                if hook_timed_out {
                    warn!(conversation_id, "Conversation delete hook exceeded its hard bound");
                }
            }

        });
        result_rx
    }

    /// Delete a conversation and run repository-owned logical-reference cleanup.
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

        // Deletion cannot be used as a back door around restart-orphan
        // quarantine. With no exact in-memory turn owner, a durable Running
        // row may still have descendants executing under the prior process.
        // Reject before installing the deletion tombstone or removing any
        // transcript/receipt rows.
        if existing.status.as_deref() == Some("running")
            && !self.runtime_state.has_active_turn(id)
            && running_orphan_disposition(&existing.r#type)?
                == RunningOrphanDisposition::ExternalTerminalProofRequired
        {
            return Err(AppError::Conflict(
                "Conversation has an unproven running turn from a prior process; deletion requires exact process-empty proof"
                    .to_owned(),
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
            .map(string_to_enum::<ConversationSource>)
            .transpose()?;
        let managed_temp_workspace =
            managed_temp_workspace_path_from_row(&self.workspace_root, &existing)?;

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
    /// Source-conversation inheritance (name, `extra`, or cron binding) is not
    /// supported. This method remains the boundary for active callers of
    /// `POST /api/conversations/clone` that already provide a complete payload;
    /// new code should prefer `create`.
    pub async fn clone_create(
        &self,
        user_id: &str,
        mut req: CloneConversationRequest,
    ) -> Result<ConversationResponse, AppError> {
        strip_clone_instance_state(&mut req.conversation.extra);
        self.create(user_id, req.conversation).await
    }

    /// Reset a terminal conversation to a fresh pending aggregate.
    ///
    /// Reset participates in the same per-conversation preparation and
    /// lifecycle admission fences as send, warmup, stop, deletion, and normal
    /// completion. The fences stay held until every older runtime build has
    /// been cancelled, the cached runtime has provably exited, persisted
    /// runtime-resume state has been invalidated, and the transcript/artifact
    /// reset has committed atomically.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn reset(&self, user_id: &str, id: &str) -> Result<(), AppError> {
        let conv_id = parse_conv_id(id)?;
        // Reject unauthorized/retained requests before taking lifecycle
        // ownership or disturbing any live runtime.
        self.conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;

        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;

        // Serialize with the complete read -> mount reconciliation -> runtime
        // build/turn-admission path. A fresh token is intentionally never
        // cancelled: reset is the owner that cancels older work.
        let preparation_token = CancellationToken::new();
        let preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(id, &preparation_token)
            .await?;
        let reset_guard = self.runtime_state.begin_conversation_reset(id)?;

        // Ownership and retention may have changed while waiting for the
        // preparation gate. Revalidate under reset admission ownership before
        // performing any teardown or durable mutation.
        let reset_row = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {id} not found")))?;
        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;
        if !matches!(reset_row.status.as_deref(), Some("pending" | "finished")) {
            return Err(AppError::Conflict(format!(
                "Conversation {id} is not in a resettable terminal state"
            )));
        }
        let cancelled_build_ids = reset_guard.cancelled_build_ids().to_vec();
        self.await_cancelled_runtime_builds_quiesced(
            id,
            &cancelled_build_ids,
            "conversation reset",
        )
        .await;
        self.runtime_state
            .forget_cancelled_runtime_builds(id, &cancelled_build_ids);
        self.cancel_and_wait_for_turn_writebacks(conv_id).await?;

        // A successful reset must never leave an old process/session able to
        // emit or resume after admission reopens. The result-bearing barrier is
        // fail-closed: teardown errors leave durable history/status untouched.
        self.runtime_registry
            .terminate_and_wait_result(id, Some(AgentKillReason::UserCancelled))
            .await?;
        if reset_row.r#type == AgentType::Nomi.serde_name() {
            self.runtime_registry
                .reset_persisted_nomi_session(id, reset_row.created_at)
                .await?;
        }
        self.runtime_state.clear_knowledge_signature(id);
        self.runtime_state.clear_turn_tokens(id);

        // The repository transaction also clears ACP resume identity/context
        // usage and absorbs accepted turn receipts. Keeping those mutations in
        // the final CAS prevents a failed reset from splitting session state
        // away from the still-authoritative transcript/status.
        match self
            .conversation_repo
            .reset_terminal_conversation(user_id, conv_id, now_ms())
            .await?
        {
            TurnLifecycleTransition::Committed | TurnLifecycleTransition::AlreadyApplied => {}
            TurnLifecycleTransition::Stale => {
                return Err(AppError::Conflict(format!(
                    "Conversation {id} lifecycle changed while resetting"
                )));
            }
        }

        // Keep both fences through the durable commit, then release the inner
        // lifecycle tombstone while the outer preparation gate still excludes
        // every send/warmup waiter. Opening the outer gate last avoids even a
        // transient reset-conflict response after the commit is authoritative.
        drop(reset_guard);
        drop(preparation_guard);

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
        let cron_job_id = CronJobId::parse(cron_job_id)
            .map_err(|error| AppError::BadRequest(format!("invalid cron_job_id: {error}")))?;
        let rows = self
            .conversation_repo
            .list_by_cron_job(user_id, cron_job_id.as_str())
            .await?;
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
    /// Recheck local artifact receipts immediately before returning persisted
    /// messages to a history consumer. Filesystem/container validation is
    /// blocking and can read large artifacts, so the whole page is processed on
    /// Tokio's blocking pool rather than occupying an async request worker.
    async fn project_history_artifact_integrity(
        &self,
        conversation: &ConversationRow,
        mut items: Vec<MessageResponse>,
    ) -> Result<Vec<MessageResponse>, AppError> {
        if !items.iter().any(message_needs_artifact_history_audit) {
            return Ok(items);
        }

        let workspace = match history_artifact_workspace(&self.workspace_root, conversation) {
            Ok(workspace) => Some(workspace),
            Err(reason) => {
                // Missing/stale workspace context is not a reason to fail the
                // entire transcript request, but it can never authorize a
                // green local-artifact projection. Passing `None` below
                // deterministically downgrades every affected tool message.
                warn!(
                    conversation_id = %conversation.conversation_id,
                    %reason,
                    "Conversation history has local artifact receipts without a usable workspace"
                );
                None
            }
        };
        let conversation_id = conversation.conversation_id.clone();
        let (projected, invalidated) = tokio::task::spawn_blocking(move || {
            let store = workspace.map(ArtifactStore::new);
            let mut invalidated = 0usize;
            for message in &mut items {
                if project_historical_artifact_integrity(message, store.as_ref()) {
                    invalidated += 1;
                }
            }
            (items, invalidated)
        })
        .await
        .map_err(|error| {
            // Returning no history is safer than returning the unverified
            // original page after a verifier task failed unexpectedly.
            AppError::Internal(format!(
                "Conversation artifact history verification failed: {error}"
            ))
        })?;

        if invalidated > 0 {
            warn!(
                %conversation_id,
                invalidated,
                "Downgraded stale or unverifiable historical artifact deliveries"
            );
        }
        Ok(projected)
    }

    /// List messages for a conversation with page-based pagination.
    pub async fn list_messages(
        &self,
        user_id: &str,
        conversation_id: &str,
        query: ListMessagesQuery,
    ) -> Result<MessageListResponse, AppError> {
        // Verify conversation exists and belongs to user
        let conversation = self
            .conversation_repo
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
        // cursor from items[0]. `cursor: None` keeps the offset-pagination path
        // for callers that have not opted into keyset pagination.
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
            let items = self
                .project_history_artifact_integrity(&conversation, items)
                .await?;
            let items = items
                .into_iter()
                .map(|message| self.project_orphaned_turn_writeback(message))
                .collect();
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

        let items = self
            .project_history_artifact_integrity(&conversation, items)
            .await?;
        let items = items
            .into_iter()
            .map(|message| self.project_orphaned_turn_writeback(message))
            .collect();
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
        let conversation = self
            .conversation_repo
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
        let mut responses = self
            .project_history_artifact_integrity(
                &conversation,
                vec![row_to_message_response(row)?],
            )
            .await?;
        let response = responses
            .pop()
            .ok_or_else(|| AppError::Internal("Message history projection returned no item".into()))?;
        let response = self.project_orphaned_turn_writeback(response);
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

    /// Start exactly one user-requested retry of a failed turn-final knowledge
    /// write-back. This intentionally does not enqueue or persist a job: the
    /// detached worker owns one attempt, and another retry remains an explicit
    /// user action after any terminal failure.
    pub async fn retry_knowledge_writeback(
        &self,
        user_id: &str,
        conversation_id: &str,
        message_id: &str,
        expected_attempt_id: &str,
    ) -> Result<(), AppError> {
        let conversation_id = parse_conv_id(conversation_id)?;
        let message_id = parse_message_id(message_id)?;
        let preparation_lease =
            self.begin_public_runtime_preparation(conversation_id, user_id)?;
        #[cfg(test)]
        self.reach_public_admission_cutpoint(
            PublicAdmissionCutpoint::AfterWritebackRetryPreparationLease,
        )
        .await;
        preparation_lease.ensure_active()?;
        let preparation_token = preparation_lease.cancellation_token();
        let _preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(conversation_id, &preparation_token)
            .await?;
        preparation_lease.ensure_active()?;
        self.recover_unadmitted_edit_resubmit_reservation_under_gate(
            user_id,
            conversation_id,
        )
        .await?;
        preparation_lease.ensure_active()?;
        self.ensure_no_ambiguous_edit_resubmit(user_id, conversation_id)
            .await?;
        preparation_lease.ensure_active()?;

        let conversation = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;
        preparation_lease.ensure_active()?;
        let admission = self
            .conversation_repo
            .get_turn_admission_state(user_id, conversation_id)
            .await?;
        preparation_lease.ensure_active()?;
        let registered_runtime =
            self.runtime_registry.has_registered_runtime(conversation_id);
        let runtime = self.runtime_registry.get_runtime(conversation_id);
        if conversation.status.as_deref() != Some("finished")
            || admission.active_operation_id.is_some()
            || self.runtime_state.has_active_turn(conversation_id)
            || runtime
                .as_ref()
                .is_some_and(|runtime| runtime.status() == Some(ConversationStatus::Running))
            || (registered_runtime && runtime.is_none())
        {
            return Err(AppError::Conflict(
                "Knowledge write-back retry requires an exact, quiescent Finished conversation"
                    .into(),
            ));
        }
        let mut assistant = self
            .conversation_repo
            .get_message(conversation_id, message_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Message {message_id} not found")))?;
        preparation_lease.ensure_active()?;
        if assistant.r#type != "text"
            || assistant.position.as_deref() != Some("left")
            || assistant.status.as_deref() != Some("finish")
        {
            return Err(AppError::Conflict(
                "Only a completed assistant text response can retry knowledge write-back".into(),
            ));
        }
        let mut assistant_content: serde_json::Value = serde_json::from_str(&assistant.content)
            .map_err(|error| AppError::Internal(format!("Invalid assistant message content JSON: {error}")))?;
        let initial_state = assistant_content
            .get("knowledge_writeback")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| AppError::Conflict("Message has no knowledge write-back attempt".into()))?;
        if initial_state
            .get("attempt_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            != expected_attempt_id
        {
            return Err(AppError::Conflict(
                "Knowledge write-back attempt changed; refresh the message before retrying".into(),
            ));
        }
        let status = initial_state
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let orphaned_running = matches!(status, "started" | "extracting" | "writing");
        let retryable_terminal = initial_state
            .get("retryable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            && matches!(status, "no_completer" | "partial" | "failed" | "interrupted");
        if !orphaned_running && !retryable_terminal {
            return Err(AppError::Conflict(
                "Knowledge write-back is not in a retryable state".into(),
            ));
        }

        // Admit the expected generation before any mount/file/model preparation.
        // Re-read under the guard so two concurrent POSTs for the same old
        // attempt cannot execute sequentially if the first finishes quickly.
        let guard = self
            .try_start_turn_writeback(conversation_id, message_id)
            .ok_or_else(|| {
                AppError::Conflict("Knowledge write-back is already running for this message".into())
            })?;
        assistant = self
            .conversation_repo
            .get_message(conversation_id, message_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Message {message_id} not found")))?;
        preparation_lease.ensure_active()?;
        assistant_content = serde_json::from_str(&assistant.content)
            .map_err(|error| AppError::Internal(format!("Invalid assistant message content JSON: {error}")))?;
        let state = assistant_content
            .get("knowledge_writeback")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| AppError::Conflict("Message has no knowledge write-back attempt".into()))?;
        if state
            .get("attempt_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            != expected_attempt_id
        {
            return Err(AppError::Conflict(
                "Knowledge write-back attempt changed; refresh the message before retrying".into(),
            ));
        }
        let current_status = state
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let current_retryable = matches!(
            current_status,
            "started" | "extracting" | "writing"
        ) || (state
            .get("retryable")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
            && matches!(
                current_status,
                "no_completer" | "partial" | "failed" | "interrupted"
            ));
        if !current_retryable {
            return Err(AppError::Conflict(
                "Knowledge write-back is not in a retryable state".into(),
            ));
        }

        let final_text = state
            .get("assistant_text")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                assistant_content
                    .get("content")
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_owned)
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| AppError::Conflict("Assistant response has no text to write back".into()))?;
        let attempt_generation = state
            .get("attempt_generation")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| AppError::Conflict("Knowledge write-back attempt generation overflow".into()))?;

        // New attempts persist the exact source id. For pre-fix failures,
        // recover the nearest preceding visible user text so those messages are
        // not permanently stranded without a retry path.
        let source = if let Some(source_message_id) = state
            .get("source_message_id")
            .and_then(serde_json::Value::as_str)
        {
            let source_message_id = parse_message_id(source_message_id)?;
            let source = self
                .conversation_repo
                .get_message(conversation_id, source_message_id)
                .await?
                .ok_or_else(|| {
                    AppError::Conflict(
                        "The source user message for this write-back no longer exists".into(),
                    )
                })?;
            preparation_lease.ensure_active()?;
            source
        } else {
            let mut before = Some((assistant.created_at, assistant.message_id.clone()));
            let mut recovered = None;
            loop {
                let page = self
                    .conversation_repo
                    .get_messages_keyset(
                    conversation_id,
                    before,
                    200,
                )
                    .await?;
                preparation_lease.ensure_active()?;
                if let Some(source) = page
                    .items
                    .iter()
                    .find(|row| {
                        row.r#type == "text"
                            && row.position.as_deref() == Some("right")
                    })
                    .cloned()
                {
                    recovered = Some(source);
                    break;
                }
                if !page.has_more {
                    break;
                }
                let Some(oldest) = page.items.last() else {
                    break;
                };
                before = Some((oldest.created_at, oldest.message_id.clone()));
            }
            recovered.ok_or_else(|| {
                AppError::Conflict(
                    "Could not recover the source user message for this write-back".into(),
                )
            })?
        };
        if source.r#type != "text" || source.position.as_deref() != Some("right") {
            return Err(AppError::Conflict(
                "Knowledge write-back source is not a user text message".into(),
            ));
        }
        let source_content: serde_json::Value = serde_json::from_str(&source.content)
            .map_err(|error| AppError::Internal(format!("Invalid source message content JSON: {error}")))?;
        let user_text = source_content
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| AppError::Conflict("Source user message has no text".into()))?;

        let mut runtime_options = self.build_runtime_options(&conversation)?;
        self.apply_knowledge_mounts(
            &conversation,
            &mut runtime_options,
            &self.runtime_registry,
            Some(&preparation_token),
        )
        .await?;
        preparation_lease.ensure_active()?;
        let (companion, _companion_id, channel_platform) =
            companion_context_from_extra(&conversation.extra)?;
        let agent_type = string_to_enum(&conversation.r#type)?;
        let (knowledge_service, mut request) = self
            .build_turn_writeback_request(
                &runtime_options.extra,
                conversation_id,
                &assistant.message_id,
                &user_text,
                None,
                agent_type,
                companion,
                channel_platform.as_deref(),
            )
            .ok_or_else(|| {
                AppError::Conflict(
                    "Knowledge write-back is no longer enabled or has no mounted knowledge base"
                        .into(),
                )
            })?;
        // Staged writes use one conversation scope across explicit tool writes
        // and turn-final extraction. This lets an automatic attempt de-duplicate
        // a proposal already staged during the same conversation.
        request.scope = conversation_id.to_owned();
        let prior_written = state
            .get("written")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let prior_failures = state
            .get("failures")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let retry_error_context = prior_failures
            .iter()
            .take(8)
            .map(|failure| {
                let normalize = |value: &str, max_chars: usize| {
                    nomi_redact::redact_secrets_owned(value.to_owned())
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(max_chars)
                        .collect::<String>()
                };
                let kb_id = normalize(failure
                    .get("kb_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<global>"), 96);
                let path = normalize(failure
                    .get("rel_path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<global>"), 320);
                let error = normalize(failure
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unspecified failure"), 512);
                format!(
                    "- retry target {kb_id}:{path}; previous error: {error}"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !retry_error_context.is_empty() {
            request.user_text.push_str(
                "\n\nPrevious knowledge write-back errors to correct on this retry:\n",
            );
            request.user_text.push_str(&retry_error_context);
        }
        if !prior_written.is_empty() {
            let persisted_scope = state
                .get("scope")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&request.scope);
            let staged_prefix =
                format!("_inbox/{}/", persisted_scope.trim_matches('/'));
            let excluded_targets: Vec<(nomifun_common::KnowledgeBaseId, String)> =
                prior_written
                .iter()
                .filter_map(|written| {
                    let kb_id = serde_json::from_value(
                        written.get("kb_id")?.clone(),
                    )
                    .ok()?;
                    let stored_path = written
                        .get("rel_path")?
                        .as_str()?
                        .trim()
                        .to_owned();
                    let rel_path = if written
                        .get("staged")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                    {
                        stored_path.strip_prefix(&staged_prefix)?.to_owned()
                    } else {
                        stored_path
                    };
                    (!rel_path.is_empty()).then_some((kb_id, rel_path))
                })
                .collect();
            if !excluded_targets.is_empty() {
                request.excluded_targets = Some(excluded_targets);
            }
        }
        let session_model = runtime_options.model.clone();
        let workspace_binding_lease = runtime_options.workspace_binding_lease.take();
        let attempt = TurnWritebackAttempt::new(
            Arc::clone(&self.conversation_repo),
            Arc::clone(&self.user_events),
            user_id.to_owned(),
            conversation_id.to_owned(),
            message_id.to_owned(),
            source.message_id,
            request.scope.clone(),
            final_text.clone(),
            prior_written,
            prior_failures,
            attempt_generation,
        );
        attempt.persist_started_intent().await.map_err(|error| {
            AppError::Internal(format!(
                "Failed to persist knowledge write-back retry intent: {error}"
            ))
        })?;
        preparation_lease.ensure_active()?;
        self.spawn_turn_writeback(
            knowledge_service,
            request,
            session_model,
            final_text,
            attempt,
            guard,
            workspace_binding_lease,
        );
        Ok(())
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
                .then_with(|| {
                    left.conversation_artifact_id
                        .cmp(&right.conversation_artifact_id)
                })
        });

        Ok(items)
    }

    /// Update the durable status of a conversation artifact and broadcast the upsert.
    pub async fn update_artifact(
        &self,
        user_id: &str,
        conversation_id: &str,
        conversation_artifact_id: &str,
        req: UpdateConversationArtifactRequest,
    ) -> Result<ConversationArtifactResponse, AppError> {
        validate_uuidv7(conversation_artifact_id).map_err(|error| {
            AppError::BadRequest(format!(
                "invalid conversation_artifact_id '{conversation_artifact_id}': {error}"
            ))
        })?;
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
                conversation_artifact_id,
                &status,
                now_ms(),
            )
            .await?
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "Conversation artifact {conversation_artifact_id} not found"
                ))
            })?;

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
    #[cfg(test)]
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub(crate) async fn send_message(
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
            MessageSendAuthority::OwnerInteractive,
            None,
            Some(lease),
            None,
            None,
            None,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn send_message_with_runtime_build_lease(
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
            MessageSendAuthority::OwnerInteractive,
            None,
            Some(lease),
            None,
            None,
            None,
        )
        .await
    }

    /// Trusted at-most-once execution boundary for durable internal effects.
    /// `operation_id` is a natural idempotency key used to claim one receipt;
    /// the receipt owns a separately minted canonical MessageId. It is deliberately not
    /// part of `SendMessageRequest`, so public callers still receive freshly
    /// server-minted message identities on every accepted request.
    pub(crate) async fn send_agent_execution_message_idempotent(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        execution_authority: AgentExecutionTurnAuthority,
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
        let runtime_build_lease = self
            .runtime_state
            .begin_runtime_preparation_for_requester(conversation_key, None, false)?;
        self.send_message_idempotent_with_lease(
            user_id,
            conversation_id,
            operation_id,
            req,
            runtime_registry,
            MessageSendAuthority::TrustedInternal,
            Some(execution_authority),
            None,
            runtime_build_lease,
            true,
            false,
            None,
            None,
        )
        .await
    }

    /// Legacy-shape harness used only by receipt/lifecycle unit tests. No
    /// production caller can invoke TrustedInternal without exact execution
    /// authority.
    #[cfg(test)]
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
        let runtime_build_lease = self
            .runtime_state
            .begin_runtime_preparation_for_requester(conversation_key, None, false)?;
        self.send_message_idempotent_with_lease(
            user_id,
            conversation_id,
            operation_id,
            req,
            runtime_registry,
            MessageSendAuthority::TrustedInternal,
            None,
            None,
            runtime_build_lease,
            true,
            false,
            None,
            None,
        )
        .await
    }

    /// Public at-most-once execution boundary with replayable acknowledgement.
    ///
    /// The opaque client token is namespaced by owner and Conversation before
    /// it reaches the globally-unique receipt table. Receipt lookup happens
    /// under a non-processing preparation fence; a completed or accepted replay
    /// therefore returns the canonical message ID without building a runtime,
    /// admitting a turn, or transiently publishing `Starting`.
    pub async fn send_message_with_idempotency_key(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<IdempotentMessageDelivery, AppError> {
        validate_public_idempotency_key(idempotency_key)?;
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }

        let conversation_key = parse_conv_id(conversation_id)?;
        let runtime_build_lease =
            self.begin_public_runtime_preparation(conversation_key, user_id)?;

        // Reject public access to retained execution-attempt transcripts before
        // creating any durable receipt. The inner path repeats this check at
        // admission time to close a concurrent relation-change race.
        self.conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        runtime_build_lease.ensure_active()?;
        self.ensure_not_retained_execution_attempt(user_id, conversation_key)
            .await?;
        runtime_build_lease.ensure_active()?;

        let operation_id =
            Self::public_turn_operation_id(user_id, conversation_key, idempotency_key);
        self.send_message_idempotent_with_lease(
            user_id,
            conversation_id,
            &operation_id,
            req,
                runtime_registry,
                MessageSendAuthority::OwnerInteractive,
                None,
                None,
                runtime_build_lease,
                true,
                false,
                None,
                None,
        )
        .await
    }

    /// Strict first-turn auto-delivery boundary.
    ///
    /// This endpoint is intentionally narrower than an ordinary user send:
    /// the repository may elect a fresh owner only while the Conversation is
    /// still the never-started creation generation (Pending, epoch zero, empty
    /// transcript, and no historical turn receipt). The proof and receipt +
    /// Running transition are one SQLite writer transaction, so a stale UI
    /// check can never restart work after another turn has completed.
    ///
    /// A matching existing receipt remains an absorbing replay. No runtime,
    /// knowledge mount, or model work is constructed before the atomic claim.
    pub async fn send_initial_message_with_idempotency_key(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<IdempotentMessageDelivery, AppError> {
        validate_public_idempotency_key(idempotency_key)?;
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }

        let conversation_key = parse_conv_id(conversation_id)?;
        let runtime_build_lease =
            self.begin_public_runtime_preparation(conversation_key, user_id)?;
        self.conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        runtime_build_lease.ensure_active()?;
        self.ensure_not_retained_execution_attempt(user_id, conversation_key)
            .await?;
        runtime_build_lease.ensure_active()?;

        let operation_id =
            Self::public_turn_operation_id(user_id, conversation_key, idempotency_key);
        self.send_message_idempotent_with_lease(
            user_id,
            conversation_id,
            &operation_id,
            req,
            runtime_registry,
            MessageSendAuthority::OwnerInteractive,
            None,
            None,
            runtime_build_lease,
            true,
            true,
            None,
            None,
        )
        .await
    }

    /// Read-only replay preflight for a fully materialized public turn request.
    ///
    /// Background owners use this before runtime/knowledge/workspace
    /// activation. A matching accepted or completed receipt is absorbing and
    /// can be settled without constructing an Agent. A miss grants no
    /// execution authority: the later keyed send must still win the atomic
    /// receiver-side claim.
    pub async fn idempotent_delivery_result_with_idempotency_key(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
        req: &SendMessageRequest,
    ) -> Result<Option<IdempotentMessageDelivery>, AppError> {
        validate_public_idempotency_key(idempotency_key)?;
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }

        let conversation_key = parse_conv_id(conversation_id)?;
        self.conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        self.ensure_not_retained_execution_attempt(user_id, conversation_key)
            .await?;

        self.ensure_no_ambiguous_edit_resubmit(user_id, conversation_key)
            .await?;

        let operation_id =
            Self::public_turn_operation_id(user_id, conversation_key, idempotency_key);
        let Some(receipt) = self
            .conversation_repo
            .get_delivery_receipt(user_id, conversation_key, &operation_id)
            .await?
        else {
            return Ok(None);
        };
        let request_payload = Self::turn_delivery_request_payload(req);
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_key
            || receipt.operation_id != operation_id
            || receipt.kind != "turn"
            || receipt.request_payload != request_payload
        {
            return Err(AppError::Conflict(
                "public message idempotency key was reused with a different request".to_owned(),
            ));
        }
        if !matches!(receipt.status.as_str(), "accepted" | "completed") {
            return Err(AppError::Conflict(format!(
                "message idempotency receipt has unsupported status '{}'",
                receipt.status
            )));
        }
        self.adopt_completed_turn_receipt_if_still_active(
            user_id,
            conversation_key,
            &receipt,
        )
        .await?;
        Ok(Some(IdempotentMessageDelivery {
            message_id: receipt.message_id,
            replayed: true,
            completed: receipt.status == "completed",
            result_ok: receipt.result_ok,
            result_text: receipt.result_text,
            result_error: receipt.result_error,
        }))
    }

    /// Observe a public turn receipt without reconstructing its request.
    ///
    /// This method is deliberately read-only. Receipt absence grants no send
    /// authority, and an accepted receipt is absorbing until the caller has
    /// used the exact background reconciliation boundary. The namespace and
    /// repository status representation remain private to this service.
    pub async fn public_turn_delivery_state(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
    ) -> Result<PublicTurnDeliveryState, AppError> {
        validate_public_idempotency_key(idempotency_key)?;
        let conversation_key = parse_conv_id(conversation_id)?;
        self.conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;

        let operation_id =
            Self::public_turn_operation_id(user_id, conversation_key, idempotency_key);
        let Some(receipt) = self
            .conversation_repo
            .get_delivery_receipt(user_id, conversation_key, &operation_id)
            .await?
        else {
            return Ok(PublicTurnDeliveryState::Missing);
        };
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_key
            || receipt.operation_id != operation_id
            || receipt.kind != "turn"
        {
            return Err(AppError::Conflict(
                "public message receipt identity does not match its exact turn scope".to_owned(),
            ));
        }

        match receipt.status.as_str() {
            "accepted" => Ok(PublicTurnDeliveryState::Accepted {
                message_id: receipt.message_id,
            }),
            "completed" => Ok(PublicTurnDeliveryState::Completed(
                IdempotentMessageDelivery {
                    message_id: receipt.message_id,
                    replayed: true,
                    completed: true,
                    result_ok: receipt.result_ok,
                    result_text: receipt.result_text,
                    result_error: receipt.result_error,
                },
            )),
            status => Err(AppError::Conflict(format!(
                "public message receipt has unsupported durable state '{status}'"
            ))),
        }
    }

    /// Read-only AutoWork receipt observation used while renewing one exact
    /// Requirement claim. It never constructs a runtime, repairs lifecycle, or
    /// treats receipt absence as execution authority.
    pub async fn autowork_delivery_result_with_idempotency_key(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
        req: &SendMessageRequest,
        authority: &RequirementConversationTurnAuthority,
    ) -> Result<Option<IdempotentMessageDelivery>, AppError> {
        validate_public_idempotency_key(idempotency_key)?;
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest(
                "Message content must not be empty".into(),
            ));
        }
        let conversation_key = parse_conv_id(conversation_id)?;
        self.conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        let operation_id =
            Self::public_turn_operation_id(user_id, conversation_key, idempotency_key);
        let Some(receipt) = self
            .conversation_repo
            .get_autowork_turn_delivery_receipt_by_scope(
                user_id,
                conversation_key,
                &authority.requirement_id,
                authority.claim_generation,
            )
            .await?
        else {
            return Ok(None);
        };
        let request_payload = Self::autowork_turn_delivery_request_payload(req, authority);
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_key
            || receipt.operation_id != operation_id
            || receipt.kind != "turn"
            || receipt.request_payload != request_payload
            || !matches!(receipt.status.as_str(), "accepted" | "completed")
        {
            return Err(AppError::Conflict(
                "AutoWork receipt does not match the exact Requirement claim capability"
                    .to_owned(),
            ));
        }
        Ok(Some(IdempotentMessageDelivery {
            message_id: receipt.message_id,
            replayed: true,
            completed: receipt.status == "completed",
            result_ok: receipt.result_ok,
            result_text: receipt.result_text,
            result_error: receipt.result_error,
        }))
    }

    /// Prove that a background turn never crossed receiver-side durable
    /// admission. This is intentionally stricter than a replay lookup: any
    /// accepted/completed receipt, retained execution, payload drift, or lookup
    /// failure prevents the caller from releasing its Requirement claim back to
    /// pending.
    pub async fn prove_no_turn_admission_with_idempotency_key(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
    ) -> Result<bool, AppError> {
        validate_public_idempotency_key(idempotency_key)?;
        let conversation_key = parse_conv_id(conversation_id)?;
        self.conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        self.ensure_not_retained_execution_attempt(user_id, conversation_key)
            .await?;
        self.ensure_no_ambiguous_edit_resubmit(user_id, conversation_key)
            .await?;

        let operation_id =
            Self::public_turn_operation_id(user_id, conversation_key, idempotency_key);
        let Some(receipt) = self
            .conversation_repo
            .get_delivery_receipt(user_id, conversation_key, &operation_id)
            .await?
        else {
            return Ok(true);
        };
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_key
            || receipt.operation_id != operation_id
            || receipt.kind != "turn"
            || !matches!(receipt.status.as_str(), "accepted" | "completed")
        {
            return Err(AppError::Conflict(
                "durable turn admission receipt has unexpected scope or state".to_owned(),
            ));
        }
        self.adopt_completed_turn_receipt_if_still_active(
            user_id,
            conversation_key,
            &receipt,
        )
        .await?;
        Ok(false)
    }

    /// Scope-first absence proof for releasing an AutoWork Requirement claim.
    /// Any receipt for `(Requirement, generation)` is absorbing, even when a
    /// caller presents a different capability-derived operation key.
    pub async fn prove_no_autowork_turn_admission(
        &self,
        user_id: &str,
        conversation_id: &str,
        requirement_id: &str,
        claim_generation: i64,
    ) -> Result<bool, AppError> {
        let conversation_key = parse_conv_id(conversation_id)?;
        self.conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        Ok(self
            .conversation_repo
            .get_autowork_turn_delivery_receipt_by_scope(
                user_id,
                conversation_key,
                requirement_id,
                claim_generation,
            )
            .await?
            .is_none())
    }

    /// Owner-authorized idempotent send for background initiators that already
    /// hold the Conversation's exact runtime preparation lease.
    ///
    /// Cron and AutoWork prepare the runtime before sending so they can
    /// subscribe to the exact Agent event stream. They must carry that same
    /// lease through turn admission; acquiring a replacement lease here would
    /// reopen a stop/reset race. The public-key validation, ownership check,
    /// retained-attempt rejection and `OwnerInteractive` authority are
    /// intentionally identical to [`Self::send_message_with_idempotency_key`].
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message_with_runtime_build_lease_and_idempotency_key(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        runtime_build_lease: RuntimeBuildLease,
    ) -> Result<IdempotentMessageDelivery, AppError> {
        validate_public_idempotency_key(idempotency_key)?;
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }

        let conversation_key = parse_conv_id(conversation_id)?;
        runtime_build_lease.ensure_active()?;
        self.conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        runtime_build_lease.ensure_active()?;
        self.ensure_not_retained_execution_attempt(user_id, conversation_key)
            .await?;
        runtime_build_lease.ensure_active()?;

        let operation_id =
            Self::public_turn_operation_id(user_id, conversation_key, idempotency_key);
        self.send_message_idempotent_with_lease(
            user_id,
            conversation_id,
            &operation_id,
            req,
            runtime_registry,
            MessageSendAuthority::OwnerInteractive,
            None,
            None,
            runtime_build_lease,
            true,
            false,
            None,
            None,
        )
        .await
    }

    /// Background send whose runtime options and mutations are applied only
    /// after the durable keyed turn wins admission. The returned subscription
    /// is installed before the model prompt is sent.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_observed_background_message_with_idempotency_key(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        runtime_build_lease: RuntimeBuildLease,
        runtime_preparation: BackgroundTurnRuntimePreparation,
    ) -> Result<ObservedIdempotentMessageDelivery, AppError> {
        self.send_observed_background_message_with_authority(
            user_id,
            conversation_id,
            idempotency_key,
            req,
            runtime_registry,
            runtime_build_lease,
            runtime_preparation,
            None,
        )
        .await
    }

    /// AutoWork receiver boundary. The opaque Requirement capability is
    /// accepted only on this internal Rust API and is checked in the same
    /// SQLite writer transaction as receipt INSERT + Conversation Running.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_observed_autowork_message_with_idempotency_key(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        runtime_build_lease: RuntimeBuildLease,
        runtime_preparation: BackgroundTurnRuntimePreparation,
        authority: RequirementConversationTurnAuthority,
    ) -> Result<ObservedIdempotentMessageDelivery, AppError> {
        self.send_observed_background_message_with_authority(
            user_id,
            conversation_id,
            idempotency_key,
            req,
            runtime_registry,
            runtime_build_lease,
            runtime_preparation,
            Some(authority),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_observed_background_message_with_authority(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        runtime_build_lease: RuntimeBuildLease,
        runtime_preparation: BackgroundTurnRuntimePreparation,
        autowork_authority: Option<RequirementConversationTurnAuthority>,
    ) -> Result<ObservedIdempotentMessageDelivery, AppError> {
        validate_public_idempotency_key(idempotency_key)?;
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest(
                "Message content must not be empty".into(),
            ));
        }
        let conversation_key = parse_conv_id(conversation_id)?;
        runtime_build_lease.ensure_active()?;
        self.conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        self.ensure_not_retained_execution_attempt(user_id, conversation_key)
            .await?;
        runtime_build_lease.ensure_active()?;

        let operation_id =
            Self::public_turn_operation_id(user_id, conversation_key, idempotency_key);
        let (observer_tx, observer_rx) = oneshot::channel();
        let delivery = self
            .send_message_idempotent_with_lease(
                user_id,
                conversation_id,
                &operation_id,
                req,
                runtime_registry,
                MessageSendAuthority::OwnerInteractive,
                None,
                autowork_authority,
                runtime_build_lease,
                true,
                false,
                Some(runtime_preparation),
                Some(observer_tx),
            )
            .await?;
        if delivery.replayed {
            return Ok(ObservedIdempotentMessageDelivery {
                delivery,
                runtime: None,
                events: None,
            });
        }
        let (runtime, events) = match observer_rx.await {
            Ok(observation) => (Some(observation.0), Some(observation.1)),
            Err(_) => (None, None),
        };
        Ok(ObservedIdempotentMessageDelivery {
            delivery,
            runtime,
            events,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_message_idempotent_with_lease(
        &self,
        user_id: &str,
        conversation_id: &str,
        operation_id: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        send_authority: MessageSendAuthority,
        execution_authority: Option<AgentExecutionTurnAuthority>,
        autowork_authority: Option<RequirementConversationTurnAuthority>,
        runtime_build_lease: RuntimeBuildLease,
        promote_before_execution: bool,
        initial_delivery: bool,
        runtime_preparation: Option<BackgroundTurnRuntimePreparation>,
        runtime_observer: Option<
            oneshot::Sender<(AgentRuntimeHandle, broadcast::Receiver<AgentStreamEvent>)>,
        >,
    ) -> Result<IdempotentMessageDelivery, AppError> {
        if promote_before_execution {
            runtime_build_lease.ensure_active()?;
        }
        let conversation_key = parse_conv_id(conversation_id)?;
        if execution_authority.is_some() && autowork_authority.is_some() {
            return Err(AppError::Conflict(
                "turn admission cannot carry both Agent Execution and AutoWork authority"
                    .to_owned(),
            ));
        }
        if initial_delivery
            && (execution_authority.is_some()
                || autowork_authority.is_some()
                || send_authority != MessageSendAuthority::OwnerInteractive)
        {
            return Err(AppError::Conflict(
                "initial auto-delivery is restricted to an owner-interactive public turn"
                    .to_owned(),
            ));
        }
        let request_payload = match (
            execution_authority.as_ref(),
            autowork_authority.as_ref(),
        ) {
            (Some(authority), None) => {
                Self::agent_execution_turn_delivery_request_payload(&req, authority)
            }
            (None, Some(authority)) => {
                Self::autowork_turn_delivery_request_payload(&req, authority)
            }
            (None, None) => Self::turn_delivery_request_payload(&req),
            (Some(_), Some(_)) => unreachable!("conflicting authority rejected above"),
        };
        // Every existing receipt is an at-most-once execution boundary across
        // process restart. `accepted` is deliberately absorbing too: the old
        // owner may have crossed an irreversible model/tool boundary before
        // crashing. Recovery requires a separate server-lock/boot-generation
        // takeover protocol; this live delivery seam never guesses from
        // Pending/empty state and never silently re-executes.
        if let Some(receipt) = self
            .conversation_repo
            .get_delivery_receipt(user_id, conversation_key, operation_id)
            .await?
        {
            if receipt.user_id != user_id
                || receipt.conversation_id != conversation_key
                || receipt.operation_id != operation_id
                || receipt.kind != "turn"
                || receipt.request_payload != request_payload
            {
                return Err(AppError::Conflict(
                    "public message idempotency key was reused with a different request"
                        .to_owned(),
                ));
            }
            if !matches!(receipt.status.as_str(), "accepted" | "completed") {
                return Err(AppError::Conflict(format!(
                    "message idempotency receipt has unsupported status '{}'",
                    receipt.status
                )));
            }
            self.adopt_completed_turn_receipt_if_still_active(
                user_id,
                conversation_key,
                &receipt,
            )
            .await?;
            return Ok(IdempotentMessageDelivery {
                message_id: receipt.message_id,
                replayed: true,
                completed: receipt.status == "completed",
                result_ok: receipt.result_ok,
                result_text: receipt.result_text,
                result_error: receipt.result_error,
            });
        }

        // A fresh durable claim is an initial mutation-owner boundary. Hold the
        // same per-Conversation preparation gate used by send/edit/warmup while
        // recovering the one provably pre-admission edit reservation and while
        // checking whether a persisted Running generation must remain
        // quarantined. Pre-gate observers must never perform this recovery: a
        // live edit owner may be paused between reserve and admit.
        let preparation_token = runtime_build_lease.cancellation_token();
        let preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(conversation_key, &preparation_token)
            .await?;
        runtime_build_lease.ensure_active()?;
        let owned_row = self
            .conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        if !send_authority.may_address_retained_execution() {
            self.ensure_not_retained_execution_attempt(user_id, conversation_key)
                .await?;
        }
        if !initial_delivery {
            self.recover_unadmitted_edit_resubmit_reservation_under_gate(
                user_id,
                conversation_key,
            )
            .await?;
        }
        self.ensure_no_ambiguous_edit_resubmit(user_id, conversation_key)
            .await?;
        runtime_build_lease.ensure_active()?;
        let row = self
            .conversation_repo
            .get(conversation_key)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        if row.status.as_deref() == Some("running") {
            return Err(self.unproven_running_generation_error(&row));
        }
        let _ = owned_row;
        runtime_build_lease.ensure_active()?;

        // Snapshot the persistent generation immediately before admission.
        // The repository consumes it with the receipt INSERT and Running
        // transition in one transaction, so reset/stop/reopen races cannot
        // admit a request prepared against an older aggregate generation.
        let expected_admission_epoch = self
            .conversation_repo
            .get_turn_admission_state(user_id, conversation_key)
            .await?
            .epoch;
        let expected_admitted_epoch = expected_admission_epoch
            .checked_add(1)
            .ok_or_else(|| {
                AppError::Conflict("Conversation admission epoch is exhausted".to_owned())
            })?;
        let operation_guard_key =
            Self::durable_operation_key(user_id, conversation_key, operation_id);
        let guard_generation = self
            .next_durable_operation_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let mut public_admission_custodian = None;
        let mut execution_admission_custodian = None;
        let claim = if let Some(authority) = execution_authority.as_ref() {
            let candidate_message_id = MessageId::new().into_string();
            let owner = Arc::new(AtomicU8::new(ADMISSION_CUSTODIAN_REQUEST_OWNER));
            execution_admission_custodian = Some(ExecutionTurnAdmissionCustodian {
                boundary: Arc::clone(&self.execution_conversation_boundary),
                user_id: user_id.to_owned(),
                conversation_id: conversation_key.to_owned(),
                operation_id: operation_id.to_owned(),
                candidate_message_id: candidate_message_id.clone(),
                request_payload: request_payload.clone(),
                authority: authority.clone(),
                expected_admitted_epoch,
                operation_guards: Arc::clone(&self.durable_operations_in_flight),
                guard_key: operation_guard_key.clone(),
                guard_generation,
                owner,
            });
            let claim_result = self.execution_conversation_boundary
                .claim_attempt_turn_receipt(
                    user_id,
                    conversation_key,
                    operation_id,
                    &candidate_message_id,
                    "turn",
                    &request_payload,
                    authority,
                    expected_admission_epoch,
                    now_ms(),
                )
                .await;
            match claim_result {
                Ok(claim) => claim,
                Err(error) => {
                    if let Some(custodian) = execution_admission_custodian.as_ref() {
                        custodian.disarm_uncommitted_claim();
                    }
                    return Err(error);
                }
            }
        } else {
            let candidate_message_id = MessageId::new().into_string();
            let owner = Arc::new(AtomicU8::new(ADMISSION_CUSTODIAN_REQUEST_OWNER));
            public_admission_custodian = Some(PublicTurnAdmissionCustodian {
                repo: Arc::clone(&self.conversation_repo),
                user_id: user_id.to_owned(),
                conversation_id: conversation_key.to_owned(),
                operation_id: operation_id.to_owned(),
                candidate_message_id: candidate_message_id.clone(),
                request_payload: request_payload.clone(),
                expected_admitted_epoch,
                operation_guards: Arc::clone(&self.durable_operations_in_flight),
                guard_key: operation_guard_key.clone(),
                guard_generation,
                owner,
            });
            let claim_result = match autowork_authority.as_ref() {
                Some(authority) => {
                    self.conversation_repo
                        .claim_autowork_turn_delivery_receipt_and_admit_with_candidate(
                            user_id,
                            conversation_key,
                            operation_id,
                            &candidate_message_id,
                            &request_payload,
                            authority,
                            expected_admission_epoch,
                            now_ms(),
                        )
                        .await
                }
                None if initial_delivery => {
                    self.conversation_repo
                        .claim_initial_turn_delivery_receipt_and_admit_with_candidate(
                            user_id,
                            conversation_key,
                            operation_id,
                            &candidate_message_id,
                            &request_payload,
                            expected_admission_epoch,
                            now_ms(),
                        )
                        .await
                }
                None => {
                    self.conversation_repo
                        .claim_turn_delivery_receipt_and_admit_with_candidate(
                            user_id,
                            conversation_key,
                            operation_id,
                            &candidate_message_id,
                            &request_payload,
                            expected_admission_epoch,
                            now_ms(),
                        )
                        .await
                }
            };
            let claim = match claim_result {
                Ok(claim) => claim,
                Err(error) => {
                    // A returned repository error is an explicit transaction
                    // rollback outcome, unlike dropping this await while its
                    // commit result is still unknown. Do not launch the
                    // ambiguity custodian for a claim proven uncommitted.
                    if let Some(custodian) = public_admission_custodian.as_ref() {
                        custodian.disarm_uncommitted_claim();
                    }
                    return Err(error.into());
                }
            };
            claim
        };
        #[cfg(test)]
        self.reach_public_admission_cutpoint(PublicAdmissionCutpoint::AfterClaimCommit)
            .await;
        let receipt = claim.receipt;
        let message_id = receipt.message_id.clone();
        if !claim.claimed_new || receipt.status == "completed" {
            self.adopt_completed_turn_receipt_if_still_active(
                user_id,
                conversation_key,
                &receipt,
            )
            .await?;
            if let Some(custodian) = public_admission_custodian.as_ref() {
                custodian.disarm_replay_loser();
            }
            if let Some(custodian) = execution_admission_custodian.as_ref() {
                custodian.disarm_replay_loser();
            }
            return Ok(IdempotentMessageDelivery {
                message_id,
                replayed: true,
                completed: receipt.status == "completed",
                result_ok: receipt.result_ok,
                result_text: receipt.result_text,
                result_error: receipt.result_error,
            });
        }
        {
            // This step runs after the durable SQLite admission. Recover a
            // poisoned lock so a past process-local panic cannot make us
            // return while leaving Running + accepted without an owner.
            let mut operations = self
                .durable_operations_in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(in_flight) = operations.get(&operation_guard_key).cloned() {
                return Ok(IdempotentMessageDelivery {
                    message_id: in_flight.message_id,
                    replayed: true,
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
            kind: "turn".to_owned(),
            request_payload: request_payload.clone(),
            execution_authority,
            durable_admitted: true,
            admission_epoch: Some(expected_admitted_epoch),
            guard_key: operation_guard_key.clone(),
            guard_generation,
            receipt_handed_off: Arc::new(AtomicBool::new(false)),
            admission_custodian_owner: public_admission_custodian
                .as_ref()
                .map(PublicTurnAdmissionCustodian::owner)
                .or_else(|| {
                    execution_admission_custodian
                        .as_ref()
                        .map(ExecutionTurnAdmissionCustodian::owner)
                }),
        };
        let runtime_build_cancellation = runtime_build_lease.cancellation_token();
        if promote_before_execution
            && let Err(error) = runtime_build_lease
                .ensure_active()
                .and_then(|()| runtime_build_lease.promote_to_turn_execution())
        {
            self.finalize_durable_admission_after_error(
                user_id,
                conversation_key,
                &delivery_lease,
                "Durably admitted turn was cancelled before process-local execution",
            )
            .await;
            if !delivery_lease.receipt_was_handed_off() {
                Self::release_durable_operation_guard(
                    &self.durable_operations_in_flight,
                    &operation_guard_key,
                    guard_generation,
                );
            }
            return Err(error);
        }

        let accepted_message_id = match self
            .send_message_inner(
                user_id,
                conversation_id,
                req,
                runtime_registry,
                send_authority,
                Some(delivery_lease.clone()),
                Some(runtime_build_lease),
                Some(preparation_guard),
                runtime_preparation,
                runtime_observer,
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
                if delivery_lease.durable_admitted {
                    self.finalize_durable_admission_after_error(
                        user_id,
                        conversation_key,
                        &delivery_lease,
                        &format!("{}", ErrorChain(&error)),
                    )
                    .await;
                } else if promote_before_execution
                    && runtime_build_cancellation.is_cancelled()
                    && !delivery_lease.receipt_was_handed_off()
                {
                    delivery_lease.handoff_receipt();
                    Self::complete_delivery_receipt_before_release(
                        &self.conversation_repo,
                        user_id,
                        conversation_key,
                        Some(operation_id),
                        false,
                        None,
                        Some("Public turn was cancelled before execution admission"),
                        &runtime_build_cancellation,
                        Some((
                            &self.durable_operations_in_flight,
                            operation_guard_key.as_str(),
                            guard_generation,
                        )),
                    )
                    .await;
                }
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
            replayed: false,
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
            id: 0,
            message_id: message_id.clone(),
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
        let stable_message_id = row
            .msg_id
            .clone()
            .unwrap_or_else(|| row.message_id.clone());
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
        Ok(row.message_id)
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
        let Some(receipt) = receipt else {
            return Ok(None);
        };
        if receipt.user_id != user_id
            || receipt.conversation_id != conversation_id
            || receipt.operation_id != operation_id
            || receipt.kind != "turn"
            || !matches!(receipt.status.as_str(), "accepted" | "completed")
        {
            return Err(AppError::Conflict(
                "internal turn delivery receipt has unexpected scope or state".to_owned(),
            ));
        }
        self.adopt_completed_turn_receipt_if_still_active(
            user_id,
            conversation_id,
            &receipt,
        )
        .await?;
        Ok(Some(IdempotentMessageDelivery {
            message_id: receipt.message_id,
            replayed: true,
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
        send_authority: MessageSendAuthority,
        durable_delivery: Option<DurableDeliveryLease>,
        mut runtime_build_lease: Option<RuntimeBuildLease>,
        preparation_guard: Option<ConversationPreparationGuard>,
        runtime_preparation: Option<BackgroundTurnRuntimePreparation>,
        runtime_observer: Option<
            oneshot::Sender<(AgentRuntimeHandle, broadcast::Receiver<AgentStreamEvent>)>,
        >,
    ) -> Result<String, AppError> {
        let public_cancellable = send_authority.public_cancellable();
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
        let durable_kind = durable_delivery
            .as_ref()
            .map(|delivery| delivery.kind.as_str());
        let durable_request_payload = durable_delivery
            .as_ref()
            .map(|delivery| delivery.request_payload.clone());
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }
        let preparation_token = runtime_build_lease
            .as_ref()
            .map(RuntimeBuildLease::cancellation_token)
            .unwrap_or_default();
        let preparation_guard = match preparation_guard {
            Some(preparation_guard) => preparation_guard,
            None => {
                self.runtime_state
                    .acquire_preparation_gate(conversation_id, &preparation_token)
                    .await?
            }
        };
        if let Some(lease) = runtime_build_lease.as_ref() {
            lease.ensure_active()?;
        }
        let send_started_at = now_ms();

        // Verify conversation exists and belongs to user
        let row = self
            .conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;
        let conversation_key = row.conversation_id.clone();
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
        // Only the trusted execution infrastructure authority may address a
        // retained attempt transcript. A public idempotency receipt provides
        // replay identity, never execution authority.
        if !send_authority.may_address_retained_execution() {
            self.ensure_not_retained_execution_attempt(user_id, &conversation_key)
                .await?;
        }

        if send_authority != MessageSendAuthority::EditResubmit {
            self.recover_unadmitted_edit_resubmit_reservation_under_gate(
                user_id,
                &conversation_key,
            )
            .await?;
            self.ensure_no_ambiguous_edit_resubmit(
                user_id,
                &conversation_key,
            )
            .await?;
        }

        let owns_atomic_durable_admission = if durable_delivery
            .as_ref()
            .is_some_and(|delivery| delivery.durable_admitted)
        {
            let delivery = durable_delivery
                .as_ref()
                .expect("checked durable admission above");
            if row.status.as_deref() != Some("running") {
                return Err(AppError::Conflict(
                    "durably admitted turn no longer owns a Running Conversation".to_owned(),
                ));
            }
            let receipt = self
                .conversation_repo
                .get_delivery_receipt(
                    user_id,
                    &conversation_key,
                    &delivery.operation_id,
                )
                .await?
                .ok_or_else(|| {
                    AppError::Conflict(
                        "durably admitted turn lost its exact receipt".to_owned(),
                    )
                })?;
            if receipt.user_id != user_id
                || receipt.conversation_id != conversation_key
                || receipt.operation_id != delivery.operation_id
                || receipt.message_id != delivery.message_id
                || receipt.kind != delivery.kind
                || receipt.request_payload != delivery.request_payload
                || receipt.status != "accepted"
            {
                return Err(AppError::Conflict(
                    "durably admitted turn receipt no longer proves this exact request"
                        .to_owned(),
                ));
            }
            let admission_epoch = delivery.admission_epoch.ok_or_else(|| {
                AppError::Conflict(
                    "durably admitted turn is missing its persistent generation".to_owned(),
                )
            })?;
            if !self
                .conversation_repo
                .validate_active_turn_operation(
                    user_id,
                    &conversation_key,
                    &delivery.operation_id,
                )
                .await?
            {
                return Err(AppError::Conflict(
                    "durably admitted turn no longer owns the active Conversation generation"
                        .to_owned(),
                ));
            }
            let admission_state = self
                .conversation_repo
                .get_turn_admission_state(user_id, &conversation_key)
                .await?;
            if admission_state.epoch != admission_epoch
                || admission_state.active_operation_id.as_deref()
                    != Some(delivery.operation_id.as_str())
            {
                return Err(AppError::Conflict(
                    "durably admitted turn generation was superseded".to_owned(),
                ));
            }
            true
        } else {
            false
        };

        if row.status.as_deref() == Some("running") && !owns_atomic_durable_admission {
            // A durable Running row is execution ownership, never permission
            // for this request to continue, replace, terminate, or guess the
            // outcome of that execution. Without exact terminal proof it stays
            // quarantined and this request is not executed.
            return Err(self.unproven_running_generation_error(&row));
        }

        let track_execution_tokens = self
            .is_active_execution_attempt_conversation(user_id, &row.conversation_id)
            .await?;

        let user_msg_id = durable_delivery
            .as_ref()
            .map(|delivery| delivery.message_id.clone())
            .unwrap_or_else(Self::mint_msg_id);
        let existing_user_message = self
            .conversation_repo
            .get_message(&row.conversation_id, &user_msg_id)
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

        let (
            mut runtime_options,
            background_desired_mode,
            background_clear_context,
            background_pre_send_hook,
        ) = match runtime_preparation {
            Some(preparation) => {
                if preparation.runtime_options.conversation_id != conversation_key {
                    return Err(AppError::Conflict(
                        "background runtime options target another Conversation".to_owned(),
                    ));
                }
                if preparation.runtime_options.user_id != user_id {
                    return Err(AppError::Forbidden(
                        "background runtime options carry another Conversation owner".to_owned(),
                    ));
                }
                (
                    preparation.runtime_options,
                    preparation.desired_mode,
                    preparation.clear_context,
                    preparation.pre_send_hook,
                )
            }
            None => {
                let options = match self.build_runtime_options(&row) {
                    Ok(opts) => opts,
                    Err(err) => {
                        error!(
                            error_code = err.error_code(),
                            error = %ErrorChain(&err),
                            "Failed to build runtime options for message send"
                        );
                        let _ = self.persist_send_failure_tip(conversation_id, None, &err).await;
                        let receipt_error = format!("{}", ErrorChain(&err));
                        if !durable_delivery
                            .as_ref()
                            .is_some_and(|delivery| delivery.durable_admitted)
                        {
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
                                    (
                                        &self.durable_operations_in_flight,
                                        key.as_str(),
                                        *generation,
                                    )
                                }),
                            )
                            .await;
                        }
                        return Err(err);
                    }
                };
                (options, None, false, None)
            }
        };
        // Background callers may provide pre-built type-specific options.
        // Re-project the frozen first-class snapshot here so every actual send
        // crosses the same authority boundary and stale/tampered adapter JSON
        // can never suppress the active preset.
        project_preset_runtime_context(&row, &runtime_options.agent_type, &mut runtime_options.extra)?;
        self.ensure_auto_workspace_skill_links(&row, &runtime_options)
            .await?;
        if let Some(lease) = runtime_build_lease.as_ref() {
            lease.ensure_active()?;
        }
        let knowledge_signature = match self
            .apply_knowledge_mounts(
                &row,
                &mut runtime_options,
                runtime_registry,
                Some(&preparation_token),
            )
            .await
        {
            Ok(signature) => signature,
            Err(error) => {
                let receipt_error = format!("{}", ErrorChain(&error));
                if !durable_delivery
                    .as_ref()
                    .is_some_and(|delivery| delivery.durable_admitted)
                {
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
                        &preparation_token,
                        durable_guard.as_ref().map(|(key, generation)| {
                            (&self.durable_operations_in_flight, key.as_str(), *generation)
                        }),
                    )
                    .await;
                }
                return Err(error);
            }
        };
        if let Some(lease) = runtime_build_lease.as_ref() {
            lease.ensure_active()?;
        }
        let stored_workspace = runtime_options.workspace.clone();

        // The receipt INSERT and this late effect admission both validate the
        // exact durable execution lease, Step/Attempt versions, Running state,
        // and active Conversation link. If Pause or a successor scheduler
        // generation won in between, leave the accepted receipt absorbing and
        // fail closed; recovery will park it instead of redelivering.
        if let Some(delivery) = durable_delivery.as_ref()
            && let Some(authority) = delivery.execution_authority.as_ref()
        {
            self.execution_conversation_boundary
                .validate_attempt_turn_effect(
                    user_id,
                    &conversation_key,
                    &delivery.operation_id,
                    &delivery.kind,
                    &delivery.request_payload,
                    authority,
                    now_ms(),
                )
                .await?;
        }

        let first_turn_msg_id = Self::mint_msg_id();
        let mut turn_handle = self
            .runtime_state
            .try_acquire_turn_with_wire_context_at_epoch_and_owner_with_persistent_generation(
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
                Some(&preparation_token),
                durable_delivery.as_ref().and_then(|delivery| {
                    delivery
                        .admission_epoch
                        .map(|epoch| (epoch, delivery.operation_id.clone()))
                }),
            )?;
        // Process-local admission alone is not execution authority.  Persist
        // the aggregate's exact Pending/Finished -> Running transition while
        // the shared preparation gate is still held.  AlreadyApplied is not a
        // replay success here: it proves some prior owner (possibly from a
        // crashed process) already owns the durable Running state.
        let running_transition = if durable_delivery
            .as_ref()
            .is_some_and(|delivery| delivery.durable_admitted)
        {
            Ok(TurnLifecycleTransition::Committed)
        } else {
            self.conversation_repo
                .mark_turn_running(user_id, &conversation_key, now_ms())
                .await
        };
        if !matches!(
            running_transition,
            Ok(TurnLifecycleTransition::Committed)
        ) {
            let receipt_error = match &running_transition {
                Ok(TurnLifecycleTransition::AlreadyApplied) => {
                    "Conversation already has a durable running turn".to_owned()
                }
                Ok(TurnLifecycleTransition::Stale) => {
                    "Conversation lifecycle rejected turn admission".to_owned()
                }
                Ok(TurnLifecycleTransition::Committed) => unreachable!(),
                Err(error) => format!("Failed to persist running turn: {}", ErrorChain(error)),
            };
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
                &preparation_token,
                durable_guard.as_ref().map(|(key, generation)| {
                    (&self.durable_operations_in_flight, key.as_str(), *generation)
                }),
            )
            .await;
            let _ = turn_handle.release();
            drop(runtime_build_lease.take());
            drop(preparation_guard);
            return Err(match running_transition {
                Ok(TurnLifecycleTransition::AlreadyApplied | TurnLifecycleTransition::Stale) => {
                    AppError::Conflict(receipt_error)
                }
                Err(error) => error.into(),
                Ok(TurnLifecycleTransition::Committed) => unreachable!(),
            });
        }

        if !durable_delivery
            .as_ref()
            .is_some_and(|delivery| delivery.admission_epoch.is_some())
        {
            let mut retry_delay = Duration::from_millis(25);
            let persistent_state = loop {
                match self
                    .conversation_repo
                    .get_turn_admission_state(user_id, &conversation_key)
                    .await
                {
                    Ok(state) => break state,
                    Err(error) => {
                        error!(
                            conversation_id,
                            error = %ErrorChain(&error),
                            "Could not bind legacy turn to its durable generation; retaining preparation and turn fences"
                        );
                        tokio::time::sleep(retry_delay).await;
                        retry_delay =
                            (retry_delay * 2).min(Duration::from_secs(2));
                    }
                }
            };
            if let Err(error) = turn_handle.bind_persistent_generation(
                persistent_state.epoch,
                persistent_state.active_operation_id,
            ) {
                // A stop closed the exact release gate while the durable
                // binding was being installed. Its tombstone owns terminal
                // reconciliation; dropping this handle only acknowledges
                // quiescence.
                drop(runtime_build_lease.take());
                drop(preparation_guard);
                return Err(error);
            }
        }

        // Durable Running is the receipt-ownership transfer point.  Every
        // subsequent non-stop exit must atomically finalize the receipt and
        // Conversation before releasing this exact handle.
        if let Some(delivery) = durable_delivery.as_ref() {
            delivery.handoff_receipt();
        }

        if let Some(pre_send_hook) = background_pre_send_hook
            && let Err(error) = pre_send_hook.prepare().await
        {
            let receipt_error = format!(
                "trusted background pre-send preparation failed: {}",
                ErrorChain(&error)
            );
            let receipt_completion = Self::turn_receipt_completion(
                durable_operation_id,
                durable_kind,
                durable_request_payload.as_deref(),
                false,
                None,
                Some(&receipt_error),
            );
            self.release_and_complete_turn(
                &mut turn_handle,
                runtime_registry,
                user_id,
                conversation_id,
                &first_turn_msg_id,
                receipt_completion,
                durable_guard.clone(),
                companion,
                companion_id,
                origin,
                channel_platform,
            )
            .await;
            drop(runtime_build_lease.take());
            return Err(error);
        }

        // Exact turn admission and durable Running now jointly own execution.
        // A stop either cancelled the preparation lease before this point or
        // now captures the active generation; no unowned build gap remains.
        drop(runtime_build_lease.take());
        let turn_cancellation = turn_handle.turn_cancellation();
        let turn_token = turn_handle.cancellation_token();

        // Store the user message. SQLite allocates the technical `id`; the
        // server-generated UUIDv7 `message_id` is the stable external key.
        let user_msg = nomifun_db::models::MessageRow {
            id: 0,
            message_id: user_msg_id.clone(),
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
                let receipt_error = format!("{}", ErrorChain(&e));
                let receipt_completion = Self::turn_receipt_completion(
                    durable_operation_id,
                    durable_kind,
                    durable_request_payload.as_deref(),
                    false,
                    None,
                    Some(&receipt_error),
                );
                self.release_and_complete_turn(
                    &mut turn_handle,
                    runtime_registry,
                    user_id,
                    conversation_id,
                    &first_turn_msg_id,
                    receipt_completion,
                    durable_guard.clone(),
                    companion,
                    companion_id,
                    origin,
                    channel_platform,
                )
                .await;
                return Err(e.into());
            }

            info!(msg_id = %user_msg_id, "User message persisted");
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
            // The stop worker closed this exact generation's release gate and
            // owns orphan finalization (Conversation + all accepted receipts).
            // Dropping the handle only acknowledges owner quiescence.
            if let Some(delivery) = durable_delivery.as_ref() {
                delivery.transfer_to_stop_owner();
            }
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
            // See the admission-cancellation branch above: stop owns the only
            // durable terminal transition and exact force-release.
            if let Some(delivery) = durable_delivery.as_ref() {
                delivery.transfer_to_stop_owner();
            }
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
        let durable_kind = durable_kind.map(str::to_owned);
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
        let source_user_message_id = user_msg_id.clone();
        let stable_turn_id = first_turn_msg_id.clone();
        let owner_turn_generation = turn_handle.turn_id();
        let owner_conversation_id = conversation_key.clone();
        #[cfg(test)]
        self.reach_public_admission_cutpoint(PublicAdmissionCutpoint::BeforeOwnerSpawn)
            .await;
        let owner_task = tokio::spawn(async move {
            let mut turn_handle = turn_handle;
            let mut runtime_observer = runtime_observer;
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
            let owner_result = AssertUnwindSafe(async {
            let mut turn_cancellation = turn_cancellation;
            let build_started_at = now_ms();
            info!(conversation_id = %conv_id, "Agent runtime build started");
            let knowledge_extra = runtime_options.extra.clone();
            let mut successful_turn_model = runtime_options.model.clone();
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
                    if cancelled {
                        // Stop owns teardown, orphan receipt settlement, and
                        // exact release for the blocked generation.
                        return;
                    }
                    error!(
                        conversation_id = %conv_id,
                        error_code = err.error_code(),
                        error = %ErrorChain(&err),
                        "Agent runtime build failed"
                    );
                    service
                        .persist_and_broadcast_send_failure_tip(
                            &user_id_owned,
                            &conv_id,
                            Some(&stable_turn_id),
                            &err,
                        )
                        .await;
                    let receipt_error = format!("{}", ErrorChain(&err));
                    let receipt_completion = Self::turn_receipt_completion(
                        durable_operation_id.as_deref(),
                        durable_kind.as_deref(),
                        durable_request_payload.as_deref(),
                        false,
                        None,
                        Some(&receipt_error),
                    );
                    service
                        .release_and_complete_turn(
                            &mut turn_handle,
                            &runtime_registry,
                            &user_id_owned,
                            &conv_id,
                            &stable_turn_id,
                            receipt_completion,
                            durable_guard.clone(),
                            companion,
                            companion_id.clone(),
                            origin.clone(),
                            channel_platform.clone(),
                        )
                        .await;
                    return;
                }
            };
            if let Some(signature) = knowledge_signature {
                service
                    .runtime_state
                    .set_knowledge_signature(&conv_id, signature);
            }

            if turn_token.is_cancelled() {
                // Stop owns this generation's only durable terminal path.
                return;
            }

            let background_runtime_preparation: Result<(), AppError> = async {
                if let Some(desired_mode) = background_desired_mode
                    .as_deref()
                    .map(str::trim)
                    .filter(|mode| !mode.is_empty())
                {
                    let current_mode = agent.get_mode().await?;
                    if current_mode.mode != desired_mode {
                        agent.set_mode(desired_mode).await?;
                    }
                }
                if background_clear_context {
                    agent.clear_context().await?;
                }
                Ok(())
            }
            .await;
            if let Err(err) = background_runtime_preparation {
                error!(
                    conversation_id = %conv_id,
                    error = %ErrorChain(&err),
                    "Trusted background runtime preparation failed"
                );
                service
                    .persist_and_broadcast_send_failure_tip(
                        &user_id_owned,
                        &conv_id,
                        Some(&stable_turn_id),
                        &err,
                    )
                    .await;
                let receipt_error = format!("{}", ErrorChain(&err));
                let receipt_completion = Self::turn_receipt_completion(
                    durable_operation_id.as_deref(),
                    durable_kind.as_deref(),
                    durable_request_payload.as_deref(),
                    false,
                    None,
                    Some(&receipt_error),
                );
                service
                    .release_and_complete_turn(
                        &mut turn_handle,
                        &runtime_registry,
                        &user_id_owned,
                        &conv_id,
                        &stable_turn_id,
                        receipt_completion,
                        durable_guard.clone(),
                        companion,
                        companion_id.clone(),
                        origin.clone(),
                        channel_platform.clone(),
                    )
                    .await;
                return;
            }

            // Arm IDMM supervision now that the Agent runtime exists (so the
            // probe's `observe` attaches to THIS turn's event stream). The
            // user-driven desktop chat path has no AutoWork loop / boot-resume
            // to arm it, so without this an enabled 智能决策 never observed the
            // turn that asks "请回复编号". Fire-and-forget; a no-op when IDMM is
            // disabled or already supervising this conversation.
            if let Some(hook) = service.current_supervision_hook() {
                hook.on_turn_start(
                    &conv_id,
                    IdmmTurnScope {
                        wire_turn_id: stable_turn_id.clone(),
                        generation: turn_handle.turn_id(),
                    },
                );
            }

            // If the factory resolved a different workspace (for example, an
            // auto-created temp directory for a row with no stored workspace),
            // persist it back.
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
                    .persist_and_broadcast_send_failure_tip(
                        &user_id_owned,
                        &conv_id,
                        Some(&stable_turn_id),
                        &err,
                    )
                    .await;
                let receipt_error = format!("{}", ErrorChain(&err));
                let receipt_completion = Self::turn_receipt_completion(
                    durable_operation_id.as_deref(),
                    durable_kind.as_deref(),
                    durable_request_payload.as_deref(),
                    false,
                    None,
                    Some(&receipt_error),
                );
                service
                    .release_and_complete_turn(
                        &mut turn_handle,
                        &runtime_registry,
                        &user_id_owned,
                        &conv_id,
                        &stable_turn_id,
                        receipt_completion,
                        durable_guard.clone(),
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
            if let Some(observer) = runtime_observer.take() {
                let events = agent.subscribe();
                let _ = observer.send((agent.clone(), events));
            }

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
                Option<ProviderWithModel>,
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
                .with_root_turn_id(stable_turn_id.clone())
                .with_cancellation(turn_cancellation.clone())
                .with_companion_context(companion, companion_id.clone())
                .with_origin(origin.clone())
                .with_channel_platform(channel_platform.clone())
                .with_artifact_workspace(agent.workspace());

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
                    Self::terminate_runtime_until_confirmed(
                        &runtime_registry,
                        &conv_id,
                        AgentKillReason::AgentErrorRecovery,
                        "broken event-stream recovery",
                    )
                    .await;
                    durable_completion = Some((
                        false,
                        outcome.final_text.clone(),
                        Some("Agent event stream integrity was lost".to_owned()),
                    ));
                    final_turn_writeback = None;
                    break;
                }

                if let Some(session_key) = agent.get_session_key() {
                    // This future may own an SQLite commit. Never cancel it on
                    // an elapsed wall-clock budget: an unknown commit result
                    // would let exact turn authority release while a late write
                    // from this generation can still land.
                    persist_session_key(&repo, &conv_id, &session_key).await;
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
                // This path can terminate and replace a process. It must not be
                // wrapped in the cancellable post-terminal side-effect budget:
                // dropping it after quarantine would let the durable Running
                // turn finalize while the old process might still execute.
                let failover_switch = service
                    .maybe_failover_in_send_loop(
                        &conv_id,
                        agent.agent_type(),
                        &outcome,
                        failover_switches_done,
                        &failover_tried,
                        &failover_extra_json,
                        &runtime_registry,
                        turn_cancellation.turn_id(),
                        &turn_token,
                    )
                    .await;
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
                    successful_turn_model = Some(switch.picked.clone());
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
                    let rebuilt = service
                        .strip_images_and_rebuild(
                            &conv_id,
                            &runtime_registry,
                            turn_cancellation.turn_id(),
                            &turn_token,
                        )
                        .await;
                    if let Some(rebuilt) = rebuilt {
                        if turn_token.is_cancelled() {
                            if let Err(error) = runtime_registry.cancel_runtime_turn(
                                &conv_id,
                                turn_cancellation.turn_id(),
                                Some(AgentKillReason::UserCancelled),
                            ) {
                                warn!(
                                    conversation_id = %conv_id,
                                    turn_id = turn_cancellation.turn_id(),
                                    error = %ErrorChain(&error),
                                    "Failed to initiate exact teardown for a cancelled image fallback; stop owner retains authority"
                                );
                            }
                            durable_completion = Some((
                                false,
                                outcome.final_text.clone(),
                                Some("Agent turn was cancelled during image fallback".to_owned()),
                            ));
                            break;
                        }
                        let _ = service.persist_images_stripped_tip(&conv_id).await;
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
                    .with_root_turn_id(stable_turn_id.clone())
                    .with_companion_context(companion, companion_id.clone())
                    .with_origin(origin.clone())
                    .with_channel_platform(channel_platform.clone());
                    let _ = surface_relay
                        .surface_terminal_error(suppressed, &turn_cancellation)
                        .await;
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

                let acp_evicted = service
                    .evict_acp_task_after_terminal_error(
                        &conv_id,
                        agent.agent_type(),
                        &outcome,
                        &runtime_registry,
                        turn_cancellation.turn_id(),
                        &turn_token,
                    )
                    .await;
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
                        final_turn_writeback = Some((
                            knowledge_service,
                            request,
                            final_text_msg_id,
                            final_text,
                            successful_turn_model.clone(),
                        ));
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
            if turn_token.is_cancelled() {
                // Stop owns orphan finalization and exact force-release.
                return;
            }

            // Knowledge write-back is part of the authoritative turn. A
            // Conversation is not Finished, its receipt is not Completed, and
            // a replacement turn cannot be admitted until this post-model
            // work has durably reached a terminal write-back state.
            if let Some((
                knowledge_service,
                mut request,
                msg_id,
                final_text,
                session_model,
            )) = final_turn_writeback
            {
                let attempt = TurnWritebackAttempt::new(
                    Arc::clone(&repo),
                    Arc::clone(&user_events),
                    user_id_owned.clone(),
                    conv_id.clone(),
                    msg_id,
                    source_user_message_id,
                    request.scope.clone(),
                    final_text.clone(),
                    Vec::new(),
                    Vec::new(),
                    1,
                );
                let writeback_result = match service
                    .resolve_turn_writeback_model(session_model.as_ref())
                    .await
                {
                    Ok(model) => {
                        request.model = model;
                        request.cancellation = Some(turn_token.clone());
                        AssertUnwindSafe(run_turn_writeback_report(
                            knowledge_service,
                            request,
                            final_text,
                            attempt,
                        ))
                        .catch_unwind()
                        .await
                    }
                    Err(writeback_error) => AssertUnwindSafe(
                        finish_turn_writeback_failure(attempt, writeback_error),
                    )
                    .catch_unwind()
                    .await,
                };
                match writeback_result {
                    Ok(Ok(())) => {}
                    Ok(Err(writeback_error)) => {
                        error!(
                            conversation_id = %conv_id,
                            error = %ErrorChain(&writeback_error),
                            "Knowledge write-back did not reach durable terminal state; retaining turn ownership"
                        );
                        // The current write-back owner retries terminal state
                        // persistence internally. This branch is deliberately
                        // fail-closed for future error variants.
                        turn_token.cancelled().await;
                        return;
                    }
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }

            if turn_token.is_cancelled() {
                return;
            }
            let receipt_completion = Self::turn_receipt_completion(
                durable_operation_id.as_deref(),
                durable_kind.as_deref(),
                durable_request_payload.as_deref(),
                ok,
                text.as_deref(),
                error.as_deref(),
            );
            service
                .release_and_complete_turn(
                    &mut turn_handle,
                    &runtime_registry,
                    &user_id_owned,
                    &conv_id,
                    &stable_turn_id,
                    receipt_completion,
                    durable_guard.clone(),
                    companion,
                    companion_id,
                    origin,
                    channel_platform,
                )
                .await;
            })
            .catch_unwind()
            .await;

            if owner_result.is_err() && turn_handle.is_cancelled() {
                // Explicit AbortHandle cancellation drops this whole outer
                // future and never reaches here. This branch covers a panic
                // racing a user stop; the stop worker owns Cancelled terminal,
                // exact release, and completion.
                warn!(conversation_id = %panic_conversation_id, "Turn owner panicked while cancellation was already active");
            } else if owner_result.is_err() {
                error!(conversation_id = %panic_conversation_id, "Turn owner task panicked; retaining ownership until failure finalization is durable");
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
                .with_root_turn_id(panic_stable_turn_id.clone())
                .with_companion_context(
                    panic_wire_context.companion,
                    panic_wire_context.companion_id.clone(),
                )
                .with_origin(panic_wire_context.origin.clone())
                .with_channel_platform(panic_wire_context.channel_platform.clone());
                if let Err(error) = panic_runtime_registry.cancel_runtime_turn(
                    &panic_conversation_id,
                    panic_cancellation.turn_id(),
                    Some(AgentKillReason::AgentErrorRecovery),
                ) {
                    warn!(
                        conversation_id = %panic_conversation_id,
                        turn_id = panic_cancellation.turn_id(),
                        error = %ErrorChain(&error),
                        "Panicked turn could not initiate exact-generation teardown; the authoritative registry barrier will retry"
                    );
                }
                Self::terminate_runtime_until_confirmed(
                    &panic_runtime_registry,
                    &panic_conversation_id,
                    AgentKillReason::AgentErrorRecovery,
                    "panicked turn recovery",
                )
                .await;

                // Do not expose a terminal boundary while the old process may
                // still be running. Once exit is proven this error projection
                // is safe, and release_and_complete_turn atomically commits the
                // receipt + Finished state before exact release/completion.
                relay
                    .surface_terminal_error(&panic_event, &panic_cancellation)
                    .await;
                let receipt_completion = Self::turn_receipt_completion(
                    durable_operation_id.as_deref(),
                    durable_kind.as_deref(),
                    durable_request_payload.as_deref(),
                    false,
                    None,
                    Some("Agent turn task exited unexpectedly before durable completion"),
                );
                service
                    .release_and_complete_turn(
                        &mut turn_handle,
                        &panic_runtime_registry,
                        &panic_user_id,
                        &panic_conversation_id,
                        &panic_stable_turn_id,
                        receipt_completion,
                        durable_guard.clone(),
                        panic_wire_context.companion,
                        panic_wire_context.companion_id,
                        panic_wire_context.origin,
                        panic_wire_context.channel_platform,
                    )
                    .await;
            }
        });
        if let Some(delivery) = durable_delivery.as_ref() {
            delivery.transfer_to_detached_turn_owner();
        }
        let owner_abort_handle = owner_task.abort_handle();
        drop(owner_task);
        self.runtime_state.register_turn_owner_abort_handle(
            &owner_conversation_id,
            owner_turn_generation,
            owner_abort_handle,
        );
        // The durable receipt, persistent Running generation, process-local
        // turn handle and detached owner task now form one continuous
        // authority chain. Only now may a view warmup or another initial owner
        // enter the preparation gate.
        drop(preparation_guard);

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
        self.ensure_retained_execution_attempt(user_id, conv_id)
            .await?;

        // A durable accepted/completed receipt is absorbing. Do not require
        // the old runtime to remain available merely to replay a lost
        // response, but reject an operation identity that is visibly being
        // reused against a different live turn.
        if let Some(receipt) = self
            .conversation_repo
            .get_delivery_receipt(user_id, conv_id, operation_id)
            .await?
        {
            let stored_scope = if receipt.kind == "steer"
                && receipt.user_id == user_id
                && receipt.conversation_id == conv_id
            {
                Self::existing_steer_receipt_scope(&receipt.request_payload, &req)
            } else {
                None
            }
            .ok_or_else(|| {
                AppError::Conflict(
                    "internal steer operation id was reused with a different request"
                        .to_owned(),
                )
            })?;
            if !matches!(receipt.status.as_str(), "accepted" | "completed") {
                return Err(AppError::Conflict(format!(
                    "steer receipt has unsupported status '{}'",
                    receipt.status
                )));
            }
            if row.status.as_deref() == Some("running")
                && let Some(active_turn) =
                    self.runtime_state.active_turn_cancellation(conv_id)
            {
                let active_scope = Self::exact_active_turn_scope(
                    &active_turn,
                    "Agent Execution steer",
                )?;
                if active_scope != stored_scope {
                    return Err(AppError::Conflict(
                        "internal steer operation id belongs to a different turn generation"
                            .to_owned(),
                    ));
                }
            }
            return Ok(receipt.message_id);
        }

        self.ensure_no_ambiguous_edit_resubmit(user_id, conv_id)
            .await?;
        let authority = self
            .acquire_idmm_active_turn_authority(
                user_id,
                conv_id,
                None,
                ExactActiveTurnAccess::AgentExecution,
                runtime_registry,
            )
            .await?;
        let request_payload =
            Self::steer_delivery_request_payload(&req, &authority.scope);
        let claim = self
            .conversation_repo
            .claim_delivery_receipt_once(
                user_id,
                conv_id,
                operation_id,
                "steer",
                &request_payload,
                now_ms(),
            )
            .await?;
        let message_id = claim.receipt.message_id.clone();
        if !claim.claimed_new || claim.receipt.status == "completed" {
            return Ok(message_id);
        }

        // The receipt INSERT is the irreversible execution election. Re-read
        // every durable and in-memory authority after winning it so a terminal
        // DB transition, generation handoff, quarantine or runtime replacement
        // in the pre-claim window cannot receive this control effect.
        let (row, active_turn, runtime) = self
            .revalidate_exact_active_turn_authority(
                user_id,
                conv_id,
                &authority.scope,
                ExactActiveTurnAccess::AgentExecution,
                &authority._lease,
                runtime_registry,
            )
            .await?;
        if active_turn.is_cancelled() {
            return Err(AppError::Conflict(
                "the Agent turn ended before the steer was delivered".to_owned(),
            ));
        }
        match runtime.steer(req.content.clone()) {
            Ok(true) => {}
            Ok(false) => {
                return Err(AppError::Conflict(
                    "the Agent turn ended before the steer was delivered".to_owned(),
                ));
            }
            Err(error) => return Err(error),
        }

        let message = nomifun_db::models::MessageRow {
            id: 0,
            message_id: message_id.clone(),
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

    fn exact_active_turn_scope(
        active_turn: &AgentTurnCancellation,
        actor: &str,
    ) -> Result<IdmmTurnScope, AppError> {
        let wire_turn_id = active_turn
            .wire_turn_id()
            .filter(|wire_turn_id| !wire_turn_id.is_empty())
            .ok_or_else(|| {
                AppError::Conflict(format!(
                    "{actor} requires the active turn's stable wire identity"
                ))
            })?
            .to_owned();
        Ok(IdmmTurnScope {
            wire_turn_id,
            generation: active_turn.turn_id(),
        })
    }

    async fn authorize_exact_active_turn_access(
        &self,
        user_id: &str,
        conversation_id: &str,
        access: ExactActiveTurnAccess,
    ) -> Result<(), AppError> {
        match access {
            ExactActiveTurnAccess::OrdinaryConversation => {
                self.ensure_not_retained_execution_attempt(user_id, conversation_id)
                    .await
            }
            ExactActiveTurnAccess::AgentExecution => {
                self.ensure_retained_execution_attempt(user_id, conversation_id)
                    .await
            }
        }
    }

    async fn revalidate_exact_active_turn_authority(
        &self,
        user_id: &str,
        conversation_id: &str,
        expected_scope: &IdmmTurnScope,
        access: ExactActiveTurnAccess,
        lease: &RuntimeBuildLease,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(ConversationRow, AgentTurnCancellation, AgentRuntimeHandle), AppError> {
        lease.ensure_active()?;
        let row = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "Conversation {conversation_id} not found"
                ))
            })?;
        lease.ensure_active()?;
        self.authorize_exact_active_turn_access(user_id, conversation_id, access)
            .await?;
        lease.ensure_active()?;
        self.ensure_no_ambiguous_edit_resubmit(user_id, conversation_id)
            .await?;
        lease.ensure_active()?;
        if row.status.as_deref() != Some("running") {
            return Err(AppError::Conflict(
                "exact-turn action requires a durable Running Conversation"
                    .to_owned(),
            ));
        }

        let active_turn = self
            .runtime_state
            .active_turn_cancellation(conversation_id)
            .ok_or_else(|| {
                AppError::Conflict(
                    "exact-turn action requires the active turn owner".to_owned(),
                )
            })?;
        let active_scope =
            Self::exact_active_turn_scope(&active_turn, "exact-turn action")?;
        if &active_scope != expected_scope {
            return Err(AppError::Conflict(
                "exact-turn action belongs to a different turn generation"
                    .to_owned(),
            ));
        }
        if active_turn.is_cancelled()
            || !runtime_registry.has_registered_runtime(conversation_id)
        {
            return Err(AppError::Conflict(
                "exact-turn action lost its active runtime authority".to_owned(),
            ));
        }
        let runtime = runtime_registry
            .get_runtime(conversation_id)
            .ok_or_else(|| {
                AppError::Conflict(
                    "exact-turn action requires a live non-quarantined runtime"
                        .to_owned(),
                )
            })?;
        if runtime.conversation_id() != conversation_id
            || runtime.status() != Some(ConversationStatus::Running)
        {
            return Err(AppError::Conflict(
                "exact-turn action requires the matching Running runtime"
                    .to_owned(),
            ));
        }

        lease.ensure_active()?;
        let current_turn = self
            .runtime_state
            .active_turn_cancellation(conversation_id)
            .ok_or_else(|| {
                AppError::Conflict(
                    "exact-turn action lost its active turn before delivery"
                        .to_owned(),
                )
            })?;
        if current_turn.is_cancelled()
            || Self::exact_active_turn_scope(&current_turn, "exact-turn action")?
                != *expected_scope
        {
            return Err(AppError::Conflict(
                "exact-turn action lost its active turn before delivery"
                    .to_owned(),
            ));
        }
        Ok((row, current_turn, runtime))
    }

    async fn acquire_idmm_active_turn_authority(
        &self,
        user_id: &str,
        conversation_id: &str,
        expected_scope: Option<&IdmmTurnScope>,
        access: ExactActiveTurnAccess,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<IdmmActiveTurnAuthority, AppError> {
        let conv_id = parse_conv_id(conversation_id)?;
        let lease = self.begin_public_runtime_preparation(conv_id, user_id)?;
        let preparation_token = lease.cancellation_token();

        let initial_row = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "Conversation {conversation_id} not found"
                ))
            })?;
        lease.ensure_active()?;
        self.authorize_exact_active_turn_access(
            user_id,
            &initial_row.conversation_id,
            access,
        )
        .await?;
        self.ensure_no_ambiguous_edit_resubmit(
            user_id,
            &initial_row.conversation_id,
        )
        .await?;
        lease.ensure_active()?;

        let preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(conv_id, &preparation_token)
            .await?;
        lease.ensure_active()?;
        let row = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "Conversation {conversation_id} not found"
                ))
            })?;
        if row.status.as_deref() != Some("running") {
            return Err(AppError::Conflict(
                "IDMM action requires a durable Running Conversation"
                    .to_owned(),
            ));
        }

        let active_turn = self
            .runtime_state
            .active_turn_cancellation(conv_id)
            .ok_or_else(|| {
                AppError::Conflict(
                    "IDMM action requires the exact active turn owner"
                        .to_owned(),
                )
            })?;
        lease.ensure_active()?;
        if active_turn.is_cancelled()
            || !runtime_registry.has_registered_runtime(conv_id)
        {
            return Err(AppError::Conflict(
                "IDMM action lost its active runtime authority".to_owned(),
            ));
        }
        let runtime = runtime_registry.get_runtime(conv_id).ok_or_else(|| {
            AppError::Conflict(
                "IDMM action requires a live non-quarantined runtime"
                    .to_owned(),
            )
        })?;
        if runtime.status() != Some(ConversationStatus::Running) {
            return Err(AppError::Conflict(
                "IDMM action requires a Running runtime".to_owned(),
            ));
        }

        let scope = Self::exact_active_turn_scope(&active_turn, "IDMM action")?;
        if expected_scope.is_some_and(|expected| expected != &scope) {
            return Err(AppError::Conflict(
                "IDMM action reservation belongs to a different turn generation"
                    .to_owned(),
            ));
        }
        lease.ensure_active()?;
        if active_turn.is_cancelled() {
            return Err(AppError::Conflict(
                "IDMM action lost its active turn before delivery".to_owned(),
            ));
        }

        Ok(IdmmActiveTurnAuthority {
            _lease: lease,
            _preparation_guard: preparation_guard,
            row,
            active_turn,
            runtime,
            scope,
        })
    }

    /// Snapshot the exact currently-running turn for an IDMM reservation.
    ///
    /// The returned scope is observation identity only; it grants no build,
    /// send, failover, or completion authority. Every eventual delivery must
    /// present it again to an exact-scope method below.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn idmm_active_turn_scope(
        &self,
        user_id: &str,
        conversation_id: &str,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<IdmmTurnScope, AppError> {
        Ok(self
            .acquire_idmm_active_turn_authority(
                user_id,
                conversation_id,
                None,
                ExactActiveTurnAccess::OrdinaryConversation,
                runtime_registry,
            )
            .await?
            .scope)
    }

    /// Continue the exact currently-running turn on behalf of IDMM.
    ///
    /// This is intentionally a never-fallback boundary. IDMM does not own the
    /// [`AgentTurnHandle`] and therefore has no authority to build a runtime,
    /// mark a Conversation Running, or start a replacement turn. The expected
    /// scope closes the reservation/delivery TOCTOU window: an action reserved
    /// for turn A is rejected if turn B has since become active.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn idmm_continue_active_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        expected_scope: &IdmmTurnScope,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<String, AppError> {
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest(
                "IDMM continuation content must not be empty".to_owned(),
            ));
        }
        if !req.files.is_empty() {
            return Err(AppError::BadRequest(
                "IDMM continuation cannot attach files to an active turn".to_owned(),
            ));
        }
        if !req.hidden || req.origin.as_deref() != Some("idmm") {
            return Err(AppError::BadRequest(
                "IDMM continuation must be hidden and carry origin=idmm".to_owned(),
            ));
        }

        let authority = self
            .acquire_idmm_active_turn_authority(
                user_id,
                conversation_id,
                Some(expected_scope),
                ExactActiveTurnAccess::OrdinaryConversation,
                runtime_registry,
            )
            .await?;
        match authority.runtime.steer(req.content.clone()) {
            Ok(true) => {}
            Ok(false) => {
                return Err(AppError::Conflict(
                    "The active turn ended before IDMM continuation delivery"
                        .to_owned(),
                ));
            }
            Err(error) => return Err(error),
        }
        // A stop may race steer itself. Keeping the preparation guard means its
        // durable finalizer cannot pass us; a delivered continuation is recorded
        // before that finalizer, while a pre-delivery cancellation was rejected.
        if !authority.active_turn.is_cancelled() {
            authority._lease.ensure_active()?;
        }

        let message_id = Self::mint_msg_id();
        let message = MessageRow {
            id: 0,
            message_id: message_id.clone(),
            conversation_id: conversation_id.to_owned(),
            msg_id: Some(message_id.clone()),
            r#type: "text".to_owned(),
            content: serde_json::json!({ "content": &req.content }).to_string(),
            position: Some("right".to_owned()),
            status: Some("finish".to_owned()),
            hidden: true,
            created_at: now_ms(),
        };
        self.conversation_repo.insert_message(&message).await?;

        let (companion, companion_id, extra_channel_platform) =
            companion_context_from_extra(&authority.row.extra)?;
        self.user_events.send_to_user(
            user_id,
            WebSocketMessage::new(
                "message.userCreated",
                serde_json::json!({
                    "conversation_id": conversation_id,
                    "msg_id": &message_id,
                    "content": &req.content,
                    "position": "right",
                    "status": "finish",
                    "hidden": true,
                    "origin": "idmm",
                    "companion": companion,
                    "companion_id": companion_id,
                    "channel_platform": req.channel_platform.or(extra_channel_platform),
                    "created_at": message.created_at,
                }),
            ),
        );
        Ok(message_id)
    }

    /// Confirm one pending tool call on the exact IDMM-reserved turn.
    ///
    /// There is no fallback to the public confirmation path and no runtime
    /// creation. The call ID must still be pending on the same generation and
    /// root wire turn that was reserved.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id, call_id = %call_id))]
    pub async fn idmm_confirm_active_turn(
        &self,
        user_id: &str,
        conversation_id: &str,
        expected_scope: &IdmmTurnScope,
        call_id: &str,
        req: ConfirmRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(), AppError> {
        if call_id.trim().is_empty() {
            return Err(AppError::BadRequest(
                "IDMM confirmation call_id must not be empty".to_owned(),
            ));
        }
        let authority = self
            .acquire_idmm_active_turn_authority(
                user_id,
                conversation_id,
                Some(expected_scope),
                ExactActiveTurnAccess::OrdinaryConversation,
                runtime_registry,
            )
            .await?;
        let confirmation_id = authority
            .runtime
            .get_confirmations()
            .into_iter()
            .find(|confirmation| confirmation.call_id == call_id)
            .map(|confirmation| confirmation.id)
            .ok_or_else(|| {
                AppError::Conflict(
                    "IDMM confirmation is no longer pending on the reserved turn"
                        .to_owned(),
                )
            })?;
        authority._lease.ensure_active()?;
        if authority.active_turn.is_cancelled() {
            return Err(AppError::Conflict(
                "IDMM confirmation lost its active turn before delivery".to_owned(),
            ));
        }
        authority
            .runtime
            .confirm(&req.msg_id, call_id, req.data, req.always_allow)?;
        self.user_events.send_to_user(
            user_id,
            WebSocketMessage::new(
                "confirmation.remove",
                serde_json::json!({
                    "conversation_id": conversation_id,
                    "id": confirmation_id,
                }),
            ),
        );
        Ok(())
    }

    /// Public, durable at-most-once steering boundary.
    ///
    /// Only the atomic receipt INSERT winner may cross the runtime steering
    /// boundary. An existing `accepted` receipt is absorbing even after a
    /// restart because the previous process may have delivered the steer and
    /// crashed before persisting its transcript row. There is deliberately no
    /// fallback to a fresh turn.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn steer_message_with_idempotency_key(
        &self,
        user_id: &str,
        conversation_id: &str,
        idempotency_key: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<IdempotentMessageDelivery, AppError> {
        validate_public_idempotency_key(idempotency_key)?;
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }
        if !req.files.is_empty() {
            return Err(AppError::BadRequest(
                "steer_unsupported: attachments must be sent as a new turn".into(),
            ));
        }

        let conv_id = parse_conv_id(conversation_id)?;
        let operation_id =
            Self::public_steer_operation_id(user_id, conv_id, idempotency_key);

        // Replay lookup intentionally precedes live-turn validation. A lost
        // response must remain replayable after the addressed turn finishes,
        // while an accepted receipt must never cause the effect to be retried.
        if let Some(receipt) = self
            .conversation_repo
            .get_delivery_receipt(user_id, conv_id, &operation_id)
            .await?
        {
            if receipt.kind != "steer"
                || receipt.user_id != user_id
                || receipt.conversation_id != conv_id
                || Self::existing_steer_receipt_scope(
                    &receipt.request_payload,
                    &req,
                )
                .is_none()
            {
                return Err(AppError::Conflict(
                    "public steer idempotency key was reused with a different request"
                        .to_owned(),
                ));
            }
            if !matches!(receipt.status.as_str(), "accepted" | "completed") {
                return Err(AppError::Conflict(format!(
                    "steer receipt has unsupported status '{}'",
                    receipt.status
                )));
            }
            return Ok(IdempotentMessageDelivery {
                message_id: receipt.message_id,
                replayed: true,
                completed: receipt.status == "completed",
                result_ok: receipt.result_ok,
                result_text: receipt.result_text,
                result_error: receipt.result_error,
            });
        }
        let steer_fence_prefix =
            Self::public_steer_operation_prefix(user_id, conv_id);
        if self
            .conversation_repo
            .has_accepted_delivery_receipt_operation_prefix(
                user_id,
                conv_id,
                &steer_fence_prefix,
            )
            .await?
        {
            return Err(AppError::Conflict(
                "another steer has an ambiguous durable outcome; explicit Conversation reset is required"
                    .to_owned(),
            ));
        }

        let authority = self
            .acquire_idmm_active_turn_authority(
                user_id,
                conv_id,
                None,
                ExactActiveTurnAccess::OrdinaryConversation,
                runtime_registry,
            )
            .await?;
        let request_payload =
            Self::steer_delivery_request_payload(&req, &authority.scope);
        let claim = self
            .conversation_repo
            .claim_delivery_receipt_once(
                user_id,
                conv_id,
                &operation_id,
                "steer",
                &request_payload,
                now_ms(),
            )
            .await?;
        let receipt = claim.receipt;
        let message_id = receipt.message_id.clone();
        if !claim.claimed_new {
            return Ok(IdempotentMessageDelivery {
                message_id,
                replayed: true,
                completed: receipt.status == "completed",
                result_ok: receipt.result_ok,
                result_text: receipt.result_text,
                result_error: receipt.result_error,
            });
        }

        authority._lease.ensure_active()?;
        if authority.active_turn.is_cancelled() {
            return Err(AppError::Conflict(
                "the Agent turn ended before the steer was delivered".to_owned(),
            ));
        }
        match authority.runtime.steer(req.content.clone()) {
            Ok(true) => {}
            Ok(false) => {
                let _ = self
                    .conversation_repo
                    .complete_delivery_receipt(
                        user_id,
                        conv_id,
                        &operation_id,
                        false,
                        None,
                        Some("the Agent turn ended before the steer was delivered"),
                        now_ms(),
                    )
                    .await?;
                return Err(AppError::Conflict(
                    "the Agent turn ended before the steer was delivered".to_owned(),
                ));
            }
            Err(error) => {
                let detail = format!("{}", ErrorChain(&error));
                let _ = self
                    .conversation_repo
                    .complete_delivery_receipt(
                        user_id,
                        conv_id,
                        &operation_id,
                        false,
                        None,
                        Some(&detail),
                        now_ms(),
                    )
                    .await?;
                return Err(error);
            }
        }

        // From this point onward `accepted` is intentionally retained on any
        // error. Delivery already crossed an irreversible runtime boundary,
        // so replay must absorb rather than guessing whether it is safe.
        let message = nomifun_db::models::MessageRow {
            id: 0,
            message_id: message_id.clone(),
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
        if !self
            .conversation_repo
            .complete_delivery_receipt(
                user_id,
                conv_id,
                &operation_id,
                true,
                None,
                None,
                now_ms(),
            )
            .await?
        {
            return Err(AppError::Internal(
                "failed to acknowledge idempotent steer receipt".to_owned(),
            ));
        }

        let (companion, companion_id, extra_channel_platform) =
            companion_context_from_extra(&authority.row.extra)?;
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
        Ok(IdempotentMessageDelivery {
            message_id,
            replayed: false,
            completed: true,
            result_ok: Some(true),
            result_text: None,
            result_error: None,
        })
    }

    /// Legacy steering seam retained only for unit tests that exercise the
    /// pre-idempotency behavior. Production routes cannot call it.
    #[cfg(test)]
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub(crate) async fn steer_message(
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
            id: 0,
            message_id: user_msg_id.clone(),
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
        turn_id: Option<&str>,
        err: &AppError,
    ) {
        let Some(row) = self.persist_send_failure_tip(conversation_id, turn_id, err).await else {
            return;
        };

        let msg_id = row
            .msg_id
            .clone()
            .unwrap_or_else(|| row.message_id.clone());
        let content_value: serde_json::Value =
            serde_json::from_str(&row.content).unwrap_or_else(|_| serde_json::Value::String(row.content.clone()));
        self.user_events.send_to_user(
            user_id,
            WebSocketMessage::new(
                "message.stream",
                serde_json::json!({
                "conversation_id": row.conversation_id,
                "turn_id": turn_id,
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

    /// Durable at-most-once edit/rewind/truncate/resubmit workflow.
    ///
    /// The receipt is claimed before the first destructive step. Every
    /// existing accepted receipt is absorbing across process restart. It also
    /// acts as a Conversation-wide send fence until terminal settlement or an
    /// explicit reset, so a crash after rewind or transcript truncation cannot
    /// be followed by an unrelated fresh turn.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id, message_id = %message_id))]
    pub async fn edit_and_resubmit_with_idempotency_key(
        &self,
        user_id: &str,
        conversation_id: &str,
        message_id: &str,
        idempotency_key: &str,
        req: SendMessageRequest,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<IdempotentMessageDelivery, AppError> {
        validate_public_idempotency_key(idempotency_key)?;
        if req.content.trim().is_empty() {
            return Err(AppError::BadRequest("Message content must not be empty".into()));
        }
        let conv_id = parse_conv_id(conversation_id)?;
        let message_id = parse_message_id(message_id)?;
        let operation_id =
            Self::public_edit_resubmit_operation_id(user_id, conv_id, idempotency_key);
        let request_payload = Self::edit_resubmit_request_payload(message_id, &req);

        // Response-loss and restart replay must not depend on the current
        // transcript or runtime, both of which the first owner may have
        // irreversibly changed.
        if let Some(receipt) = self
            .conversation_repo
            .get_delivery_receipt(user_id, conv_id, &operation_id)
            .await?
        {
            if receipt.kind != "turn"
                || receipt.user_id != user_id
                || receipt.conversation_id != conv_id
                || receipt.request_payload != request_payload
            {
                return Err(AppError::Conflict(
                    "edit/resubmit idempotency key was reused with a different request"
                        .to_owned(),
                ));
            }
            if !matches!(receipt.status.as_str(), "accepted" | "completed") {
                return Err(AppError::Conflict(format!(
                    "edit/resubmit receipt has unsupported status '{}'",
                    receipt.status
                )));
            }
            self.adopt_completed_turn_receipt_if_still_active(
                user_id,
                conv_id,
                &receipt,
            )
            .await?;
            return Ok(IdempotentMessageDelivery {
                message_id: receipt.message_id,
                replayed: true,
                completed: receipt.status == "completed",
                result_ok: receipt.result_ok,
                result_text: receipt.result_text,
                result_error: receipt.result_error,
            });
        }
        let runtime_build_lease =
            self.begin_public_runtime_preparation(conv_id, user_id)?;
        let preparation_token = runtime_build_lease.cancellation_token();
        let preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(conv_id, &preparation_token)
            .await?;
        runtime_build_lease.ensure_active()?;
        self.recover_unadmitted_edit_resubmit_reservation_under_gate(
            user_id,
            conv_id,
        )
        .await?;
        self.ensure_no_ambiguous_edit_resubmit(user_id, conv_id)
            .await?;
        runtime_build_lease.ensure_active()?;

        let row = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;
        runtime_build_lease.ensure_active()?;
        if row.r#type != "nomi" {
            return Err(AppError::BadRequest(
                "Edit & resubmit is only supported for Nomi conversations".into(),
            ));
        }
        if row.status.as_deref() == Some("running")
            || self.runtime_state.has_active_turn(conv_id)
        {
            return Err(AppError::Conflict(
                "Edit & resubmit requires an idle terminal Conversation".to_owned(),
            ));
        }

        let recent = self
            .conversation_repo
            .get_messages_keyset(conv_id, None, 50)
            .await?;
        runtime_build_lease.ensure_active()?;
        let target = recent
            .items
            .iter()
            .find(|message| {
                message.position.as_deref() == Some("right")
                    && message.r#type == "text"
            })
            .ok_or_else(|| {
                AppError::BadRequest("No editable user message found".into())
            })?;
        if target.message_id != message_id {
            return Err(AppError::BadRequest(
                "Only the most recent user message can be edited".into(),
            ));
        }
        let (from_created_at, from_id) =
            (target.created_at, target.message_id.clone());

        // Snapshot the exact terminal generation immediately before claiming
        // the destructive workflow. The repository consumes this epoch while
        // inserting the receipt and fence in one SQLite writer transaction, so
        // a concurrent fresh send can never slip between those two mutations.
        let expected_admission_epoch = self
            .conversation_repo
            .get_turn_admission_state(user_id, conv_id)
            .await?
            .epoch;
        let reserved_admission_epoch = expected_admission_epoch
            .checked_add(1)
            .ok_or_else(|| {
                AppError::Conflict("Conversation admission epoch is exhausted".to_owned())
            })?;
        let admitted_admission_epoch = reserved_admission_epoch.checked_add(1).ok_or_else(|| {
            AppError::Conflict("Conversation admission epoch is exhausted".to_owned())
        })?;
        let guard_key =
            Self::durable_operation_key(user_id, conv_id, &operation_id);
        let guard_generation = self
            .next_durable_operation_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let candidate_message_id = MessageId::new().into_string();
        let edit_admission_custodian = EditResubmitAdmissionCustodian {
            repo: Arc::clone(&self.conversation_repo),
            runtime_registry: Arc::clone(runtime_registry),
            runtime_state: Arc::clone(&self.runtime_state),
            user_id: user_id.to_owned(),
            conversation_id: conv_id.to_owned(),
            operation_id: operation_id.clone(),
            candidate_message_id: candidate_message_id.clone(),
            request_payload: request_payload.clone(),
            reserved_admission_epoch,
            admitted_admission_epoch,
            conversation_created_at: row.created_at,
            operation_guards: Arc::clone(&self.durable_operations_in_flight),
            guard_key: guard_key.clone(),
            guard_generation,
            owner: Arc::new(AtomicU8::new(ADMISSION_CUSTODIAN_REQUEST_OWNER)),
            phase: Arc::new(AtomicU8::new(EDIT_CUSTODIAN_RESERVED)),
        };
        let claim_result = self
            .conversation_repo
            .claim_edit_resubmit_receipt_and_fence(
                user_id,
                conv_id,
                &operation_id,
                &candidate_message_id,
                &request_payload,
                message_id,
                expected_admission_epoch,
                now_ms(),
            )
            .await;
        let claim = match claim_result {
            Ok(claim) => claim,
            Err(error) => {
                edit_admission_custodian.disarm_uncommitted_claim();
                return Err(error.into());
            }
        };
        let receipt = claim.receipt;
        let replacement_message_id = receipt.message_id.clone();
        if !matches!(receipt.status.as_str(), "accepted" | "completed") {
            return Err(AppError::Conflict(format!(
                "edit/resubmit receipt has unsupported status '{}'",
                receipt.status
            )));
        }
        if !claim.claimed_new || receipt.status == "completed" {
            edit_admission_custodian.disarm_replay_loser();
            self.adopt_completed_turn_receipt_if_still_active(
                user_id,
                conv_id,
                &receipt,
            )
            .await?;
            return Ok(IdempotentMessageDelivery {
                message_id: replacement_message_id,
                replayed: true,
                completed: receipt.status == "completed",
                result_ok: receipt.result_ok,
                result_text: receipt.result_text,
                result_error: receipt.result_error,
            });
        }

        // Reservation is not execution authority. Consume its exact epoch in a
        // second conditional transaction before touching the engine or
        // transcript. A reset between reserve and admit increments the epoch,
        // settles the receipt and removes the fence, permanently rejecting this
        // stale edit owner.
        let admitted = self
            .conversation_repo
            .admit_reserved_edit_turn(
                user_id,
                conv_id,
                &operation_id,
                &request_payload,
                reserved_admission_epoch,
                now_ms(),
            )
            .await?;
        if !admitted {
            // The reservation receipt/fence is itself durable state. If a
            // retained execution link or reset wins before admission, absorb
            // only this reserved generation and remove its matching fence so
            // unrelated future turns are not permanently blocked.
            let transition = self
                .conversation_repo
                .recover_unadmitted_edit_resubmit_reservation(
                    user_id,
                    conv_id,
                    &operation_id,
                    &candidate_message_id,
                    &request_payload,
                    reserved_admission_epoch,
                    "Edit/resubmit reservation lost admission authority",
                    now_ms(),
                )
                .await?;
            if matches!(
                transition,
                TurnLifecycleTransition::Committed | TurnLifecycleTransition::AlreadyApplied
            ) {
                edit_admission_custodian.disarm_replay_loser();
            }
            return Err(AppError::Conflict(
                "Edit/resubmit reservation lost Conversation admission authority".to_owned(),
            ));
        }
        edit_admission_custodian.mark_admitted()?;

        self.durable_operations_in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                guard_key.clone(),
                DurableOperationLease {
                    message_id: replacement_message_id.clone(),
                    generation: guard_generation,
                },
            );
        let delivery = DurableDeliveryLease {
            operation_id: operation_id.clone(),
            message_id: replacement_message_id,
            kind: "turn".to_owned(),
            request_payload: request_payload.clone(),
            execution_authority: None,
            durable_admitted: true,
            admission_epoch: Some(admitted_admission_epoch),
            guard_key,
            guard_generation,
            receipt_handed_off: Arc::new(AtomicBool::new(false)),
            admission_custodian_owner: Some(edit_admission_custodian.owner()),
        };

        // From this point the durable Running generation has an exact owner.
        // Any process-local preparation failure is settled through that owner;
        // it can never fall back to a generic status-based transition.
        let preparation_result: Result<(), AppError> = async {
            runtime_build_lease.ensure_active()?;
            runtime_build_lease.promote_to_turn_execution()?;
            let agent = self.runtime_handle(conversation_id)?;
            edit_admission_custodian.mark_destructive_runtime_mutation()?;
            agent.rewind_last_turn().await?;
            runtime_build_lease.ensure_active()?;
            self.conversation_repo
                .delete_messages_from(conv_id, from_created_at, &from_id)
                .await?;
            runtime_build_lease.ensure_active()?;
            Ok(())
        }
        .await;
        if let Err(error) = preparation_result {
            if edit_admission_custodian.destructive_runtime_mutation_started() {
                Self::quarantine_edit_runtime_until_confirmed(
                    runtime_registry,
                    &self.runtime_state,
                    conv_id,
                    row.created_at,
                )
                .await;
            }
            self.finalize_durable_admission_after_error(
                user_id,
                conv_id,
                &delivery,
                &format!("{}", ErrorChain(&error)),
            )
            .await;
            if !delivery.receipt_was_handed_off() {
                Self::release_durable_operation_guard(
                    &self.durable_operations_in_flight,
                    &delivery.guard_key,
                    delivery.guard_generation,
                );
            }
            return Err(error);
        }

        let replacement_message_id = match self
            .send_message_inner(
                user_id,
                conversation_id,
                req,
                runtime_registry,
                MessageSendAuthority::EditResubmit,
                Some(delivery.clone()),
                Some(runtime_build_lease),
                Some(preparation_guard),
                None,
                None,
            )
            .await
        {
            Ok(message_id) => {
                if !delivery.receipt_was_handed_off() {
                    Self::release_durable_operation_guard(
                        &self.durable_operations_in_flight,
                        &delivery.guard_key,
                        delivery.guard_generation,
                    );
                }
                message_id
            }
            Err(error) => {
                if edit_admission_custodian.destructive_runtime_mutation_started() {
                    Self::quarantine_edit_runtime_until_confirmed(
                        runtime_registry,
                        &self.runtime_state,
                        conv_id,
                        row.created_at,
                    )
                    .await;
                }
                self.finalize_durable_admission_after_error(
                    user_id,
                    conv_id,
                    &delivery,
                    &format!("{}", ErrorChain(&error)),
                )
                .await;
                if !delivery.receipt_was_handed_off() {
                    Self::release_durable_operation_guard(
                        &self.durable_operations_in_flight,
                        &delivery.guard_key,
                        delivery.guard_generation,
                    );
                }
                return Err(error);
            }
        };
        Ok(IdempotentMessageDelivery {
            message_id: replacement_message_id,
            replayed: false,
            completed: false,
            result_ok: None,
            result_text: None,
            result_error: None,
        })
    }

    /// Edit the most recent user message and re-run from there (Nomi only).
    /// Rewinds the engine's last turn, truncates the DB transcript at the
    /// target message (inclusive), then re-sends the edited content as a fresh
    /// turn. Rejects non-Nomi conversations and any target that is not the
    /// latest user message (so the engine rewind and DB truncation stay aligned).
    #[cfg(test)]
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id, message_id = %message_id))]
    pub(crate) async fn edit_and_resubmit(
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
        if target.message_id != message_id {
            return Err(AppError::BadRequest(
                "Only the most recent user message can be edited".into(),
            ));
        }
        let (from_created_at, from_id) = (target.created_at, target.message_id.clone());

        // 4. 取在飞 agent 并回退最后一个 turn（内部会先停掉在飞 turn）。
        let agent = self.runtime_handle(conversation_id)?;
        agent.rewind_last_turn().await?;
        runtime_build_lease.ensure_active()?;

        // The completed old turn may already own a detached knowledge
        // write-back. Drain it before deleting its source/assistant rows so it
        // cannot publish late knowledge or recreate state after the rewind.
        self.cancel_and_wait_for_turn_writebacks(conv_id).await?;
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
    ///
    /// The stop tombstone remains owned until all cancelled builds and the turn
    /// owner are quiescent, the registered process tree has *proven* exit, and
    /// a durable Running aggregate (including every accepted turn receipt) has
    /// atomically reached Finished. There is intentionally no total teardown or
    /// database timeout: on failure the registry quarantine + stop tombstone +
    /// exact release block remain authoritative and retries continue.
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
                    loop {
                        if runtime_state
                            .wait_for_cleanup_fences(
                                &conversation_id,
                                CANCEL_TEARDOWN_GRACE,
                            )
                            .await
                        {
                            let _ = result_tx.send(Ok(()));
                            break;
                        }
                        warn!(
                            conversation_id,
                            "Conversation stop follower is still waiting for the authoritative cleanup owner"
                        );
                    }
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
        let cached_generation = turn_cancellation
            .as_ref()
            .and_then(AgentTurnCancellation::persistent_generation)
            .map(|(epoch, operation_id)| ConversationTurnAdmissionState {
                epoch,
                active_operation_id: operation_id.map(str::to_owned),
            });
        let service = self.clone();
        let kill_reason = if deletion_owned {
            AgentKillReason::ConversationDeleted
        } else {
            AgentKillReason::UserCancelled
        };
        let durable_reason = if deletion_owned {
            "Conversation turn was terminated for deletion after runtime exit"
        } else {
            "Conversation turn was cancelled after runtime exit"
        };

        tokio::spawn(async move {
            // Keep this guard outside catch_unwind. A cleanup panic therefore
            // cannot drop the tombstone and expose a possibly Running row.
            let stop_guard = stop_guard;
            let cleanup = AssertUnwindSafe(async {
                // The synchronous stop tombstone is already established.
                // Keyed turns carry the generation captured at their atomic
                // admission; legacy/idle cleanup snapshots under this
                // tombstone before any teardown await.
                let expected_generation = if let Some(generation) = cached_generation {
                    generation
                } else {
                    let mut retry_delay = Duration::from_millis(25);
                    loop {
                        match service
                            .conversation_repo
                            .get_turn_admission_state(&user_id, &conversation_id)
                            .await
                        {
                            Ok(generation) => break generation,
                            Err(error) => {
                                error!(
                                    conversation_id,
                                    error = %ErrorChain(&error),
                                    "Stop could not snapshot durable generation; retaining stop tombstone"
                                );
                                tokio::time::sleep(retry_delay).await;
                                retry_delay =
                                    (retry_delay * 2).min(Duration::from_secs(2));
                            }
                        }
                    }
                };
                if let Some(cancellation) = turn_cancellation.as_ref() {
                    if let Err(error) = runtime_registry.cancel_runtime_turn(
                        &conversation_id,
                        cancellation.turn_id(),
                        Some(kill_reason),
                    ) {
                        warn!(
                            conversation_id,
                            turn_id = cancellation.turn_id(),
                            error = %ErrorChain(&error),
                            "Failed to initiate exact-generation stop; authoritative teardown barrier will retry"
                        );
                    }
                    cancellation.cancel();
                }

                let build_cleanup = async {
                    loop {
                        if service
                            .runtime_state
                            .wait_for_runtime_builds(
                                &conversation_id,
                                &cancelled_build_ids,
                                CANCEL_TEARDOWN_GRACE,
                            )
                            .await
                        {
                            break;
                        }
                        warn!(
                            conversation_id,
                            "Cancelled runtime preparation is still active; retaining stop ownership"
                        );
                    }
                    service.runtime_state.forget_cancelled_runtime_builds(
                        &conversation_id,
                        &cancelled_build_ids,
                    );
                };

                let owner_cleanup = async {
                    let Some(cancellation) = turn_cancellation.as_ref() else {
                        return true;
                    };
                    let terminal_seen = cancellation
                        .wait_for_terminal_observed(CANCEL_RELEASE_GRACE)
                        .await;
                    while !cancellation
                        .wait_for_owner_quiesced(CANCEL_TEARDOWN_GRACE)
                        .await
                    {
                        warn!(
                            conversation_id,
                            turn_id = cancellation.turn_id(),
                            "Cancelled turn owner is still quiescing; retaining the stop tombstone without aborting an in-flight persistence future"
                        );
                    }
                    terminal_seen
                };

                let teardown = Self::terminate_runtime_until_confirmed(
                    &runtime_registry,
                    &conversation_id,
                    kill_reason,
                    "conversation stop",
                );
                let (_, terminal_seen, ()) =
                    tokio::join!(build_cleanup, owner_cleanup, teardown);

                // Every retry preparation lease captured by the stop tombstone
                // is now quiescent, so no new detached manual write-back can
                // register after this drain observes an empty map. Keep stop
                // ownership indefinitely while any already-spawned retry
                // reaches its cancellation-safe terminal boundary.
                service
                    .cancel_and_wait_for_turn_writebacks(&conversation_id)
                    .await?;

                // Serialize status reconciliation with send/warmup/reset. The
                // stop tombstone has already cancelled every older preparation,
                // so this gate can only be released by those owners quiescing.
                let preparation_token = CancellationToken::new();
                let preparation_guard = loop {
                    match service
                        .runtime_state
                        .acquire_preparation_gate(
                            &conversation_id,
                            &preparation_token,
                        )
                        .await
                    {
                        Ok(guard) => break guard,
                        Err(error) => {
                            error!(
                                conversation_id,
                                error = %ErrorChain(&error),
                                "Stop could not acquire durable-finalization gate; retaining stop ownership"
                            );
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                };

                // Runtime construction, the outer turn owner, and the backend
                // process tree are now proven quiescent. Detached knowledge
                // write-back children are separately tracked, so stop must
                // drain that activity and durably reconcile each attempt
                // before it may finalize the exact Conversation generation.
                // Neither barrier has a total timeout: the stop tombstone and
                // preparation gate stay owned on every transient DB failure.
                await_turn_writeback_quiesced(&conversation_id).await;
                reconcile_quiesced_writebacks_until_resolved(
                    Arc::clone(&service.conversation_repo),
                    Some(Arc::clone(&service.user_events)),
                    &user_id,
                    &conversation_id,
                )
                .await;

                let mut retry_delay = Duration::from_millis(25);
                let transition = loop {
                    match service
                        .conversation_repo
                        .finalize_exact_cancelled_turn_generation(
                            &user_id,
                            &conversation_id,
                            expected_generation.epoch,
                            expected_generation.active_operation_id.as_deref(),
                            durable_reason,
                            now_ms(),
                        )
                        .await
                    {
                        Ok(transition @ (TurnLifecycleTransition::Committed
                        | TurnLifecycleTransition::AlreadyApplied
                        | TurnLifecycleTransition::Stale)) => break transition,
                        Err(error) => {
                            error!(
                                conversation_id,
                                error = %ErrorChain(&error),
                                "Stop durable finalization failed; retaining runtime quarantine and retrying"
                            );
                        }
                    }
                    tokio::time::sleep(retry_delay).await;
                    retry_delay =
                        (retry_delay * 2).min(Duration::from_secs(2));
                };
                let committed =
                    transition == TurnLifecycleTransition::Committed;

                let mut completion: Option<(Option<String>, TurnWireContext)> =
                    None;
                if let Some(cancellation) = turn_cancellation.as_ref() {
                    let wire_turn_id =
                        cancellation.wire_turn_id().map(str::to_owned);
                    let terminal_msg_id =
                        cancellation.terminal_msg_id().map(str::to_owned);
                    let wire_context = cancellation.wire_context().clone();
                    if committed
                        && publish_completion
                        && !terminal_seen
                        && let Some(terminal_msg_id) =
                            terminal_msg_id.as_ref()
                    {
                        let relay = StreamRelay::new(
                            conversation_id.clone(),
                            terminal_msg_id.clone(),
                            user_id.clone(),
                            Arc::clone(&service.conversation_repo),
                            Arc::clone(&service.user_events),
                            None,
                        )
                        .with_root_turn_id(
                            cancellation
                                .wire_turn_id()
                                .unwrap_or(terminal_msg_id)
                                .to_owned(),
                        )
                        .with_companion_context(
                            wire_context.companion,
                            wire_context.companion_id.clone(),
                        )
                        .with_origin(wire_context.origin.clone())
                        .with_channel_platform(
                            wire_context.channel_platform.clone(),
                        );
                        relay.surface_cancelled_turn(cancellation);
                    }

                    let released = service
                        .runtime_state
                        .force_release_cancelled_turn(cancellation);
                    if committed && publish_completion && released {
                        completion = Some((wire_turn_id, wire_context));
                    }
                } else if committed && publish_completion {
                    // Only a durable Running orphan produces an idle completion.
                    // Pending stop is a no-op on business status and Finished
                    // stop is already terminal, so neither emits a duplicate.
                    completion = Some((None, TurnWireContext::default()));
                }

                if let Some((wire_turn_id, wire_context)) = completion {
                    StreamRelay::broadcast_turn_completed_with_context(
                        &service.user_events,
                        &user_id,
                        &conversation_id,
                        wire_turn_id,
                        Some(service.final_completion_runtime(
                            &conversation_id,
                        )),
                        wire_context.companion,
                        wire_context.companion_id,
                        wire_context.origin,
                        wire_context.channel_platform,
                    );
                }

                // Synchronous event enqueue occurs while both fences are held.
                // A client reacting to it cannot admit work until these drops.
                drop(preparation_guard);
                Ok::<(), AppError>(())
            })
            .catch_unwind()
            .await;

            match cleanup {
                Ok(result) => {
                    drop(stop_guard);
                    let _ = result_tx.send(result);
                }
                Err(_) => {
                    error!(
                        conversation_id,
                        "Conversation stop cleanup panicked; retaining the stop tombstone permanently"
                    );
                    let _ = result_tx.send(Err(AppError::Internal(
                        "conversation stop cleanup panicked while lifecycle remained fenced"
                            .to_owned(),
                    )));
                    std::future::pending::<()>().await;
                    drop(stop_guard);
                }
            }
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
            #[cfg(test)]
            self.reach_public_admission_cutpoint(
                PublicAdmissionCutpoint::AfterPublicPreparationCancelCaptured,
            )
            .await;
            // This authority is intentionally narrower than a conversation
            // stop: an unverified public preparation may cancel only work
            // initiated by the same authenticated requester, never a
            // private/durable build or an unrelated idle runtime. Keep the
            // requester preflight fence through physical build quiescence so
            // returning success can never orphan a still-running factory/tool
            // future or lose its queryable lease.
            self.note_user_cancel(conversation_id);
            self.await_cancelled_runtime_builds_quiesced(
                conversation_id,
                cancelled_build_ids,
                "public runtime-preparation stop",
            )
            .await;
            self.runtime_state
                .forget_cancelled_runtime_builds(conversation_id, cancelled_build_ids);
            // The same requester preflight that prevented a late preparation
            // lease also closes manual retry admission. Drain already
            // registered write-back guards before acknowledging stop; otherwise
            // an unrelated retry attempt for the same Finished conversation
            // could still publish after this narrow public-build stop returned.
            self.cancel_and_wait_for_turn_writebacks(conversation_key)
                .await?;
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

            // A cold durable Running row is a restart orphan, not local stop
            // authority. The replacement process cannot prove that the prior
            // backend's complete descendant tree is empty merely because its
            // registry has no exact active owner. Keep the receipt/aggregate
            // untouched and do not install a stop tombstone or user-cancel
            // stamp until a backend presents queryable exact terminal proof.
            if conversation.status.as_deref() == Some("running")
                && running_orphan_disposition(&conversation.r#type)?
                    == RunningOrphanDisposition::ExternalTerminalProofRequired
            {
                drop(user_cancel_preflight);
                return Err(AppError::Conflict(
                    "Conversation has an unproven running turn from a prior process; stop cannot finalize it without exact process-empty proof"
                        .to_owned(),
                ));
            }

            // A user stops or decides Attempt work through Agent Execution.
            // The internal cleanup path keeps using AgentExecution origin and
            // may cancel retained attempt runtimes.
            if origin == CancelOrigin::User {
                tokio::time::timeout(
                    CANCEL_AUTH_PREFLIGHT_GRACE,
                    self.ensure_not_retained_execution_attempt(user_id, &conversation.conversation_id),
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
                // The detached worker deliberately retains its fence and keeps
                // retrying. Do not acknowledge success before runtime exit and
                // durable terminal commit have actually completed.
                warn!(
                    conversation_id,
                    "Conversation stop is still fenced while durable cleanup continues"
                );
                Err(AppError::Timeout(
                    "conversation stop is still in progress; runtime exit and durable finalization have not yet been confirmed"
                        .to_owned(),
                ))
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
    /// Unlike [`Self::reset`] (which also deletes DB messages), this:
    ///  1. resets the live agent's in-memory/session context if one is running
    ///     (ACP rotates to a fresh `session/new`, Nomi empties its engine,
    ///     OpenClaw/Remote forget their gateway session) — see
    ///     [`AgentRuntimeHandle::clear_context`]; and
    ///  2. without constructing a cold runtime, clears the persisted ACP resume
    ///     identity and the exact `conversation_id + created_at` Nomi session
    ///     generation so a later explicit send cannot resume archived context.
    ///
    /// The preparation/reset fences serialize this maintenance operation with
    /// send, warmup, stop, completion, and destructive reset. Message rows and
    /// the durable conversation status are intentionally left untouched, and
    /// this method never publishes turn lifecycle events.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn clear_context(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<(), AppError> {
        let conv_id = parse_conv_id(conversation_id)?;
        // Reject unauthorized/retained requests before taking lifecycle
        // ownership or disturbing any live runtime.
        self.conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;

        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;
        self.ensure_no_ambiguous_edit_resubmit(user_id, conv_id)
            .await?;

        // Context release is a maintenance mutation, not runtime preparation.
        // Serialize with the complete build/admission path, then take the
        // reversible reset owner so stop/delete/completion cannot overlap the
        // async in-memory or filesystem reset. Neither fence constructs a
        // runtime or changes the persisted conversation lifecycle.
        let preparation_token = CancellationToken::new();
        let preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(conversation_id, &preparation_token)
            .await?;
        let reset_guard = self
            .runtime_state
            .begin_conversation_reset(conversation_id)?;

        // Ownership or lifecycle may have changed while waiting for the
        // preparation gate. Re-read the row under both fences and use this
        // exact generation's created_at as the persisted Nomi owner token.
        let row = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "Conversation {conversation_id} not found"
                ))
            })?;
        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;
        self.ensure_no_ambiguous_edit_resubmit(user_id, conv_id)
            .await?;
        if !matches!(row.status.as_deref(), Some("pending" | "finished")) {
            return Err(AppError::Conflict(format!(
                "Conversation {conversation_id} is not in a context-clearable terminal state"
            )));
        }

        let cancelled_build_ids = reset_guard.cancelled_build_ids().to_vec();
        self.await_cancelled_runtime_builds_quiesced(
            conversation_id,
            &cancelled_build_ids,
            "conversation context clear",
        )
        .await;
        self.runtime_state
            .forget_cancelled_runtime_builds(conversation_id, &cancelled_build_ids);

        // Reset an existing idle runtime in place. A cold Nomi conversation
        // instead uses the registry's factory-admission barrier and exact
        // created_at owner token; manufacturing a runtime just to erase the
        // transcript is expressly forbidden.
        let had_runtime = if let Some(agent) = self.runtime_registry.get_runtime(conversation_id) {
            agent.clear_context().await?;
            true
        } else {
            info!("No active agent; clearing persisted state only");
            false
        };

        if row.r#type == AgentType::Nomi.serde_name() && !had_runtime {
            self.runtime_registry
                .reset_persisted_nomi_session(conversation_id, row.created_at)
                .await?;
        }

        // ACP session clearing is durable authority, not best effort. Returning
        // success while the old resume id survives would make the next cold
        // runtime silently recover the supposedly archived context.
        self.acp_session_repo.clear_session_id(conv_id).await?;

        drop(reset_guard);
        drop(preparation_guard);
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
        _runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(), AppError> {
        let conv_id = parse_conv_id(conversation_id)?;
        self.conversation_repo
            .get(conv_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;
        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;

        // Clear is an aggregate lifecycle mutation, not two independent
        // deletes. Serialize it with every runtime preparation/send and hold a
        // reset tombstone until receipt absorption, projection detachment,
        // transcript deletion, and session reset commit together.
        let preparation_token = CancellationToken::new();
        let preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(conversation_id, &preparation_token)
            .await?;
        let clear_guard = self
            .runtime_state
            .begin_conversation_reset(conversation_id)?;

        let clear_row = self
            .conversation_repo
            .get(conv_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
                AppError::NotFound(format!("Conversation {conversation_id} not found"))
            })?;
        self.ensure_not_retained_execution_attempt(user_id, conv_id)
            .await?;
        if !matches!(clear_row.status.as_deref(), Some("pending" | "finished")) {
            return Err(AppError::Conflict(format!(
                "Conversation {conversation_id} is not in a clearable terminal state"
            )));
        }
        let cancelled_build_ids = clear_guard.cancelled_build_ids().to_vec();
        self.await_cancelled_runtime_builds_quiesced(
            conversation_id,
            &cancelled_build_ids,
            "conversation transcript clear",
        )
        .await;
        self.runtime_state
            .forget_cancelled_runtime_builds(conversation_id, &cancelled_build_ids);
        self.cancel_and_wait_for_turn_writebacks(conv_id).await?;
        self.runtime_registry
            .terminate_and_wait_result(
                conversation_id,
                Some(AgentKillReason::UserCancelled),
            )
            .await?;
        if clear_row.r#type == AgentType::Nomi.serde_name() {
            self.runtime_registry
                .reset_persisted_nomi_session(conversation_id, clear_row.created_at)
                .await?;
        }
        self.runtime_state.clear_knowledge_signature(conversation_id);
        self.runtime_state.clear_turn_tokens(conversation_id);

        match self
            .conversation_repo
            .clear_terminal_conversation_messages(user_id, conv_id, now_ms())
            .await?
        {
            TurnLifecycleTransition::Committed | TurnLifecycleTransition::AlreadyApplied => {}
            TurnLifecycleTransition::Stale => {
                return Err(AppError::Conflict(format!(
                    "Conversation {conversation_id} lifecycle changed while clearing messages"
                )));
            }
        }

        drop(clear_guard);
        drop(preparation_guard);
        info!("Conversation messages cleared");
        Ok(())
    }

    /// Build or reuse a runtime through the same strict knowledge-binding
    /// preparation path used by an interactive send.
    ///
    /// Background initiators (Cron and AutoWork) already own a
    /// [`RuntimeBuildLease`] and need to preserve their specialized factory
    /// options. This stage is preparation-only: their later keyed send owns
    /// the actual turn admission. It must neither mint nor overwrite registry
    /// turn authority. They must not call the registry directly, either:
    /// doing so would bypass physical workspace authority and could reconcile
    /// `.nomi/knowledge` underneath another live conversation.
    pub async fn get_or_create_runtime_with_prepared_knowledge(
        &self,
        row: &ConversationRow,
        mut runtime_options: AgentRuntimeBuildOptions,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        build_lease: &RuntimeBuildLease,
    ) -> Result<AgentRuntimeHandle, AppError> {
        if runtime_options.conversation_id != row.conversation_id {
            return Err(AppError::Conflict(format!(
                "runtime options conversation {} do not match authoritative row {}",
                runtime_options.conversation_id, row.conversation_id
            )));
        }
        if runtime_options.user_id != row.user_id {
            return Err(AppError::Forbidden(format!(
                "runtime options owner does not match conversation {}",
                row.conversation_id
            )));
        }

        build_lease.ensure_active()?;
        let cancellation = build_lease.cancellation_token();
        let _preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(&row.conversation_id, &cancellation)
            .await?;
        build_lease.ensure_active()?;
        self.recover_unadmitted_edit_resubmit_reservation_under_gate(
            &row.user_id,
            &row.conversation_id,
        )
        .await?;
        self.ensure_no_ambiguous_edit_resubmit(
            &row.user_id,
            &row.conversation_id,
        )
        .await?;
        build_lease.ensure_active()?;

        let knowledge_signature = self
            .apply_knowledge_mounts(
                row,
                &mut runtime_options,
                runtime_registry,
                Some(&cancellation),
            )
            .await?;
        build_lease.ensure_active()?;

        let agent = runtime_registry
            .get_or_create_runtime_for_preparation(
                &row.conversation_id,
                cancellation.clone(),
                runtime_options,
            )
            .await?;
        if build_lease.is_cancelled() {
            // Preparation never owns a turn generation. Prove the reusable
            // slot exited without manufacturing an exact-turn cancellation.
            Self::terminate_runtime_until_confirmed(
                runtime_registry,
                &row.conversation_id,
                AgentKillReason::UserCancelled,
                "cancelled background runtime preparation",
            )
            .await;
            return Err(AppError::Conflict(format!(
                "conversation {} runtime preparation was cancelled",
                row.conversation_id
            )));
        }

        // Runtime prompt metadata becomes authoritative only after the exact
        // slot owns its workspace lease and factory construction succeeded.
        if let Some(signature) = knowledge_signature {
            self.runtime_state
                .set_knowledge_signature(&row.conversation_id, signature);
        }
        Ok(agent)
    }

    /// Best-effort runtime preparation triggered by opening a conversation
    /// view. Navigation is read-only: only a never-started, transcript-empty
    /// pending conversation may construct a cold runtime.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn warmup_for_view(
        &self,
        user_id: &str,
        conversation_id: &str,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(), AppError> {
        self.warmup_inner(user_id, conversation_id, runtime_registry)
            .await
    }

    async fn warmup_inner(
        &self,
        user_id: &str,
        conversation_id: &str,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
    ) -> Result<(), AppError> {
        let lease = self.begin_public_runtime_preparation(conversation_id, user_id)?;
        let preparation_token = lease.cancellation_token();
        let initial_row = self
            .conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;
        lease.ensure_active()?;

        self.ensure_not_retained_execution_attempt(user_id, &initial_row.conversation_id)
            .await?;
        lease.ensure_active()?;

        let _preparation_guard = self
            .runtime_state
            .acquire_preparation_gate(conversation_id, &preparation_token)
            .await?;
        lease.ensure_active()?;

        // Re-read after acquiring the shared gate. A send may have completed
        // admission while this view warmup was waiting; decisions based on the
        // pre-gate snapshot would otherwise recycle/build after the turn.
        let row = self
            .conversation_repo
            .get(parse_conv_id(conversation_id)?)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {conversation_id} not found")))?;
        self.ensure_not_retained_execution_attempt(user_id, &row.conversation_id)
            .await?;
        self.recover_unadmitted_edit_resubmit_reservation_under_gate(
            user_id,
            &row.conversation_id,
        )
        .await?;
        self.ensure_no_ambiguous_edit_resubmit(
            user_id,
            &row.conversation_id,
        )
        .await?;
        lease.ensure_active()?;

        let persisted_status = match row.status.as_deref() {
            None | Some("") => ConversationStatus::Finished,
            Some(status) => string_to_enum(status)?,
        };
        if persisted_status == ConversationStatus::Running {
            lease.ensure_active()?;
            if !self.runtime_state.has_active_turn(conversation_id) {
                // A durable Running row with no process-local turn owner is an
                // unproven restart orphan even when an idle runtime slot is
                // cached. Navigation is read-only: it neither tears down a
                // guessed process authority nor publishes a terminal result.
                return Err(self.unproven_running_generation_error(&row));
            }
            return Ok(());
        }
        if persisted_status != ConversationStatus::Pending
            || self.runtime_state.has_active_turn(conversation_id)
        {
            debug!(
                ?persisted_status,
                "Skipping view-only warmup for a conversation that has already entered its lifecycle"
            );
            return Ok(());
        }

        // A failed terminal-status write can leave a durable transcript on a
        // row that still says Pending. History is therefore an independent
        // lifecycle witness, and lookup failure must fail closed rather than
        // treating an unknown transcript as empty.
        let transcript = self
            .conversation_repo
            .get_messages_keyset(&row.conversation_id, None, 1)
            .await?;
        lease.ensure_active()?;
        if !transcript.items.is_empty() {
            debug!(
                "Skipping view-only warmup because the conversation already has durable history"
            );
            return Ok(());
        }

        let (runtime_options, knowledge_signature) = self
            .prepare_runtime_options_for_execution(
                &row,
                runtime_registry,
                Some(&preparation_token),
            )
            .await?;
        lease.ensure_active()?;
        let stored_workspace = runtime_options.workspace.clone();
        let agent = runtime_registry
            .get_or_create_runtime_for_preparation(
                conversation_id,
                preparation_token.clone(),
                runtime_options,
            )
            .await?;
        if lease.is_cancelled() {
            // View warmup is never a turn owner. Cancellation tears down the
            // preparation slot with proof and cannot target a real turn by a
            // coincidentally equal build id.
            Self::terminate_runtime_with_proof(
                runtime_registry,
                conversation_id,
                AgentKillReason::UserCancelled,
                "cancelled warmup",
            )
            .await?;
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} warmup was cancelled"
            )));
        }

        // Persist auto-resolved workspace if factory picked a different path.
        self.maybe_persist_workspace(conversation_id, &stored_workspace, agent.workspace())
            .await?;
        lease.ensure_active()?;
        if let Some(signature) = knowledge_signature {
            self.runtime_state
                .set_knowledge_signature(conversation_id, signature);
        }

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

/// Render the immutable preset snapshot as explicit runtime context.
///
/// Preset instructions alone are insufficient for introspection: a model can
/// follow them while still truthfully believing it has no product-level
/// "preset" because it was never told the preset's identity.  The envelope
/// makes activation observable to the model without asking adapters to reload
/// the mutable catalog.
fn render_preset_runtime_context(snapshot: &ResolvedPresetSnapshot) -> String {
    let mut prompt = format!(
        "[NomiFun active preset]\n\
         Name: {}\n\
         Revision: {}\n\
         This preset is active for the current conversation. If the user asks \
         whether a preset/设定 is active, answer accurately with this name and \
         revision.\n\
         \n\
         [Preset instructions]",
        snapshot.preset_name, snapshot.preset_revision
    );
    let instructions = snapshot.instructions.trim();
    if instructions.is_empty() {
        prompt.push_str("\n(No additional instructions.)");
    } else {
        prompt.push('\n');
        prompt.push_str(instructions);
    }
    prompt
}

/// Validate the first-class preset lineage and project it into the prompt key
/// consumed by the concrete runtime adapter.
///
/// `conversations.preset_snapshot` is authoritative.  `extra.preset_rules`
/// and `extra.preset_context` are merely ephemeral adapter projections and
/// must never be allowed to drift from the frozen snapshot.
fn project_preset_runtime_context(
    row: &ConversationRow,
    agent_type: &AgentType,
    extra: &mut serde_json::Value,
) -> Result<(), AppError> {
    let Some(object) = extra.as_object_mut() else {
        return Err(AppError::Internal(format!(
            "Conversation {} extra must be a JSON object",
            row.conversation_id
        )));
    };

    let lineage_present =
        row.preset_id.is_some() || row.preset_revision.is_some() || row.preset_snapshot.is_some();
    if !lineage_present {
        return Ok(());
    }

    let preset_id = row.preset_id.as_deref().ok_or_else(|| {
        AppError::Internal(format!(
            "Conversation {} preset lineage is missing preset_id",
            row.conversation_id
        ))
    })?;
    let preset_revision = row.preset_revision.ok_or_else(|| {
        AppError::Internal(format!(
            "Conversation {} preset lineage is missing preset_revision",
            row.conversation_id
        ))
    })?;
    let raw_snapshot = row.preset_snapshot.as_deref().ok_or_else(|| {
        AppError::Internal(format!(
            "Conversation {} preset lineage is missing preset_snapshot",
            row.conversation_id
        ))
    })?;
    let snapshot: ResolvedPresetSnapshot =
        serde_json::from_str(raw_snapshot).map_err(|error| {
            AppError::Internal(format!(
                "Conversation {} has invalid preset_snapshot: {error}",
                row.conversation_id
            ))
        })?;
    if snapshot.preset_id != preset_id || snapshot.preset_revision != preset_revision {
        return Err(AppError::Internal(format!(
            "Conversation {} preset lineage does not match its frozen snapshot",
            row.conversation_id
        )));
    }

    // Some trusted target builders (currently companion/public-persona paths)
    // already compose these instructions into a stronger persona prompt.
    // Preserve that explicit server-owned deduplication marker.
    if object
        .get("preset_instructions_embedded")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        object.remove("preset_rules");
        object.remove("preset_context");
        return Ok(());
    }

    let context = serde_json::Value::String(render_preset_runtime_context(&snapshot));
    match agent_type {
        AgentType::Nomi => {
            object.insert("preset_rules".to_owned(), context);
            object.remove("preset_context");
        }
        AgentType::Acp
        | AgentType::OpenclawGateway
        | AgentType::Nanobot
        | AgentType::Remote => {
            object.insert("preset_context".to_owned(), context);
            object.remove("preset_rules");
        }
    }
    debug!(
        conversation_id = %row.conversation_id,
        preset_id,
        preset_revision,
        agent_type = agent_type.serde_name(),
        "projected immutable preset snapshot into runtime context"
    );
    Ok(())
}

impl ConversationService {
    /// Resolve the authoritative factory options and attach the exact physical
    /// workspace binding required for one execution build.
    ///
    /// Send/warmup and in-turn recovery must use this preparation sequence
    /// before calling the runtime registry. The returned signature is only a
    /// candidate; callers commit it after factory construction succeeds.
    pub(crate) async fn prepare_runtime_options_for_execution(
        &self,
        row: &ConversationRow,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<(AgentRuntimeBuildOptions, Option<String>), AppError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(AppError::Conflict(format!(
                "runtime preparation for conversation {} was cancelled",
                row.conversation_id
            )));
        }
        let mut runtime_options = self.build_runtime_options(row)?;
        self.ensure_auto_workspace_skill_links(row, &runtime_options)
            .await?;
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(AppError::Conflict(format!(
                "runtime preparation for conversation {} was cancelled",
                row.conversation_id
            )));
        }
        let knowledge_signature = self
            .apply_knowledge_mounts(row, &mut runtime_options, runtime_registry, cancellation)
            .await?;
        Ok((runtime_options, knowledge_signature))
    }

    pub(crate) fn commit_runtime_knowledge_signature(
        &self,
        conversation_id: &str,
        signature: Option<String>,
    ) {
        if let Some(signature) = signature {
            self.runtime_state
                .set_knowledge_signature(conversation_id, signature);
        }
    }

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

        project_preset_runtime_context(row, &agent_type, &mut extra)?;

        if !self.execution_authority(&row.user_id).controls_host() {
            // Even a row written outside the service cannot smuggle a custom
            // workspace, prompt-side capability config or installation binding
            // into execution. Preserve only the server-minted managed-workspace
            // identity; dropping it would force the factory to invent or reuse
            // an unowned fallback path.
            let temp_workspace_id = require_temp_workspace_id(&extra, &row.conversation_id)?;
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
                        row.conversation_id
                    )));
                }
            }
        };

        Ok(AgentRuntimeBuildOptions {
            user_id: row.user_id.clone(),
            agent_type,
            workspace,
            model,
            conversation_id: row.conversation_id.clone(),
            delegation_policy,
            extra,
            // Stamp/validate the nomi session against this conversation instance.
            conversation_created_at: Some(row.created_at),
            workspace_binding_lease: None,
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
                    row.conversation_id
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

        let acp_agent = if runtime_options.agent_type == AgentType::Acp {
            Some(
                resolve_acp_agent_metadata(
                    &self.agent_metadata_repo,
                    &runtime_options.extra,
                )
                .await?,
            )
        } else {
            None
        };
        let Some(rel_dirs) =
            native_skills_dirs(&runtime_options.agent_type, acp_agent.as_ref())
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
            conversation_id = %row.conversation_id,
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
    /// stay confined to the hidden `.nomi/knowledge/` directory. Runtime
    /// creation fails closed when physical binding authority or exact mount
    /// reconciliation cannot be proven; it must never continue with prompt
    /// metadata that disagrees with the shared filesystem.
    ///
    /// Binding target selection (spec §3 ruling 6 / §4.5): a conversation
    /// whose `extra.companion_id` is a non-blank string mounts the companion-level
    /// binding `('companion', companion_id)`; everything else keeps the per-conversation
    /// binding `('conversation', conversation_id)`. No merge between the two.
    pub(crate) async fn apply_knowledge_mounts(
        &self,
        row: &ConversationRow,
        runtime_options: &mut AgentRuntimeBuildOptions,
        runtime_registry: &Arc<dyn AgentRuntimeRegistry>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Option<String>, AppError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(AppError::Conflict(format!(
                "knowledge mount preparation for conversation {} was cancelled",
                runtime_options.conversation_id
            )));
        }
        let workspace = PathBuf::from(runtime_options.workspace.trim());
        // Knowledge roots are installation-owned filesystem resources.  A
        // model-only user must never reach workpath/companion binding lookup or
        // create a physical mount before the Agent factory applies its own
        // ceiling. It still holds conservative unbound authority so no other
        // runtime can activate a different mount namespace underneath it.
        if !self.execution_authority(&row.user_id).controls_host() {
            return attach_unbound_workspace_authority(runtime_options, &workspace).map(Some);
        }
        let service = self.knowledge_service.read().ok().and_then(|guard| guard.clone());
        let Some(service) = service else {
            return attach_unbound_workspace_authority(runtime_options, &workspace).map(Some);
        };

        // The persisted Conversation row is the binding authority. Specialized
        // background factory extras intentionally omit presentation/preset
        // fields and must not silently route the same conversation to another
        // binding target.
        let binding_extra: serde_json::Value = serde_json::from_str(&row.extra)
            .map_err(|error| {
                AppError::Internal(format!(
                    "conversation {} has invalid extra JSON: {error}",
                    row.conversation_id
                ))
            })?;
        let (target_kind, target_id) =
            knowledge_binding_target(&binding_extra, &runtime_options.conversation_id)?;
        let target_id = target_id.to_owned();
        // Workpath-first for conversation sessions (session-list unification
        // spec §7): the binding belongs to the workspace path, not the
        // individual conversation. `session_workpath_key` maps a
        // backend-managed (temporary) workspace — one under `workspace_root`,
        // the same root `row_to_response` treats as the data dir for the
        // `is_temporary_workspace` flag — to the `__default__` sentinel, and
        // every user-chosen directory to its normalized key. The knowledge
        // service looks up the `('workpath', key)` row first and only falls
        // back to the supported conversation-scoped binding on a full miss.
        // Companion sessions keep their `('companion', companion_id)` binding unchanged — they
        // are not per-workspace.
        let preset_binding = binding_extra
            .get("preset_knowledge_binding")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let plan = if target_kind == "conversation" && !preset_binding {
            let wp_key = nomifun_knowledge::session_workpath_key(&workspace, &self.workspace_root);
            service
                .prepare_mounts_for_session(&wp_key, &workspace)
                .await?
        } else {
            service
                .prepare_mounts_for_target(target_kind, &target_id, &workspace)
                .await?
        };
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(AppError::Conflict(format!(
                "knowledge mount preparation for conversation {} was cancelled",
                runtime_options.conversation_id
            )));
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
        // Compare only the durable binding contract. The outcome also contains
        // mutable knowledge content (TOC, summaries and live-source display
        // metadata) that writeback/refresh may legitimately change while this
        // exact runtime owns the same mounts. Treating that content as runtime
        // identity made a later view warmup recycle a completed conversation.
        let new_signature = plan.binding_signature().to_owned();
        let known_signature = self.runtime_state.knowledge_signature(&conversation_id);
        let registered_runtime = runtime_registry.has_registered_runtime(&conversation_id);
        let binding_is_unknown_or_changed =
            known_signature.as_deref() != Some(new_signature.as_str());

        if registered_runtime && binding_is_unknown_or_changed {
            // The plan is still read-only at this point. Never mutate the
            // shared mount namespace while an old or unknown process could
            // observe it.
            let runtime_is_running = runtime_registry
                .get_runtime(&conversation_id)
                .is_some_and(|agent| agent.status() == Some(ConversationStatus::Running));
            if self.runtime_state.has_active_turn(&conversation_id) || runtime_is_running {
                return Err(AppError::Conflict(format!(
                    "conversation {conversation_id} has an active runtime with a different or unknown knowledge binding"
                )));
            }
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(AppError::Conflict(format!(
                    "knowledge binding recycle for conversation {conversation_id} was cancelled"
                )));
            }
            info!(
                conversation_id = %conversation_id,
                known_signature = ?known_signature,
                "knowledge binding changed or is unknown; proving old runtime exit before mount reconciliation"
            );
            Self::terminate_runtime_with_proof(
                runtime_registry,
                &conversation_id,
                AgentKillReason::KnowledgeBindingChanged,
                "knowledge binding recycle",
            )
            .await?;
            self.runtime_state
                .clear_knowledge_signature(&conversation_id);
        } else if !registered_runtime {
            // A signature is runtime-slot metadata, not durable binding state.
            // An idle teardown may have removed the slot outside Conversation,
            // so discard any stale hint before attaching the next exact lease.
            self.runtime_state
                .clear_knowledge_signature(&conversation_id);
        }

        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(AppError::Conflict(format!(
                "knowledge mount activation for conversation {conversation_id} was cancelled"
            )));
        }

        // Authority acquisition happens before sync. A conflicting active
        // conversation therefore fails without deleting or replacing a single
        // mount. The returned RAII lease is transferred into the exact runtime
        // slot by AgentRuntimeRegistry before its factory starts.
        let (outcome, workspace_binding_lease) = plan.activate(&conversation_id).await?;
        runtime_options.workspace_binding_lease = Some(workspace_binding_lease);

        let Some(obj) = runtime_options.extra.as_object_mut() else {
            return Err(AppError::Internal(format!(
                "conversation {conversation_id} runtime extra is not an object"
            )));
        };
        if outcome.mounts.is_empty() {
            obj.remove("knowledge_mounts");
            obj.remove("knowledge_writeback");
            obj.remove("knowledge_writeback_mode");
            obj.remove("knowledge_writeback_eagerness");
            obj.remove("knowledge_channel_write_enabled");
            return Ok(Some(new_signature));
        }
        debug!(
            conversation_id = %row.conversation_id,
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
        Ok(Some(new_signature))
    }

    fn build_turn_writeback_request(
        &self,
        extra: &serde_json::Value,
        conversation_id: &str,
        _msg_id: &str,
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
        let scope = conversation_id.trim_matches('/').to_owned();
        let request = nomifun_knowledge::TurnWritebackRequest {
            mounts: mounts.clone(),
            binding: nomifun_knowledge::KnowledgeBinding {
                enabled: true,
                writeback,
                writeback_mode,
                writeback_eagerness,
                channel_write_enabled,
                kb_ids: mounts.iter().map(|m| m.knowledge_base_id.clone()).collect(),
                ..Default::default()
            },
            surface,
            scope,
            user_text: user_text.to_owned(),
            assistant_text: String::new(),
            model: None,
            excluded_targets: None,
            cancellation: None,
        };

        Some((service, request))
    }

    /// Write the resolved workspace back to `conversation.extra.workspace` when
    /// the factory picked a different (auto-generated) path than what was stored.
    ///
    /// This handles any accepted row whose `extra.workspace` is empty: the
    /// factory creates a temp directory at task-build time, and this persists
    /// the resolved path for subsequent runtime and frontend reads.
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

        let mut extra: serde_json::Value = serde_json::from_str(&row.extra).map_err(|error| {
            AppError::Internal(format!(
                "Conversation {conversation_id} has invalid extra JSON: {error}"
            ))
        })?;
        if !extra.is_object() {
            return Err(AppError::Internal(format!(
                "Conversation {conversation_id} extra must be a JSON object"
            )));
        }
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

}

fn take_string_array(
    object: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Vec<String>, AppError> {
    match object.remove(key) {
        Some(value) => serde_json::from_value(value)
            .map_err(|error| AppError::BadRequest(format!("Invalid extra.{key}: {error}"))),
        None => Ok(Vec::new()),
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

fn allocate_temp_workspace_id(workspace_root: &Path) -> (String, PathBuf) {
    loop {
        let temp_workspace_id = generate_id();
        let path = auto_temp_workspace_path(workspace_root, &temp_workspace_id);
        if !path.exists() {
            return (temp_workspace_id, path);
        }
    }
}

fn auto_temp_workspace_path(workspace_root: &Path, temp_workspace_id: &str) -> PathBuf {
    workspace_root
        .join("conversations")
        .join(temp_workspace_id)
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
    validate_uuidv7(value).map_err(|error| {
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
    let temp_workspace_id = require_temp_workspace_id(extra, &row.conversation_id)?;
    let _ = agent_type;
    Ok(auto_temp_workspace_path(workspace_root, temp_workspace_id))
}

/// Resolve the authoritative workspace used to validate host-local receipts in
/// persisted history. Managed workspaces are recomputed from their durable
/// token so database restores or data-root relocation cannot make an old
/// absolute `extra.workspace` escape the current installation. Custom
/// workspaces use the same path validation as runtime construction.
fn history_artifact_workspace(
    workspace_root: &Path,
    row: &ConversationRow,
) -> Result<PathBuf, String> {
    let extra: serde_json::Value = serde_json::from_str(&row.extra)
        .map_err(|error| format!("invalid conversation extra JSON: {error}"))?;
    if temp_workspace_marker_present(&extra) {
        let agent_type: AgentType = string_to_enum(&row.r#type)
            .map_err(|error| format!("invalid conversation agent type: {error}"))?;
        return auto_workspace_path_for_row(workspace_root, row, &agent_type, &extra)
            .map_err(|error| error.to_string());
    }

    let workspace = extra
        .get("workspace")
        .and_then(serde_json::Value::as_str)
        .filter(|workspace| !workspace.is_empty())
        .ok_or_else(|| "conversation has no artifact workspace".to_owned())?;
    validate_runtime_workspace_path(workspace)
        .map(PathBuf::from)
        .map_err(|error| error.to_string())
}

fn managed_temp_workspace_path_from_row(
    workspace_root: &Path,
    row: &ConversationRow,
) -> Result<Option<PathBuf>, AppError> {
    let extra: serde_json::Value = serde_json::from_str(&row.extra).map_err(|error| {
        AppError::Internal(format!(
            "Conversation {} has invalid extra JSON: {error}",
            row.conversation_id
        ))
    })?;
    if !temp_workspace_marker_present(&extra) {
        return Ok(None);
    }
    let temp_workspace_id = require_temp_workspace_id(&extra, &row.conversation_id)?;
    Ok(Some(auto_temp_workspace_path(
        workspace_root,
        temp_workspace_id,
    )))
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

/// Resolve the native skills directory list for an agent by looking it
/// up in the `agent_metadata` catalog (ACP vendors) or the bundled
/// `AgentType` table (non-ACP built-ins).
///
/// Returns `None` when the agent does not support native skill
/// discovery — callers should then skip the workspace-symlink step and
/// rely on prompt injection instead.
fn native_skills_dirs(
    agent_type: &AgentType,
    acp_agent: Option<&AgentMetadataRow>,
) -> Option<Vec<String>> {
    if *agent_type == AgentType::Acp {
        let row = acp_agent?;
        let raw = row.native_skills_dirs.as_deref()?;
        return serde_json::from_str::<Vec<String>>(raw).ok();
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
    let row = resolve_acp_agent_metadata(repo, extra).await?;
    let capabilities = Some(&row)
        .and_then(|row| row.agent_capabilities.as_deref())
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .map(|value| parse_acp_mcp_capabilities(&value))
        .unwrap_or_default();

    Ok(McpSupportPolicy::from_acp_capabilities(capabilities))
}

async fn resolve_acp_agent_metadata(
    repo: &Arc<dyn IAgentMetadataRepository>,
    extra: &serde_json::Value,
) -> Result<AgentMetadataRow, AppError> {
    let agent_id =
        required_trimmed_extra_string(extra, "agent_id", "ACP conversation")?;
    let row = repo
        .get(agent_id)
        .await
        .map_err(|error| AppError::Internal(format!("agent_metadata lookup: {error}")))?
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "ACP extra.agent_id '{agent_id}' does not exist"
            ))
        })?;
    validate_acp_agent_metadata_row(&row, extra)?;
    Ok(row)
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
            mcp_server_id: McpServerId::parse(row.mcp_server_id.clone())
                .expect("repository MCP IDs must be canonical UUIDv7"),
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
            mcp_server_id: McpServerId::parse(row.mcp_server_id.clone())
                .expect("repository MCP IDs must be canonical UUIDv7"),
            name: row.name.clone(),
            status: ConversationMcpStatusKind::Loaded,
            reason: None,
        },
        Err(reason) => ConversationMcpStatus {
            mcp_server_id: McpServerId::parse(row.mcp_server_id.clone())
                .expect("repository MCP IDs must be canonical UUIDv7"),
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
            mcp_server_id: server.mcp_server_id.clone(),
            name: server.name.clone(),
            status: ConversationMcpStatusKind::Unsupported,
            reason: Some(format!("transport '{transport}' is not supported by this agent")),
        };
    }

    match validate_session_transport(&server.transport) {
        Ok(()) => ConversationMcpStatus {
            mcp_server_id: server.mcp_server_id.clone(),
            name: server.name.clone(),
            status: ConversationMcpStatusKind::Loaded,
            reason: None,
        },
        Err(reason) => ConversationMcpStatus {
            mcp_server_id: server.mcp_server_id.clone(),
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

/// Execution preferences and execution identity are typed v3 columns/relations,
/// never open-ended Agent factory data. Rejecting these keys preserves one
/// canonical durable source of truth.
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

/// Backend lifecycle authority never comes from the open `extra` bag.
///
/// These names mirror persisted columns/receipts or private recovery markers.
/// Reject instead of silently stripping them so every caller gets an explicit
/// failure and no partial PATCH can make an injected fence indistinguishable
/// from a backend reservation after restart.
pub(crate) const BACKEND_OWNED_LIFECYCLE_EXTRA_KEYS: [&str; 10] = [
    "_edit_resubmit_fence",
    "active_turn_operation_id",
    "admission_epoch",
    "turn_operation_id",
    "turn_admission_epoch",
    "delivery_operation_id",
    "delivery_receipt_status",
    "execution_attempt_id",
    "execution_step_id",
    "execution_id",
];

fn reject_backend_owned_lifecycle_extra_keys(
    extra: &serde_json::Value,
) -> Result<(), AppError> {
    let Some(object) = extra.as_object() else {
        return Ok(());
    };
    let forbidden = BACKEND_OWNED_LIFECYCLE_EXTRA_KEYS
        .into_iter()
        .find(|key| object.contains_key(*key));

    match forbidden {
        Some(key) => Err(AppError::BadRequest(format!(
            "`extra.{key}` is backend-owned Conversation lifecycle authority"
        ))),
        None => Ok(()),
    }
}

/// The v3 skill snapshot contract has no aliases or cache payload. These keys
/// are rejected at every service write boundary rather than interpreted,
/// persisted, or silently removed.
fn reject_retired_skill_extra_keys(extra: &serde_json::Value) -> Result<(), AppError> {
    let Some(object) = extra.as_object() else {
        return Ok(());
    };
    let retired = [
        "enabled_skills",
        "exclude_builtin_skills",
        "loaded_skills",
    ]
    .into_iter()
    .find(|key| object.contains_key(*key));

    match retired {
        Some(key) => Err(AppError::BadRequest(format!(
            "`extra.{key}` is not part of the v3 conversation skill contract"
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

    let mut extra: serde_json::Value =
        match serde_json::from_str::<serde_json::Value>(&row.extra) {
        Ok(extra) if extra.is_object() => extra,
        Ok(_) => {
            warn!(conversation_id, "Refusing to persist session key: conversation extra is not an object");
            return;
        }
        Err(error) => {
            warn!(
                conversation_id,
                error = %ErrorChain(&error),
                "Refusing to persist session key over invalid conversation extra JSON"
            );
            return;
        }
    };

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
/// currently loaded in the client. The UUIDv7 message ID contains no `:`, so
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

fn attach_unbound_workspace_authority(
    runtime_options: &mut AgentRuntimeBuildOptions,
    workspace: &Path,
) -> Result<String, AppError> {
    let conversation_id = runtime_options.conversation_id.clone();
    runtime_options.workspace_binding_lease = Some(
        nomifun_knowledge::WorkspaceBindingLease::acquire_unbound(
            workspace,
            conversation_id.clone(),
        )?,
    );
    if let Some(extra) = runtime_options.extra.as_object_mut() {
        for key in [
            "knowledge_mounts",
            "knowledge_binding_signature",
            "knowledge_mounts_signature",
            "knowledge_writeback",
            "knowledge_writeback_mode",
            "knowledge_writeback_eagerness",
            "knowledge_channel_write_enabled",
        ] {
            extra.remove(key);
        }
    }
    Ok("kb-runtime-v1:unbound".to_owned())
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
            preset_id: response
                .preset_id
                .as_deref()
                .unwrap_or_else(|| s(extra, "preset_id")),
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
            conversation_id = %response.conversation_id,
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
            conversation_id = %response.conversation_id,
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
) -> Result<Option<Vec<String>>, AppError> {
    let Some(value) = obj.remove("selected_mcp_server_ids") else {
        return Ok(None);
    };
    let serde_json::Value::Array(items) = value else {
        return Err(AppError::BadRequest(
            "selected_mcp_server_ids must be an array of canonical lowercase UUIDv7 MCP server IDs".into(),
        ));
    };
    let ids = items
        .into_iter()
        .enumerate()
        .map(|(index, value)| match value {
            serde_json::Value::String(id) => McpServerId::parse(id)
                .map(McpServerId::into_string)
                .map_err(|error| {
                    AppError::BadRequest(format!(
                        "selected_mcp_server_ids[{index}] must be a canonical lowercase UUIDv7: {error}"
                    ))
                }),
            _ => Err(AppError::BadRequest(format!(
                "selected_mcp_server_ids[{index}] must be a UUIDv7 string"
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PROVIDER_ID_1: &str = "0190f5fe-7c00-7a00-8000-000000000001";
    const PROVIDER_ID_2: &str = "0190f5fe-7c00-7a00-8000-000000000002";
    const RUNTIME_PRESET_ID: &str = "0190f5fe-7c00-7a00-8000-000000000003";

    fn runtime_preset_snapshot() -> ResolvedPresetSnapshot {
        ResolvedPresetSnapshot {
            preset_id: RUNTIME_PRESET_ID.to_owned(),
            preset_revision: 4,
            preset_name: "文案版".to_owned(),
            target: nomifun_api_types::PresetTarget::Conversation,
            routing_description: None,
            instructions: "直接输出走心治愈的短视频文案。".to_owned(),
            resolved_agent_id: None,
            resolved_agent_type: Some("nomi".to_owned()),
            resolved_agent_backend: None,
            resolved_model: None,
            included_skills: Vec::new(),
            excluded_auto_skills: Vec::new(),
            knowledge_policy: Default::default(),
            knowledge_base_ids: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn row_with_runtime_preset(extra: serde_json::Value) -> ConversationRow {
        let snapshot = runtime_preset_snapshot();
        ConversationRow {
            id: 1,
            conversation_id: "0190f5fe-7c00-7a00-8000-000000000004".to_owned(),
            user_id: "0190f5fe-7c00-7a00-8000-000000000005".to_owned(),
            name: "preset-runtime-test".to_owned(),
            r#type: "nomi".to_owned(),
            extra: serde_json::to_string(&extra).unwrap(),
            delegation_policy: "disabled".to_owned(),
            execution_model_pool: None,
            decision_policy: "agent_decides".to_owned(),
            execution_template_id: None,
            model: None,
            status: Some("pending".to_owned()),
            source: Some("nomifun".to_owned()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            cron_job_id: None,
            preset_id: Some(snapshot.preset_id.clone()),
            preset_revision: Some(snapshot.preset_revision),
            preset_snapshot: Some(serde_json::to_string(&snapshot).unwrap()),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn frozen_snapshot_overwrites_tampered_nomi_adapter_prompt() {
        let row = row_with_runtime_preset(json!({
            "preset_rules": "tampered",
            "preset_context": "stale",
            "workspace": "/tmp/test"
        }));
        let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();

        project_preset_runtime_context(&row, &AgentType::Nomi, &mut extra).unwrap();

        let prompt = extra["preset_rules"].as_str().unwrap();
        assert!(prompt.contains("Name: 文案版"));
        assert!(prompt.contains("Revision: 4"));
        assert!(prompt.contains("直接输出走心治愈的短视频文案。"));
        assert_ne!(prompt, "tampered");
        assert!(extra.get("preset_context").is_none());
        assert_eq!(extra["workspace"], "/tmp/test");
    }

    #[test]
    fn frozen_snapshot_projects_to_non_nomi_runtime_context() {
        let row = row_with_runtime_preset(json!({}));
        let mut extra = json!({});

        project_preset_runtime_context(&row, &AgentType::Acp, &mut extra).unwrap();

        assert!(extra["preset_context"]
            .as_str()
            .unwrap()
            .contains("Name: 文案版"));
        assert!(extra.get("preset_rules").is_none());
    }

    #[test]
    fn incomplete_or_mismatched_preset_lineage_fails_closed() {
        let mut incomplete = row_with_runtime_preset(json!({}));
        incomplete.preset_snapshot = None;
        let mut extra = json!({});
        assert!(project_preset_runtime_context(&incomplete, &AgentType::Nomi, &mut extra).is_err());

        let mut mismatch = row_with_runtime_preset(json!({}));
        mismatch.preset_revision = Some(5);
        assert!(project_preset_runtime_context(&mismatch, &AgentType::Nomi, &mut extra).is_err());
    }

    #[test]
    fn non_snapshot_execution_persona_is_not_erased() {
        let mut row = row_with_runtime_preset(json!({"preset_rules": "execution persona"}));
        row.preset_id = None;
        row.preset_revision = None;
        row.preset_snapshot = None;
        let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();

        project_preset_runtime_context(&row, &AgentType::Nomi, &mut extra).unwrap();

        assert_eq!(extra["preset_rules"], "execution persona");
    }

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

    fn model_ref(provider_id: &str, model: &str) -> ExecutionModelRef {
        ExecutionModelRef {
            provider_id: provider_id.to_owned(),
            model: model.to_owned(),
        }
    }

    fn provider_model(provider_id: &str, model: &str) -> ProviderWithModel {
        ProviderWithModel {
            provider_id: provider_id.to_owned(),
            model: model.to_owned(),
            use_model: Some(model.to_owned()),
        }
    }

    #[test]
    fn preset_model_reconciliation_replaces_stale_single_lead() {
        let requested = provider_model(PROVIDER_ID_1, "model-1");
        let resolved = model_ref(PROVIDER_ID_2, "model-2");

        let reconciled = reconcile_preset_conversation_model_pool(
            Some(ExecutionModelPool::Single {
                model: model_ref(PROVIDER_ID_1, "model-1"),
            }),
            Some(&requested),
            &resolved,
        )
        .unwrap();

        assert_eq!(
            reconciled,
            Some(ExecutionModelPool::Single { model: resolved })
        );
    }

    #[test]
    fn preset_model_reconciliation_replaces_range_lead_and_preserves_collaborators() {
        let requested = provider_model(PROVIDER_ID_1, "model-1");
        let resolved = model_ref(PROVIDER_ID_2, "model-2");
        let collaborator = model_ref(PROVIDER_ID_2, "collaborator");

        let reconciled = reconcile_preset_conversation_model_pool(
            Some(ExecutionModelPool::Range {
                models: vec![
                    model_ref(PROVIDER_ID_1, "model-1"),
                    collaborator.clone(),
                ],
            }),
            Some(&requested),
            &resolved,
        )
        .unwrap();

        assert_eq!(
            reconciled,
            Some(ExecutionModelPool::Range {
                models: vec![resolved, collaborator],
            })
        );
    }

    #[test]
    fn preset_model_reconciliation_moves_existing_resolved_lead_without_duplication() {
        let requested = provider_model(PROVIDER_ID_1, "model-1");
        let resolved = model_ref(PROVIDER_ID_2, "model-2");
        let collaborator = model_ref(PROVIDER_ID_2, "collaborator");

        let reconciled = reconcile_preset_conversation_model_pool(
            Some(ExecutionModelPool::Range {
                models: vec![
                    model_ref(PROVIDER_ID_1, "model-1"),
                    collaborator.clone(),
                    resolved.clone(),
                ],
            }),
            Some(&requested),
            &resolved,
        )
        .unwrap();

        assert_eq!(
            reconciled,
            Some(ExecutionModelPool::Range {
                models: vec![resolved, collaborator],
            })
        );
    }

    #[test]
    fn preset_model_reconciliation_preserves_inherited_and_automatic_authority() {
        let requested = provider_model(PROVIDER_ID_1, "model-1");
        let resolved = model_ref(PROVIDER_ID_2, "model-2");

        assert_eq!(
            reconcile_preset_conversation_model_pool(None, Some(&requested), &resolved).unwrap(),
            None
        );
        assert_eq!(
            reconcile_preset_conversation_model_pool(
                Some(ExecutionModelPool::Automatic),
                Some(&requested),
                &resolved,
            )
            .unwrap(),
            Some(ExecutionModelPool::Automatic)
        );
    }

    #[test]
    fn preset_model_reconciliation_is_a_noop_when_the_lead_is_already_resolved() {
        let requested = provider_model(PROVIDER_ID_2, "model-2");
        let resolved = model_ref(PROVIDER_ID_2, "model-2");
        let pool = ExecutionModelPool::Range {
            models: vec![
                resolved.clone(),
                model_ref(PROVIDER_ID_2, "collaborator"),
            ],
        };

        assert_eq!(
            reconcile_preset_conversation_model_pool(
                Some(pool.clone()),
                Some(&requested),
                &resolved,
            )
            .unwrap(),
            Some(pool)
        );
    }

    #[test]
    fn preset_model_reconciliation_rejects_an_invalid_original_authority() {
        let requested = provider_model(PROVIDER_ID_1, "model-1");
        let resolved = model_ref(PROVIDER_ID_2, "model-2");

        assert!(matches!(
            reconcile_preset_conversation_model_pool(
                Some(ExecutionModelPool::Single {
                    model: model_ref(PROVIDER_ID_2, "unrelated"),
                }),
                Some(&requested),
                &resolved,
            ),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn preset_model_reconciliation_rejects_an_invalid_requested_lead_without_a_pool() {
        let mut requested = provider_model(PROVIDER_ID_1, "model-1");
        requested.use_model = Some(" ".to_owned());

        assert!(matches!(
            reconcile_preset_conversation_model_pool(
                None,
                Some(&requested),
                &model_ref(PROVIDER_ID_2, "model-2"),
            ),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn preset_model_reconciliation_requires_a_finite_pool_to_authorize_an_implicit_lead() {
        let resolved = model_ref(PROVIDER_ID_2, "model-2");

        assert!(matches!(
            reconcile_preset_conversation_model_pool(
                Some(ExecutionModelPool::Single {
                    model: model_ref(PROVIDER_ID_1, "model-1"),
                }),
                None,
                &resolved,
            ),
            Err(AppError::BadRequest(_))
        ));
        assert!(matches!(
            reconcile_preset_conversation_model_pool(
                Some(ExecutionModelPool::Range {
                    models: vec![model_ref(PROVIDER_ID_1, "model-1")],
                }),
                None,
                &resolved,
            ),
            Err(AppError::BadRequest(_))
        ));
        assert_eq!(
            reconcile_preset_conversation_model_pool(
                Some(ExecutionModelPool::Automatic),
                None,
                &resolved,
            )
            .unwrap(),
            Some(ExecutionModelPool::Automatic)
        );
    }

    #[test]
    fn parse_selected_mcp_ids_accepts_canonical_uuidv7_array() {
        let first = "0190f5fe-7c00-7a00-8000-000000000123";
        let second = "0190f5fe-7c00-7a00-8000-000000000999";
        let mut obj = json!({ "selected_mcp_server_ids": [first, second] })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            parse_selected_mcp_server_ids(&mut obj).unwrap(),
            Some(vec![first.to_owned(), second.to_owned()])
        );
    }

    #[test]
    fn parse_selected_mcp_ids_rejects_legacy_and_non_canonical_values() {
        for value in [
            json!([4]),
            json!(["4"]),
            json!(["550e8400-e29b-41d4-a716-446655440000"]),
            json!(["0190F5FE-7C00-7A00-8000-000000000123"]),
            json!(["mcp_0190f5fe-7c00-7a00-8000-000000000123"]),
            json!("not-an-array"),
        ] {
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
        let companion_id = "0190f5fe-7c00-7a00-8abc-012345678901";
        let conversation_id = "0190f5fe-7c00-7a00-8abc-012345678902";
        let extra = json!({"companion_id": companion_id});
        assert_eq!(
            knowledge_binding_target(&extra, conversation_id).unwrap(),
            ("companion", companion_id)
        );
    }

    #[test]
    fn knowledge_binding_target_rejects_untrimmed_companion_id() {
        let extra = json!({"companion_id": "  0190f5fe-7c00-7a00-8abc-012345678901  "});
        assert!(knowledge_binding_target(
            &extra,
            "0190f5fe-7c00-7a00-8abc-012345678902"
        )
        .is_err());
    }

    #[test]
    fn knowledge_binding_target_rejects_empty_companion_id() {
        let extra = json!({"companion_id": ""});
        assert!(knowledge_binding_target(
            &extra,
            "0190f5fe-7c00-7a00-8abc-012345678902"
        )
        .is_err());
    }

    #[test]
    fn knowledge_binding_target_rejects_blank_companion_id() {
        let extra = json!({"companion_id": "   \t "});
        assert!(knowledge_binding_target(
            &extra,
            "0190f5fe-7c00-7a00-8abc-012345678902"
        )
        .is_err());
    }

    #[test]
    fn knowledge_binding_target_missing_companion_id_falls_back() {
        let extra = json!({"workspace": "/tmp/ws"});
        let conversation_id = "0190f5fe-7c00-7a00-8abc-012345678902";
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
        let conversation_id = "0190f5fe-7c00-7a00-8abc-012345678902";
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
            "0190f5fe-7c00-7a00-8abc-012345678902"
        )
        .is_err());
    }

    fn response_with_type(agent_type: nomifun_common::AgentType) -> ConversationResponse {
        ConversationResponse {
            conversation_id: nomifun_common::ConversationId::new().into_string(),
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
            "agent_id": "0190f5fe-7c00-7a00-8000-000000000101",
            "agent_name": "Claude Code",
            "backend": "claude",
            "current_model_id": "opus",
            "session_mode": "default",
        });
        let lineage = PresetLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "acp");
        assert_eq!(
            lineage.agent_id,
            "0190f5fe-7c00-7a00-8000-000000000101"
        );
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
                mcp_server_id: McpServerId::new(),
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
                mcp_server_id: McpServerId::new(),
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
